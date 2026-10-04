//! krowk's own streaming client for the Anthropic Messages API — no SDK and
//! no multi-provider crate, because those flatten away exactly what this
//! keeps: thinking signatures, redacted thinking, and where the cache
//! breakpoints go (R-PROV-1).
//!
//! Prompt caching is on, and placed rather than hoped for (R-PROV-3). The
//! prompt renders tools → system → messages, and a breakpoint caches
//! everything before it, so there are three:
//!
//! 1. on the system block — tools and system, identical for the whole
//!    session;
//! 2. on the last block of the last message — this call's whole prefix,
//!    which the next call in the tool loop, or the next turn, reads back;
//! 3. on the last block of the user message before that — where the previous
//!    call put its breakpoint 2, so its cache entry is an explicit read
//!    point even when a turn appends more than the API's lookback reaches.
//!
//! The history is only ever appended to: nothing earlier is re-rendered
//! differently from one call to the next, which is what keeps both the
//! cache and replayed thinking valid.

pub mod stream;
/// The SSE parser every wire client shares, where it has always been found.
pub use crate::sse;

use crate::engine::{BoxFuture, EngineError, Events, HistoryItem};
use crate::http::{self, Answer, Peer};
use crate::instances::{Resolved, Thinking};
use crate::native::{self, ModelClient, ModelRequest, ModelResponse};
use crate::protocol::{Effort, Item, ProviderBlob, WireApi};
use serde_json::{json, Map, Value};
use tokio::sync::watch;

pub const API_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    http: reqwest::Client,
    instance: Resolved,
    user_agent: String,
}

impl AnthropicClient {
    pub fn new(instance: Resolved, krowk_version: &str) -> Result<AnthropicClient, EngineError> {
        Ok(AnthropicClient { http: http::client()?, instance, user_agent: format!("krowk/{krowk_version}") })
    }
}

/// The request body for one call. Public for the fixture tests, which pin
/// its shape: replayed thinking and the cache breakpoints above all.
pub fn request_body(req: &ModelRequest, instance: &Resolved) -> Value {
    let ephemeral = || json!({ "type": "ephemeral" });
    let mut messages = messages(&req.history, &req.images);
    let user_turns: Vec<usize> = messages.iter().enumerate().filter(|(_, m)| m["role"] == "user").map(|(i, _)| i).collect();
    for &i in user_turns.iter().rev().take(2) {
        if let Some(last) = messages[i]["content"].as_array_mut().and_then(|c| c.last_mut()) {
            last["cache_control"] = ephemeral();
        }
    }
    let tools: Vec<Value> = req.tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema })).collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("max_tokens".into(), json!(instance.max_tokens));
    body.insert("stream".into(), json!(true));
    body.insert("system".into(), json!([{ "type": "text", "text": req.system, "cache_control": ephemeral() }]));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    // `none` on the ladder is thinking off; any other rung is the
    // Messages API's own effort, which it takes by the same names.
    let thinking = instance.thinking == Thinking::Adaptive && req.effort != Some(Effort::None);
    if thinking {
        body.insert("thinking".into(), json!({ "type": "adaptive" }));
    }
    if let Some(e) = req.effort.filter(|e| *e != Effort::None) {
        body.insert("output_config".into(), json!({ "effort": e.name() }));
    }
    body.insert("messages".into(), Value::Array(messages));
    Value::Object(body)
}

/// The branch as Messages-API messages. Items of one response are one
/// assistant message; everything a person or a tool produced between them
/// is one user message, tool results first, as the API requires.
fn messages(history: &[HistoryItem], images: &crate::images::Loaded) -> Vec<Value> {
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    let push = |out: &mut Vec<(String, Vec<Value>)>, role: &str, block: Value| match out.last_mut() {
        Some((r, blocks)) if r == role => blocks.push(block),
        _ => out.push((role.into(), vec![block])),
    };
    let mut shown = std::collections::HashSet::new();
    for h in history {
        match &h.item {
            Item::UserText { text, images: refs } => {
                push(&mut out, "user", json!({ "type": "text", "text": text }));
                for (r, label, data) in crate::images::sent(refs, images, &mut shown) {
                    push(&mut out, "user", json!({ "type": "text", "text": label }));
                    if let Some(data) = data {
                        push(&mut out, "user", json!({ "type": "image", "source": { "type": "base64", "media_type": r.media_type, "data": data } }));
                    }
                }
            }
            Item::ToolResult { call_id, output, is_error } => {
                let block = json!({ "type": "tool_result", "tool_use_id": call_id, "content": output, "is_error": is_error });
                // Results lead their message: insert before any text already there.
                match out.last_mut() {
                    Some((r, blocks)) if r == "user" => {
                        let at = blocks.iter().take_while(|b| b["type"] == "tool_result").count();
                        blocks.insert(at, block);
                    }
                    _ => out.push(("user".into(), vec![block])),
                }
            }
            Item::AssistantText { text } if !text.is_empty() => push(&mut out, "assistant", json!({ "type": "text", "text": text })),
            Item::AssistantText { .. } => {}
            Item::ToolCall { call_id, name, input } => push(&mut out, "assistant", json!({ "type": "tool_use", "id": call_id, "name": name, "input": input })),
            // Replayed only when the blob itself says it is ours — never
            // by where the item sits, which a response that failed before
            // completing does not record. Another provider's reasoning is
            // downgraded to framed plain text (R-SWITCH-1) — with a blob of
            // its own, or none at all, as a backend that keeps its reasoning
            // to itself (Codex) leaves it: this API's own thinking always
            // carries its signature, and is never completed without one.
            Item::Reasoning { text, blob } => match native::replays(blob, stream::PROVIDER, WireApi::AnthropicMessages) {
                Some(b) => {
                    if let Some(block) = replay(b, text) {
                        push(&mut out, "assistant", block);
                    }
                }
                None => {
                    if let Some(t) = native::downgraded(text) {
                        push(&mut out, "assistant", json!({ "type": "text", "text": t }));
                    }
                }
            },
        }
    }
    close_dangling_calls(&mut out);
    out.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect()
}

/// A thinking block rebuilt from its blob, the signature untouched.
fn replay(blob: &ProviderBlob, text: &str) -> Option<Value> {
    match blob.data.get("type")?.as_str()? {
        "thinking" => Some(json!({ "type": "thinking", "thinking": text, "signature": blob.data.get("signature")? })),
        "redacted_thinking" => Some(json!({ "type": "redacted_thinking", "data": blob.data.get("data")? })),
        _ => None,
    }
}

/// Every tool_use needs its tool_result in the very next message. A turn
/// interrupted between the two leaves a call with none, which is answered
/// here rather than sent back unanswered.
fn close_dangling_calls(out: &mut Vec<(String, Vec<Value>)>) {
    let mut i = 0;
    while i < out.len() {
        if out[i].0 == "assistant" {
            let calls: Vec<String> = out[i].1.iter().filter(|b| b["type"] == "tool_use").filter_map(|b| b["id"].as_str().map(String::from)).collect();
            if !calls.is_empty() {
                if out.get(i + 1).is_none_or(|(r, _)| r != "user") {
                    out.insert(i + 1, ("user".into(), Vec::new()));
                }
                let next = &mut out[i + 1].1;
                let missing: Vec<&String> = calls.iter().filter(|id| !next.iter().any(|b| b["tool_use_id"] == id.as_str())).collect();
                for (n, id) in missing.into_iter().enumerate() {
                    next.insert(n, json!({ "type": "tool_result", "tool_use_id": id, "content": "no result was recorded: the turn stopped first", "is_error": true }));
                }
            }
        }
        i += 1;
    }
    out.retain(|(_, blocks)| !blocks.is_empty());
}

impl ModelClient for AnthropicClient {
    fn provider(&self) -> &str {
        stream::PROVIDER
    }

    fn wire_api(&self) -> WireApi {
        WireApi::AnthropicMessages
    }

    fn stream<'a>(&'a self, req: &'a ModelRequest, events: &'a Events, cancel: watch::Receiver<bool>) -> BoxFuture<'a, Result<ModelResponse, EngineError>> {
        Box::pin(async move {
            let inst = &self.instance;
            if let Some(fix) = inst.missing_key() {
                return Err(EngineError::new("not_authenticated", fix));
            }
            let body = request_body(req, inst).to_string();
            let url = format!("{}/v1/messages", inst.base_url);
            let peer = Peer { vendor: "Anthropic", instance: &inst.name, base_url: &inst.base_url };
            let build = || {
                self.http
                    .post(&url)
                    .header("x-api-key", &inst.api_key)
                    .header("anthropic-version", API_VERSION)
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream")
                    .header("user-agent", &self.user_agent)
                    .body(body.clone())
            };
            match http::send(&build, &cancel, peer).await? {
                Answer::Streaming(resp) => http::read_stream(resp, stream::Decoder::default(), events, &cancel, &req.model, peer).await,
                Answer::Refused(resp) => {
                    Err(http::refused(resp, peer, &req.model, &crate::keys::auth_fix(inst)).await)
                }
                Answer::Interrupted => Ok(ModelResponse { model: req.model.clone(), interrupted: true, ..ModelResponse::default() }),
            }
        })
    }
}
