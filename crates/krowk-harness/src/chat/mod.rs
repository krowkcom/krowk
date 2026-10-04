//! krowk's own streaming client for Chat Completions (R-PROV-1): xAI,
//! OpenRouter and any server that speaks it at a base URL.
//!
//! Chat Completions has no standard field for reasoning, so each vendor
//! sends its own — `reasoning_content` (xAI, DeepSeek and most open-model
//! servers), `reasoning` (OpenRouter's text) and `reasoning_details`
//! (OpenRouter's structured form, which carries the signatures and
//! encrypted blocks of the model behind it). Whichever arrive are kept in
//! the reasoning item's blob under the vendor's own names and sent back on
//! the assistant message they came with, as they came — only to the same
//! provider on this wire API (R-LOG-3).
//!
//! Caching (R-PROV-3) is the provider's, and krowk's part is a prefix that
//! never moves: the history is only ever appended to, and renders the same
//! bytes on every call. Where a provider takes a key for it, the session id
//! is sent: xAI's `x-grok-conv-id` routes a conversation's calls to the
//! server holding its cache, and OpenAI's `prompt_cache_key` does the same.

pub mod stream;

use crate::engine::{BoxFuture, EngineError, Events, HistoryItem};
use crate::http::{self, Answer, Peer};
use crate::instances::Resolved;
use crate::native::{self, ModelClient, ModelRequest, ModelResponse};
use crate::oauth;
use crate::protocol::{Item, WireApi};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::watch;

/// What a call is authorized with.
pub enum Credential {
    /// A key from the environment, sent as a bearer token.
    Key(String),
    /// Nothing: a local server that wants no key.
    None,
    /// An OAuth login's access token, refreshed as it expires.
    OAuth(Arc<oauth::Tokens>),
}

pub struct ChatClient {
    http: reqwest::Client,
    instance: Resolved,
    credential: Credential,
    user_agent: String,
}

impl ChatClient {
    pub fn new(instance: Resolved, credential: Credential, krowk_version: &str) -> Result<ChatClient, EngineError> {
        Ok(ChatClient { http: http::client()?, instance, credential, user_agent: format!("krowk/{krowk_version}") })
    }
}

/// The request body for one call. Public for the fixture tests, which pin
/// its shape: replayed vendor reasoning above all. `provider` is whose
/// reasoning blobs replay here.
pub fn request_body(req: &ModelRequest, provider: &str) -> Value {
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } }))
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages(&req.system, &req.history, provider, &req.images)));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!("auto"));
    }
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if let Some(e) = req.effort {
        // OpenRouter normalizes effort under `reasoning`; everyone else who
        // takes one reads OpenAI's `reasoning_effort`.
        if provider == "openrouter" {
            body.insert("reasoning".into(), json!({ "effort": e.name() }));
        } else {
            body.insert("reasoning_effort".into(), json!(e.name()));
        }
    }
    if provider == "openai" && !req.session_id.is_empty() {
        body.insert("prompt_cache_key".into(), json!(req.session_id));
    }
    Value::Object(body)
}

/// One assistant message as it is built: the items of one response.
#[derive(Default)]
struct Assistant {
    response: Option<usize>,
    text: String,
    calls: Vec<Value>,
    call_ids: Vec<String>,
    vendor: Map<String, Value>,
}

/// The branch as Chat Completions messages: the system prompt, then one
/// message per user prompt, per response (its text, tool calls and vendor
/// reasoning together) and per tool result. Every call is answered.
fn messages(system: &str, history: &[HistoryItem], provider: &str, images: &crate::images::Loaded) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    let answered: HashSet<&str> = history
        .iter()
        .filter_map(|h| match &h.item {
            Item::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let mut open: Option<Assistant> = None;
    let mut unanswered: Vec<String> = Vec::new();
    let flush = |out: &mut Vec<Value>, open: &mut Option<Assistant>, unanswered: &mut Vec<String>| {
        if let Some(a) = open.take() {
            let mut m = Map::new();
            m.insert("role".into(), json!("assistant"));
            m.insert("content".into(), if a.text.is_empty() && !a.calls.is_empty() { Value::Null } else { json!(a.text) });
            if !a.calls.is_empty() {
                m.insert("tool_calls".into(), Value::Array(a.calls));
            }
            m.extend(a.vendor);
            out.push(Value::Object(m));
            unanswered.extend(a.call_ids.into_iter().filter(|id| !answered.contains(id.as_str())));
        }
    };
    let close_calls = |out: &mut Vec<Value>, unanswered: &mut Vec<String>| {
        for id in unanswered.drain(..) {
            out.push(json!({ "role": "tool", "tool_call_id": id, "content": "no result was recorded: the turn stopped first" }));
        }
    };
    let mut shown = std::collections::HashSet::new();
    for h in history {
        let assistant = matches!(h.item, Item::AssistantText { .. } | Item::Reasoning { .. } | Item::ToolCall { .. });
        if assistant {
            let same = open.as_ref().is_some_and(|a| a.response == h.response && (h.response.is_some() || a.calls.is_empty()));
            if !same {
                flush(&mut out, &mut open, &mut unanswered);
                close_calls(&mut out, &mut unanswered);
                open = Some(Assistant { response: h.response, ..Assistant::default() });
            }
        }
        match &h.item {
            Item::UserText { text, images: refs } => {
                flush(&mut out, &mut open, &mut unanswered);
                close_calls(&mut out, &mut unanswered);
                // Plain text stays a string, as every compatible server
                // takes; only a prompt with images is sent as parts.
                if refs.is_empty() {
                    out.push(json!({ "role": "user", "content": text }));
                } else {
                    let mut parts = vec![json!({ "type": "text", "text": text })];
                    for (r, label, data) in crate::images::sent(refs, images, &mut shown) {
                        parts.push(json!({ "type": "text", "text": label }));
                        if let Some(data) = data {
                            parts.push(json!({ "type": "image_url", "image_url": { "url": format!("data:{};base64,{data}", r.media_type) } }));
                        }
                    }
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
            Item::ToolResult { call_id, output, .. } => {
                flush(&mut out, &mut open, &mut unanswered);
                out.push(json!({ "role": "tool", "tool_call_id": call_id, "content": output }));
            }
            Item::AssistantText { text } => {
                let a = open.as_mut().expect("opened above");
                if !a.text.is_empty() && !text.is_empty() {
                    a.text.push_str("\n\n");
                }
                a.text.push_str(text);
            }
            Item::Reasoning { text, blob } => {
                let a = open.as_mut().expect("opened above");
                match native::replays(blob, provider, WireApi::ChatCompletions) {
                    Some(b) => {
                        if let Some(fields) = b.data.as_object() {
                            a.vendor.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
                        }
                    }
                    None => {
                        if let Some(t) = native::downgraded(text) {
                            a.text = if a.text.is_empty() { t } else { format!("{t}\n\n{}", a.text) };
                        }
                    }
                }
            }
            Item::ToolCall { call_id, name, input } => {
                let a = open.as_mut().expect("opened above");
                // A freeform input (a patch) has no JSON form here: it goes
                // back as the `{input}` the JSON form of the tool takes.
                let args = match input {
                    Value::String(s) => json!({ "input": s }).to_string(),
                    v => v.to_string(),
                };
                a.calls.push(json!({ "id": call_id, "type": "function", "function": { "name": name, "arguments": args } }));
                a.call_ids.push(call_id.clone());
            }
        }
    }
    flush(&mut out, &mut open, &mut unanswered);
    close_calls(&mut out, &mut unanswered);
    out
}

impl ChatClient {
    /// The token to send (`Some(None)` for a keyless server), or none when
    /// the turn was interrupted while waiting for one.
    async fn token(&self, refresh: bool, cancel: &watch::Receiver<bool>) -> Result<Option<Option<String>>, EngineError> {
        Ok(match &self.credential {
            Credential::Key(k) => Some(Some(k.clone())),
            Credential::None => Some(None),
            Credential::OAuth(t) => t.bearer_unless(&self.http, refresh, cancel.clone()).await?.map(Some),
        })
    }

    fn auth_fix(&self) -> String {
        match &self.credential {
            Credential::OAuth(_) => format!("sign in again with `{}`", oauth::login_command(&self.instance.name)),
            _ => crate::keys::auth_fix(&self.instance),
        }
    }
}

impl ModelClient for ChatClient {
    fn provider(&self) -> &str {
        &self.instance.provider
    }

    fn wire_api(&self) -> WireApi {
        WireApi::ChatCompletions
    }

    fn stream<'a>(&'a self, req: &'a ModelRequest, events: &'a Events, cancel: watch::Receiver<bool>) -> BoxFuture<'a, Result<ModelResponse, EngineError>> {
        Box::pin(async move {
            let inst = &self.instance;
            if let Some(fix) = inst.missing_key() {
                return Err(EngineError::new("not_authenticated", fix));
            }
            let body = request_body(req, &inst.provider).to_string();
            let url = format!("{}/chat/completions", inst.base_url);
            let peer = Peer { vendor: inst.vendor, instance: &inst.name, base_url: &inst.base_url };
            // An OAuth token the server refuses is refreshed once, whatever
            // its expiry said: the server's clock and ours may disagree.
            for refresh in [false, true] {
                // Getting a token can wait on another krowk's refresh: an
                // interrupt ends that wait. A refresh already sent finishes
                // and is saved (see Tokens::bearer_unless), and the call
                // then stops before it is made.
                let interrupted = || Ok(ModelResponse { model: req.model.clone(), interrupted: true, ..ModelResponse::default() });
                let Some(token) = self.token(refresh, &cancel).await? else { return interrupted() };
                if *cancel.borrow() {
                    return interrupted();
                }
                let build = || {
                    let mut r = self
                        .http
                        .post(&url)
                        .header("content-type", "application/json")
                        .header("accept", "text/event-stream")
                        .header("user-agent", &self.user_agent)
                        .body(body.clone());
                    if let Some(t) = &token {
                        r = r.bearer_auth(t);
                    }
                    if inst.provider == "xai" && !req.session_id.is_empty() {
                        r = r.header("x-grok-conv-id", &req.session_id);
                    }
                    r
                };
                match http::send(&build, &cancel, peer).await? {
                    Answer::Streaming(resp) => return http::read_stream(resp, stream::Decoder::new(&inst.provider), events, &cancel, &req.model, peer).await,
                    Answer::Refused(resp) if resp.status().as_u16() == 401 && !refresh && matches!(self.credential, Credential::OAuth(_)) => continue,
                    Answer::Refused(resp) => {
                        return Err(http::refused(resp, peer, &req.model, &self.auth_fix()).await);
                    }
                    Answer::Interrupted => return Ok(ModelResponse { model: req.model.clone(), interrupted: true, ..ModelResponse::default() }),
                }
            }
            unreachable!("the second attempt returns")
        })
    }
}
