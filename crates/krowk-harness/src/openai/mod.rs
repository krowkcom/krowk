//! krowk's own streaming client for OpenAI's Responses API (R-PROV-1): no
//! SDK, and nothing between the stream and krowk's items that could flatten
//! away the encrypted reasoning or the freeform tools.
//!
//! **Stateless.** Every request is `store: false` and carries the whole
//! branch as input items: krowk's log is the conversation, never a response
//! id on OpenAI's side that could expire or be deleted. Reasoning comes back
//! encrypted (`include: ["reasoning.encrypted_content"]`), rides in the
//! item's blob exactly as it arrived, and goes back on the next call
//! unmodified — only to OpenAI on this wire API (R-LOG-3).
//!
//! **Caching** (R-PROV-3). OpenAI caches a prompt's prefix by itself; what
//! makes it hit is a prefix that does not move and a key that routes every
//! call of the session to the same cache. So the input is only ever
//! appended to — instructions, tools and every earlier item render the same
//! bytes on every call — and `prompt_cache_key` is the session id.
//!
//! **Freeform tools.** A tool definition with a grammar (`apply_patch`) is
//! sent as a `custom` tool with a Lark grammar, for the models OpenAI
//! trained on them; its call arrives as `custom_tool_call` with the patch as
//! plain text, and is answered with `custom_tool_call_output`.

pub mod stream;

use crate::engine::{BoxFuture, EngineError, Events, HistoryItem};
use crate::http::{self, Answer, Peer};
use crate::instances::Resolved;
use crate::native::{self, ModelClient, ModelRequest, ModelResponse};
use crate::protocol::{Item, WireApi};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use tokio::sync::watch;

pub const PROVIDER: &str = "openai";

pub struct ResponsesClient {
    http: reqwest::Client,
    instance: Resolved,
    user_agent: String,
}

impl ResponsesClient {
    pub fn new(instance: Resolved, krowk_version: &str) -> Result<ResponsesClient, EngineError> {
        Ok(ResponsesClient { http: http::client()?, instance, user_agent: format!("krowk/{krowk_version}") })
    }
}

/// The request body for one call. Public for the fixture tests, which pin
/// its shape: statelessness, the cache key, freeform tools and replayed
/// reasoning above all. `provider` is whose reasoning blobs replay here.
pub fn request_body(req: &ModelRequest, provider: &str) -> Value {
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| match &t.grammar {
            Some(g) => json!({
                "type": "custom",
                "name": t.name,
                "description": t.description,
                "format": { "type": "grammar", "syntax": g.syntax, "definition": g.definition },
            }),
            None => json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.input_schema, "strict": false }),
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("instructions".into(), json!(req.system));
    body.insert("input".into(), Value::Array(input(&req.history, provider, &req.images)));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!("auto"));
        body.insert("parallel_tool_calls".into(), json!(true));
    }
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    if req.reasoning {
        // A model that does not reason refuses the include; one that does
        // sends nothing krowk could replay without it.
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        let mut reasoning = Map::new();
        if let Some(e) = req.effort {
            reasoning.insert("effort".into(), json!(e.name()));
        }
        reasoning.insert("summary".into(), json!("auto"));
        body.insert("reasoning".into(), Value::Object(reasoning));
    }
    if !req.session_id.is_empty() {
        body.insert("prompt_cache_key".into(), json!(req.session_id));
    }
    Value::Object(body)
}

/// The branch as input items, in order. Every call is answered — a call
/// the turn stopped before answering gets a result saying so — because the
/// API refuses a call sent back without its output.
fn input(history: &[HistoryItem], provider: &str, images: &crate::images::Loaded) -> Vec<Value> {
    let custom: HashSet<&str> = history
        .iter()
        .filter_map(|h| match &h.item {
            Item::ToolCall { call_id, input: Value::String(_), .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let answered: HashSet<&str> = history
        .iter()
        .filter_map(|h| match &h.item {
            Item::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    let mut open: Vec<(String, bool)> = Vec::new();
    let close = |out: &mut Vec<Value>, open: &mut Vec<(String, bool)>| {
        for (call_id, is_custom) in open.drain(..) {
            out.push(call_output(&call_id, is_custom, "no result was recorded: the turn stopped first"));
        }
    };
    for h in history {
        match &h.item {
            Item::UserText { text, images: refs } => {
                close(&mut out, &mut open);
                let mut content = vec![json!({ "type": "input_text", "text": text })];
                for (r, label, data) in crate::images::sent(refs, images) {
                    content.push(json!({ "type": "input_text", "text": label }));
                    if let Some(data) = data {
                        content.push(json!({ "type": "input_image", "image_url": format!("data:{};base64,{data}", r.media_type) }));
                    }
                }
                out.push(json!({ "type": "message", "role": "user", "content": content }));
            }
            Item::AssistantText { text } if !text.is_empty() => {
                out.push(json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] }));
            }
            Item::AssistantText { .. } => {}
            Item::Reasoning { text, blob } => match native::replays(blob, provider, WireApi::OpenaiResponses) {
                Some(b) => {
                    if let Some(item) = replay(&b.data) {
                        out.push(item);
                    }
                }
                None => {
                    if let Some(t) = native::downgraded(text) {
                        out.push(json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": t }] }));
                    }
                }
            },
            Item::ToolCall { call_id, name, input } => {
                match input {
                    Value::String(patch) => out.push(json!({ "type": "custom_tool_call", "call_id": call_id, "name": name, "input": patch })),
                    // The arguments are the JSON text of the input, rendered
                    // the same way every time, so the prefix never moves.
                    v => out.push(json!({ "type": "function_call", "call_id": call_id, "name": name, "arguments": v.to_string() })),
                }
                if !answered.contains(call_id.as_str()) {
                    open.push((call_id.clone(), matches!(input, Value::String(_))));
                }
            }
            Item::ToolResult { call_id, output, is_error: _ } => {
                out.push(call_output(call_id, custom.contains(call_id.as_str()), output));
            }
        }
    }
    close(&mut out, &mut open);
    out
}

fn call_output(call_id: &str, is_custom: bool, output: &str) -> Value {
    let kind = if is_custom { "custom_tool_call_output" } else { "function_call_output" };
    json!({ "type": kind, "call_id": call_id, "output": output })
}

/// A reasoning item rebuilt from its blob: the item as the stream sent it,
/// `encrypted_content` and summary untouched. Only the item's `id` is left
/// off — with `store: false` OpenAI keeps nothing under that id, and an id
/// it cannot find is refused — and `status`, which is output-only.
fn replay(data: &Value) -> Option<Value> {
    let mut item = data.as_object()?.clone();
    if item.get("type")?.as_str()? != "reasoning" || item.get("encrypted_content").is_none_or(Value::is_null) {
        return None;
    }
    item.remove("id");
    item.remove("status");
    Some(Value::Object(item))
}

impl ModelClient for ResponsesClient {
    fn provider(&self) -> &str {
        &self.instance.provider
    }

    fn wire_api(&self) -> WireApi {
        WireApi::OpenaiResponses
    }

    fn custom_tools(&self, model: &str) -> bool {
        crate::toolset::takes_custom_tools(model)
    }

    fn stream<'a>(&'a self, req: &'a ModelRequest, events: &'a Events, cancel: watch::Receiver<bool>) -> BoxFuture<'a, Result<ModelResponse, EngineError>> {
        Box::pin(async move {
            let inst = &self.instance;
            if let Some(fix) = inst.missing_key() {
                return Err(EngineError::new("not_authenticated", fix));
            }
            let body = request_body(req, &inst.provider).to_string();
            let url = format!("{}/responses", inst.base_url);
            let peer = Peer { vendor: inst.vendor, instance: &inst.name, base_url: &inst.base_url };
            let build = || {
                let mut r = self
                    .http
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream")
                    .header("user-agent", &self.user_agent)
                    .body(body.clone());
                if !inst.api_key.is_empty() {
                    r = r.bearer_auth(&inst.api_key);
                }
                r
            };
            match http::send(&build, &cancel, peer).await? {
                Answer::Streaming(resp) => http::read_stream(resp, stream::Decoder::new(&inst.provider), events, &cancel, &req.model, peer).await,
                Answer::Refused(resp) => {
                    Err(http::refused(resp, peer, &req.model, &crate::keys::auth_fix(inst)).await)
                }
                Answer::Interrupted => Ok(ModelResponse { model: req.model.clone(), interrupted: true, ..ModelResponse::default() }),
            }
        })
    }
}
