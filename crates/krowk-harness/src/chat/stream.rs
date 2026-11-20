//! A Chat Completions stream, decoded straight into krowk's items
//! (R-PROV-1). The wire has no items, only one growing message: each chunk
//! carries a `delta` of it. krowk opens an item the first time a part of it
//! arrives — reasoning, text, or the tool call at each `tool_calls` index —
//! streams the rest as deltas, and completes them all when the stream says
//! `[DONE]` (or ends after a `finish_reason`, for servers that never send it).

use crate::engine::{EngineError, EngineEvent};
use crate::http::clip;
use crate::wire::Decode;
use crate::native::ModelResponse;
use crate::protocol::{Delta, Item, ItemKind, ProviderBlob, Usage, WireApi};
use crate::sse::SseEvent;
use serde_json::{json, Map, Value};

struct Call {
    /// The `tool_calls` index it streams under.
    index: u64,
    id: String,
    call_id: String,
    name: String,
    args: String,
}

/// The state of one response as its chunks arrive.
pub struct Decoder {
    provider: String,
    reasoning: Option<(String, String)>,
    /// The vendor's reasoning fields, under its own names, as they grew.
    vendor: Map<String, Value>,
    text: Option<(String, String)>,
    /// In the order the model made them. An index names the call being
    /// streamed, not one call forever: a server may reuse index 0 for each
    /// call in turn, and a new `id` there is a new call.
    calls: Vec<Call>,
    pub items: Vec<(String, Item)>,
    pub response_id: Option<String>,
    pub model: String,
    pub usage: Usage,
    pub stop_reason: Option<String>,
    /// `[DONE]` arrived, and the items are complete.
    pub done: bool,
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// OpenRouter streams `reasoning_details` as fragments of an array: one
/// element per chunk, those of the same `index` being pieces of one entry.
/// Pieces of one entry are joined — their text fields end to end, the rest
/// taken from the latest — so what is sent back is the array as a
/// non-streaming answer would have held it.
fn merge_details(into: &mut Vec<Value>, fragments: &[Value]) {
    for f in fragments {
        let same = f.get("index").and_then(|i| into.iter_mut().rev().find(|e| e.get("index") == Some(i)));
        match (same, f.as_object()) {
            (Some(Value::Object(e)), Some(f)) => {
                for (k, v) in f {
                    match (e.get_mut(k), v) {
                        (Some(Value::String(a)), Value::String(b)) if matches!(k.as_str(), "text" | "summary" | "data") => a.push_str(b),
                        _ => {
                            e.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            _ => into.push(f.clone()),
        }
    }
}

impl Decoder {
    /// A decoder whose reasoning blobs name `provider`.
    pub fn new(provider: &str) -> Decoder {
        Decoder {
            provider: provider.into(),
            reasoning: None,
            vendor: Map::new(),
            text: None,
            calls: Vec::new(),
            items: Vec::new(),
            response_id: None,
            model: String::new(),
            usage: Usage::default(),
            stop_reason: None,
            done: false,
        }
    }

    fn take_usage(&mut self, u: &Value) {
        let n = |p: &str| u.pointer(p).and_then(Value::as_i64).unwrap_or(0).max(0);
        let (prompt, cached) = (n("/prompt_tokens"), n("/prompt_tokens_details/cached_tokens"));
        let (completion, reasoning) = (n("/completion_tokens"), n("/completion_tokens_details/reasoning_tokens"));
        // OpenAI and OpenRouter count reasoning inside completion tokens;
        // xAI counts it beside them, which its total shows.
        let beside = reasoning > 0 && u.get("total_tokens").and_then(Value::as_i64) == Some(prompt + completion + reasoning);
        self.usage = Usage {
            input_tokens: prompt - cached.min(prompt),
            cache_read_tokens: cached.min(prompt),
            output_tokens: if beside { completion } else { completion - reasoning.min(completion) },
            reasoning_tokens: if beside { reasoning } else { reasoning.min(completion) },
            cache_write_tokens: 0,
        };
    }

    /// Folds one chunk in; returns what the client should be told.
    pub fn apply(&mut self, ev: &SseEvent) -> Result<Vec<EngineEvent>, EngineError> {
        let mut out = Vec::new();
        if ev.data.trim() == "[DONE]" {
            out.extend(self.complete()?);
            self.done = true;
            return Ok(out);
        }
        let v: Value = serde_json::from_str(&ev.data)
            .map_err(|e| EngineError::new("malformed_response", format!("the server sent a stream chunk that is not JSON ({e}): {}", clip(&ev.data, 200))))?;
        if let Some(e) = v.get("error") {
            let message = e.get("message").and_then(Value::as_str).map(String::from).unwrap_or_else(|| e.to_string());
            let status = e.get("code").and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok()).unwrap_or(0);
            let code = match status {
                429 => "rate_limited",
                s if s >= 500 => "provider_unavailable",
                _ => "provider_error",
            };
            return Err(EngineError::new(code, format!("the server stopped the response ({}) — run the prompt again with --resume", clip(&message, 300))).with_status(status));
        }
        if let Some(id) = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            self.response_id = Some(id.into());
        }
        if let Some(m) = v.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            self.model = m.into();
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.take_usage(u);
        }
        let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else { return Ok(out) };
        let d = choice.get("delta").cloned().unwrap_or_default();
        // Reasoning, under whichever name the vendor uses for its text.
        let mut said = String::new();
        for field in ["reasoning_content", "reasoning"] {
            if let Some(t) = d.get(field).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                said.push_str(t);
                match self.vendor.get_mut(field) {
                    Some(Value::String(s)) => s.push_str(t),
                    _ => {
                        self.vendor.insert(field.into(), json!(t));
                    }
                }
            }
        }
        if let Some(details) = d.get("reasoning_details").and_then(Value::as_array).filter(|a| !a.is_empty()) {
            let mut all = match self.vendor.remove("reasoning_details") {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            };
            merge_details(&mut all, details);
            self.vendor.insert("reasoning_details".into(), Value::Array(all));
            if said.is_empty() {
                said = details.iter().map(|x| format!("{}{}", str_of(x, "text"), str_of(x, "summary"))).collect();
            }
            if self.reasoning.is_none() {
                self.open_reasoning(&mut out);
            }
        }
        if !said.is_empty() {
            self.open_reasoning(&mut out);
            let (id, text) = self.reasoning.as_mut().expect("opened");
            text.push_str(&said);
            out.push(EngineEvent::ItemDelta { item_id: id.clone(), delta: Delta::Text { text: said } });
        }
        if let Some(t) = d.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            if self.text.is_none() {
                let id = krowk_store::new_id();
                out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::AssistantText });
                self.text = Some((id, String::new()));
            }
            let (id, text) = self.text.as_mut().expect("opened");
            text.push_str(t);
            out.push(EngineEvent::ItemDelta { item_id: id.clone(), delta: Delta::Text { text: t.into() } });
        }
        for tc in d.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(self.calls.len() as u64);
            let (call_id, name) = (str_of(tc, "id"), tc.pointer("/function/name").and_then(Value::as_str).unwrap_or_default());
            let current = self.calls.iter().rposition(|c| c.index == index).filter(|&at| call_id.is_empty() || self.calls[at].call_id.is_empty() || self.calls[at].call_id == call_id);
            let at = match current {
                Some(at) => at,
                None => {
                    let id = krowk_store::new_id();
                    out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::ToolCall { call_id: call_id.into(), name: name.into() } });
                    self.calls.push(Call { index, id, call_id: call_id.into(), name: name.into(), args: String::new() });
                    self.calls.len() - 1
                }
            };
            let call = &mut self.calls[at];
            // An id or a name that arrives after the call's first fragment.
            if call.call_id.is_empty() {
                call.call_id = call_id.into();
            }
            if call.name.is_empty() {
                call.name = name.into();
            }
            if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
                call.args.push_str(a);
                out.push(EngineEvent::ItemDelta { item_id: call.id.clone(), delta: Delta::ToolInput { partial_json: a.into() } });
            }
        }
        if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(if r == "tool_calls" { "tool_use".into() } else { r.to_string() });
        }
        Ok(out)
    }

    fn open_reasoning(&mut self, out: &mut Vec<EngineEvent>) {
        if self.reasoning.is_none() {
            let id = krowk_store::new_id();
            out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::Reasoning });
            self.reasoning = Some((id, String::new()));
        }
    }

    /// Every open item, whole: reasoning, then the text, then the calls in
    /// the order the model made them.
    fn complete(&mut self) -> Result<Vec<EngineEvent>, EngineError> {
        let mut out = Vec::new();
        if let Some((id, text)) = self.reasoning.take() {
            let blob = ProviderBlob { provider: self.provider.clone(), wire_api: WireApi::ChatCompletions, data: Value::Object(std::mem::take(&mut self.vendor)) };
            self.push(&mut out, id, Item::Reasoning { text, blob: Some(blob) });
        }
        if let Some((id, text)) = self.text.take() {
            self.push(&mut out, id, Item::AssistantText { text });
        }
        for c in std::mem::take(&mut self.calls) {
            let input = if c.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).map_err(|e| {
                    EngineError::new("malformed_response", format!("the server sent arguments for {} that are not JSON ({e}) — the call may have been cut off by the output limit", c.name))
                })?
            };
            self.push(&mut out, c.id, Item::ToolCall { call_id: c.call_id, name: c.name, input });
        }
        Ok(out)
    }

    fn push(&mut self, out: &mut Vec<EngineEvent>, id: String, item: Item) {
        out.push(EngineEvent::ItemCompleted { item_id: id.clone(), item: item.clone() });
        self.items.push((id, item));
    }
}

impl Decode for Decoder {
    fn apply(&mut self, ev: &SseEvent) -> Result<Vec<EngineEvent>, EngineError> {
        Decoder::apply(self, ev)
    }

    fn done(&self) -> bool {
        self.done
    }

    fn ended(&mut self) -> Option<Vec<EngineEvent>> {
        self.stop_reason.as_ref()?;
        self.complete().ok()
    }

    /// Text that arrived is kept as far as it got. Reasoning is kept too
    /// when the vendor sends it as plain text — it needs no signature to
    /// stand — but half a call's arguments cannot be sent back.
    fn interrupt(&mut self) -> Vec<EngineEvent> {
        self.calls.clear();
        if self.vendor.contains_key("reasoning_details") {
            self.reasoning = None;
        }
        self.text = self.text.take().filter(|(_, t)| !t.is_empty());
        self.complete().unwrap_or_default()
    }

    fn finish(self, requested_model: &str, interrupted: bool) -> ModelResponse {
        ModelResponse {
            response_id: self.response_id,
            model: if self.model.is_empty() { requested_model.to_string() } else { self.model },
            usage: self.usage,
            stop_reason: self.stop_reason,
            items: self.items,
            interrupted,
        }
    }
}
