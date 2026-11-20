//! The Responses API's stream, decoded straight into krowk's items
//! (R-PROV-1): one output item is one krowk item, streamed under one id.
//!
//! An item is announced by `response.output_item.added`, grows through its
//! deltas — `output_text`, `reasoning_summary_text` (or `reasoning_text`),
//! `function_call_arguments`, `custom_tool_call_input` — and is whole at
//! `response.output_item.done`, whose `item` is the authority: what the
//! deltas said is only what the client saw typed. A reasoning item is kept
//! whole in its blob, `encrypted_content` byte for byte, because that is
//! what goes back on the next call.

use crate::engine::{EngineError, EngineEvent};
use crate::http::clip;
use crate::wire::Decode;
use crate::native::ModelResponse;
use crate::protocol::{Delta, Item, ItemKind, ProviderBlob, Usage, WireApi};
use crate::sse::SseEvent;
use serde_json::{json, Value};
use std::collections::BTreeMap;

struct Open {
    id: String,
    kind: ItemKind,
    /// What the deltas said, kept for an interrupt.
    text: String,
}

/// The state of one response as its events arrive.
pub struct Decoder {
    provider: String,
    /// Open items by their output index.
    open: BTreeMap<u64, Open>,
    pub items: Vec<(String, Item)>,
    pub response_id: Option<String>,
    pub model: String,
    pub usage: Usage,
    pub stop_reason: Option<String>,
    /// `response.completed` (or `response.incomplete`) arrived.
    pub done: bool,
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

impl Decoder {
    /// A decoder whose reasoning blobs name `provider`.
    pub fn new(provider: &str) -> Decoder {
        Decoder { provider: provider.into(), open: BTreeMap::new(), items: Vec::new(), response_id: None, model: String::new(), usage: Usage::default(), stop_reason: None, done: false }
    }

    fn open_at(&mut self, v: &Value) -> Option<&mut Open> {
        let index = v.get("output_index").and_then(Value::as_u64)?;
        self.open.get_mut(&index)
    }

    fn take_response(&mut self, r: &Value) {
        if let Some(id) = r.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.into());
        }
        if let Some(m) = r.get("model").and_then(Value::as_str) {
            self.model = m.into();
        }
        if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
            let n = |p: &str| u.pointer(p).and_then(Value::as_i64).unwrap_or(0).max(0);
            // Input counts the cached part, and output the reasoning part;
            // krowk's columns are each priced once, so they are split out.
            let (input, cached) = (n("/input_tokens"), n("/input_tokens_details/cached_tokens"));
            let (output, reasoning) = (n("/output_tokens"), n("/output_tokens_details/reasoning_tokens"));
            self.usage = Usage {
                input_tokens: input - cached.min(input),
                cache_read_tokens: cached.min(input),
                output_tokens: output - reasoning.min(output),
                reasoning_tokens: reasoning.min(output),
                cache_write_tokens: 0,
            };
        }
    }

    /// Folds one event in; returns what the client should be told.
    pub fn apply(&mut self, ev: &SseEvent) -> Result<Vec<EngineEvent>, EngineError> {
        if ev.data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(&ev.data)
            .map_err(|e| EngineError::new("malformed_response", format!("OpenAI sent a stream event that is not JSON ({e}): {}", clip(&ev.data, 200))))?;
        let kind = v.get("type").and_then(Value::as_str).unwrap_or(&ev.event).to_string();
        let mut out = Vec::new();
        match kind.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(r) = v.get("response") {
                    self.take_response(r);
                }
            }
            "response.output_item.added" => {
                let item = v.get("item").cloned().unwrap_or_default();
                let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(self.open.len() as u64);
                let kind = match str_of(&item, "type") {
                    "message" => Some(ItemKind::AssistantText),
                    "reasoning" => Some(ItemKind::Reasoning),
                    "function_call" | "custom_tool_call" => Some(ItemKind::ToolCall { call_id: str_of(&item, "call_id").into(), name: str_of(&item, "name").into() }),
                    // A built-in tool's item (web search, say): not krowk's.
                    _ => None,
                };
                if let Some(kind) = kind {
                    let id = krowk_store::new_id();
                    out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: kind.clone() });
                    self.open.insert(index, Open { id, kind, text: String::new() });
                }
            }
            "response.output_text.delta" | "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let d = str_of(&v, "delta").to_string();
                if let Some(o) = self.open_at(&v) {
                    o.text.push_str(&d);
                    out.push(EngineEvent::ItemDelta { item_id: o.id.clone(), delta: Delta::Text { text: d } });
                }
            }
            // A freeform tool's input streams as its raw text.
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let d = str_of(&v, "delta").to_string();
                if let Some(o) = self.open_at(&v) {
                    o.text.push_str(&d);
                    out.push(EngineEvent::ItemDelta { item_id: o.id.clone(), delta: Delta::ToolInput { partial_json: d } });
                }
            }
            "response.output_item.done" => {
                let index = v.get("output_index").and_then(Value::as_u64);
                let item = v.get("item").cloned().unwrap_or_default();
                if let Some(open) = index.and_then(|i| self.open.remove(&i)) {
                    let done = self.complete(&item)?;
                    out.push(EngineEvent::ItemCompleted { item_id: open.id.clone(), item: done.clone() });
                    self.items.push((open.id, done));
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = v.get("response").cloned().unwrap_or_default();
                self.take_response(&r);
                self.stop_reason = Some(match r.pointer("/incomplete_details/reason").and_then(Value::as_str) {
                    Some(reason) => reason.to_string(),
                    None if self.items.iter().any(|(_, i)| matches!(i, Item::ToolCall { .. })) => "tool_use".into(),
                    None => str_of(&r, "status").to_string(),
                });
                self.done = true;
            }
            "response.failed" | "error" => {
                let e = if kind == "error" { v.get("error").cloned().unwrap_or_else(|| v.clone()) } else { v.pointer("/response/error").cloned().unwrap_or_default() };
                let (code, message) = (str_of(&e, "code"), str_of(&e, "message"));
                let (ours, status) = match code {
                    "rate_limit_exceeded" => ("rate_limited", 429),
                    "server_error" | "server_is_overloaded" => ("provider_unavailable", 500),
                    "context_length_exceeded" => ("context_too_long", 400),
                    _ => ("provider_error", 0),
                };
                return Err(EngineError::new(ours, format!("OpenAI stopped the response ({code}: {message}) — run the prompt again with --resume")).with_status(status));
            }
            _ => {}
        }
        Ok(out)
    }

    /// An output item, whole, as krowk's item.
    fn complete(&self, item: &Value) -> Result<Item, EngineError> {
        Ok(match str_of(item, "type") {
            "message" => {
                let parts = item.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
                let text: String = parts.iter().filter(|p| p["type"] == "output_text").map(|p| str_of(p, "text")).collect();
                let refusal: String = parts.iter().filter(|p| p["type"] == "refusal").map(|p| str_of(p, "refusal")).collect();
                Item::AssistantText { text: if text.is_empty() { refusal } else { text } }
            }
            "reasoning" => {
                let pick = |k: &str| -> Vec<String> { item.get(k).and_then(Value::as_array).into_iter().flatten().map(|p| str_of(p, "text").to_string()).filter(|t| !t.is_empty()).collect() };
                let mut text = pick("summary");
                if text.is_empty() {
                    text = pick("content");
                }
                Item::Reasoning { text: text.join("\n\n"), blob: Some(ProviderBlob { provider: self.provider.clone(), wire_api: WireApi::OpenaiResponses, data: item.clone() }) }
            }
            "custom_tool_call" => Item::ToolCall { call_id: str_of(item, "call_id").into(), name: str_of(item, "name").into(), input: json!(str_of(item, "input")) },
            _ => {
                let (name, args) = (str_of(item, "name"), str_of(item, "arguments"));
                let input = if args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(args).map_err(|e| {
                        EngineError::new("malformed_response", format!("OpenAI sent arguments for {name} that are not JSON ({e}) — the call may have been cut off by the output limit"))
                    })?
                };
                Item::ToolCall { call_id: str_of(item, "call_id").into(), name: name.into(), input }
            }
        })
    }
}

impl Decode for Decoder {
    fn apply(&mut self, ev: &SseEvent) -> Result<Vec<EngineEvent>, EngineError> {
        Decoder::apply(self, ev)
    }

    fn done(&self) -> bool {
        self.done
    }

    /// Text that arrived is kept as far as it got; reasoning without its
    /// encrypted content and half a call's arguments cannot be sent back,
    /// so they are dropped.
    fn interrupt(&mut self) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        for (_, o) in std::mem::take(&mut self.open) {
            if o.kind == ItemKind::AssistantText && !o.text.is_empty() {
                let item = Item::AssistantText { text: o.text };
                out.push(EngineEvent::ItemCompleted { item_id: o.id.clone(), item: item.clone() });
                self.items.push((o.id, item));
            }
        }
        out
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
