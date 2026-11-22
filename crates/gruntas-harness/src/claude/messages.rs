//! Anthropic's Messages stream, decoded straight into gruntas's items
//! (R-PROV-1): one content block is one item, streamed under one id. Claude
//! Code forwards this stream as its `stream_event` lines (`super::stream`).
//!
//! Thinking is where fidelity matters. A `thinking` block's text and its
//! `signature` must go back to the API exactly as they came, or the next
//! call is refused; a `redacted_thinking` block's `data` likewise. So the
//! item carries the readable text, and the signature or the redacted data
//! rides in the item's `ProviderBlob` untouched (R-LOG-3).

use crate::sse::SseEvent;
use crate::engine::{EngineError, EngineEvent};
use crate::protocol::{Delta, Item, ItemKind, ProviderBlob, Usage, WireApi};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const PROVIDER: &str = "anthropic";

enum Block {
    Text { id: String, text: String },
    Thinking { id: String, text: String, signature: String },
    Redacted { id: String, data: String },
    ToolUse { id: String, call_id: String, name: String, json: String },
    /// A block type gruntas does not model (a server tool's, say): skipped
    /// whole, so an unknown type never breaks the stream.
    Other,
}

/// The state of one response as its events arrive.
#[derive(Default)]
pub struct Decoder {
    blocks: BTreeMap<u64, Block>,
    pub items: Vec<(String, Item)>,
    pub response_id: Option<String>,
    pub model: String,
    pub usage: Usage,
    pub stop_reason: Option<String>,
    /// `message_stop` arrived: the response is whole.
    pub done: bool,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: u64,
    #[serde(default)]
    message: Option<Value>,
    #[serde(default)]
    content_block: Option<Value>,
    #[serde(default)]
    delta: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

impl Decoder {
    /// Folds one event in; returns what the client should be told.
    pub fn apply(&mut self, ev: &SseEvent) -> Result<Vec<EngineEvent>, EngineError> {
        if ev.event == "ping" {
            return Ok(Vec::new());
        }
        let raw: Raw = serde_json::from_str(&ev.data)
            .map_err(|e| EngineError::new("malformed_response", format!("Anthropic sent a stream event that is not JSON ({e}): {}", clip(&ev.data))))?;
        let mut out = Vec::new();
        match raw.kind.as_str() {
            "message_start" => {
                let m = raw.message.unwrap_or_default();
                self.response_id = m.get("id").and_then(Value::as_str).map(String::from);
                self.model = str_of(&m, "model").to_string();
                if let Some(u) = m.get("usage") {
                    self.take_usage(u);
                }
            }
            "content_block_start" => {
                let b = raw.content_block.unwrap_or_default();
                let id = gruntas_store::new_id();
                let (block, kind) = match str_of(&b, "type") {
                    "text" => (Block::Text { id: id.clone(), text: str_of(&b, "text").into() }, Some(ItemKind::AssistantText)),
                    "thinking" => (
                        Block::Thinking { id: id.clone(), text: str_of(&b, "thinking").into(), signature: str_of(&b, "signature").into() },
                        Some(ItemKind::Reasoning),
                    ),
                    "redacted_thinking" => (Block::Redacted { id: id.clone(), data: str_of(&b, "data").into() }, Some(ItemKind::Reasoning)),
                    "tool_use" => {
                        let (call_id, name) = (str_of(&b, "id").to_string(), str_of(&b, "name").to_string());
                        // An input that arrived whole in the start event, not
                        // as deltas, is kept as its JSON text.
                        let json = b.get("input").filter(|i| i.as_object().is_some_and(|o| !o.is_empty())).map(Value::to_string).unwrap_or_default();
                        let kind = ItemKind::ToolCall { call_id: call_id.clone(), name: name.clone() };
                        (Block::ToolUse { id: id.clone(), call_id, name, json }, Some(kind))
                    }
                    _ => (Block::Other, None),
                };
                if let Some(kind) = kind {
                    out.push(EngineEvent::ItemStarted { item_id: id, kind });
                }
                self.blocks.insert(raw.index, block);
            }
            "content_block_delta" => {
                let d = raw.delta.unwrap_or_default();
                match (self.blocks.get_mut(&raw.index), str_of(&d, "type")) {
                    (Some(Block::Text { id, text }), "text_delta") => {
                        let t = str_of(&d, "text");
                        text.push_str(t);
                        out.push(EngineEvent::ItemDelta { item_id: id.clone(), delta: Delta::Text { text: t.into() } });
                    }
                    (Some(Block::Thinking { id, text, .. }), "thinking_delta") => {
                        let t = str_of(&d, "thinking");
                        text.push_str(t);
                        out.push(EngineEvent::ItemDelta { item_id: id.clone(), delta: Delta::Text { text: t.into() } });
                    }
                    // A signature is never shown and never streamed: it is
                    // collected whole, byte for byte.
                    (Some(Block::Thinking { signature, .. }), "signature_delta") => signature.push_str(str_of(&d, "signature")),
                    (Some(Block::ToolUse { id, json, .. }), "input_json_delta") => {
                        let p = str_of(&d, "partial_json");
                        json.push_str(p);
                        out.push(EngineEvent::ItemDelta { item_id: id.clone(), delta: Delta::ToolInput { partial_json: p.into() } });
                    }
                    // Citations and anything newer ride along unmodelled.
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(block) = self.blocks.remove(&raw.index)
                    && let Some((id, item)) = complete(block)?
                {
                    out.push(EngineEvent::ItemCompleted { item_id: id.clone(), item: item.clone() });
                    self.items.push((id, item));
                }
            }
            "message_delta" => {
                if let Some(r) = raw.delta.as_ref().and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    self.stop_reason = Some(r.into());
                }
                if let Some(u) = &raw.usage {
                    self.take_usage(u);
                }
            }
            "message_stop" => self.done = true,
            "error" => {
                let e = raw.error.unwrap_or_default();
                let kind = str_of(&e, "type");
                // The statuses the API documents for each error type, so a
                // failure mid-stream exits the way the same failure up front would.
                let (code, status) = match kind {
                    "overloaded_error" => ("provider_unavailable", 529),
                    "api_error" => ("provider_unavailable", 500),
                    "rate_limit_error" => ("rate_limited", 429),
                    _ => ("provider_error", 0),
                };
                let message = format!("Anthropic stopped the response ({kind}: {}) — run the prompt again with --resume", str_of(&e, "message"));
                return Err(EngineError::new(code, message).with_status(status));
            }
            _ => {}
        }
        Ok(out)
    }

    /// Usage fields are cumulative: `message_delta` repeats and grows what
    /// `message_start` said, so each field present overwrites.
    fn take_usage(&mut self, u: &Value) {
        let n = |k: &str| u.get(k).and_then(Value::as_i64);
        if let Some(v) = n("input_tokens") {
            self.usage.input_tokens = v;
        }
        if let Some(v) = n("cache_read_input_tokens") {
            self.usage.cache_read_tokens = v;
        }
        if let Some(v) = n("cache_creation_input_tokens") {
            self.usage.cache_write_tokens = v;
        }
        if let Some(out) = n("output_tokens") {
            // Thinking is counted inside output; where the API says how much
            // of it was thinking, the split is kept, as the importers keep it.
            let thinking = u.pointer("/output_tokens_details/thinking_tokens").and_then(Value::as_i64).unwrap_or(0).clamp(0, out.max(0));
            self.usage.output_tokens = out - thinking;
            self.usage.reasoning_tokens = thinking;
        }
    }

    /// What survives an interrupt: text that arrived is kept as far as it
    /// got; reasoning without its signature and half a tool input cannot be
    /// sent back, so they are dropped.
    pub fn interrupt(&mut self) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        for (_, block) in std::mem::take(&mut self.blocks) {
            if let Block::Text { id, text } = block
                && !text.is_empty()
            {
                let item = Item::AssistantText { text };
                out.push(EngineEvent::ItemCompleted { item_id: id.clone(), item: item.clone() });
                self.items.push((id, item));
            }
        }
        out
    }
}

fn complete(block: Block) -> Result<Option<(String, Item)>, EngineError> {
    Ok(Some(match block {
        Block::Text { id, text } => (id, Item::AssistantText { text }),
        Block::Thinking { id, text, signature } => (id, Item::Reasoning { text, blob: Some(blob(json!({ "type": "thinking", "signature": signature }))) }),
        Block::Redacted { id, data } => (id, Item::Reasoning { text: String::new(), blob: Some(blob(json!({ "type": "redacted_thinking", "data": data }))) }),
        Block::ToolUse { id, call_id, name, json } => {
            let input = if json.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&json).map_err(|e| {
                    EngineError::new("malformed_response", format!("Anthropic sent tool input for {name} that is not JSON ({e}) — the call may have been cut off by max_tokens"))
                })?
            };
            (id, Item::ToolCall { call_id, name, input })
        }
        Block::Other => return Ok(None),
    }))
}

fn blob(data: Value) -> ProviderBlob {
    ProviderBlob { provider: PROVIDER.into(), wire_api: WireApi::AnthropicMessages, data }
}

fn clip(s: &str) -> String {
    if s.chars().count() <= 200 { s.to_string() } else { s.chars().take(200).collect::<String>() + "…" }
}
