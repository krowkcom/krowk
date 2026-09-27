//! Claude Code's stream-json, translated into krowk's events (R-BACK-5).
//!
//! With `--include-partial-messages`, `claude -p` forwards the Messages API
//! stream it reads as `stream_event` lines — `message_start`, the content
//! blocks, `message_stop` — so the same decoder the native Anthropic client
//! uses turns them into items: one content block is one item, streamed under
//! one id, its thinking signature collected whole. Claude Code also sends
//! each finished block as an `assistant` message; those are skipped for a
//! message whose stream was seen, and are the items themselves for one that
//! was not, so an older binary, or one that drops a partial, still logs
//! every block.
//!
//! What Claude Code runs is reported as `user` messages holding
//! `tool_result` blocks. They arrive before the stream of the message that
//! called the tool has stopped, so they are held until that response is
//! logged: the log reads call, response, result, as a native turn does.
//!
//! Lines of a subagent (`parent_tool_use_id` set) belong to the subagent's
//! own conversation, which Claude Code keeps; the turn logs the `Task` call
//! and its result. What each of the subagent's calls cost is still the
//! session's spend, so the usage of its `assistant` messages is reported,
//! once per message, as `SubagentResponse` — by a `Meter` the process
//! keeps, since a background agent's calls go on between turns. `result`
//! ends the turn, with Claude Code's own `total_cost_usd` for it.

use crate::anthropic::stream::Decoder;
use crate::engine::{EngineError, EngineEvent};
use crate::protocol::{Item, ItemKind, LimitState, LimitStatus, ProviderBlob, Usage, WireApi};
use crate::sse::SseEvent;
use serde_json::{json, Value};
use std::collections::HashSet;

/// What `system`/`init` says of the process, at the start of every turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Init {
    pub session_id: String,
    pub model: String,
    pub cwd: String,
    pub tools: Vec<String>,
    /// The protocol features this binary announces, e.g. `interrupt_receipt_v1`.
    pub capabilities: Vec<String>,
    /// Where its API credential comes from: `none` for a Claude login.
    pub api_key_source: String,
    /// Each MCP server and its status, e.g. `krowk` / `connected`.
    pub mcp_servers: Vec<(String, String)>,
    /// The permission mode it runs the turn in, e.g. `default`.
    pub permission_mode: String,
}

/// How the turn ended, as `result` says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub subtype: String,
    pub is_error: bool,
    /// The final answer, or the error's words.
    pub text: String,
    pub errors: Vec<String>,
    pub session_id: String,
    /// The HTTP status of an API failure, when Claude Code reports one.
    pub api_status: Option<u16>,
    /// The result closes a turn Claude Code began by itself (its `origin`,
    /// e.g. `task-notification`), not the one krowk prompted.
    pub unprompted: bool,
}

struct SubagentMessage {
    id: String,
    model: String,
    latest: Usage,
    reported: Usage,
}

/// What `latest` adds to `reported`, field by field; never negative.
fn growth(latest: &Usage, reported: &Usage) -> Usage {
    let d = |a: i64, b: i64| (a - b).max(0);
    Usage {
        input_tokens: d(latest.input_tokens, reported.input_tokens),
        output_tokens: d(latest.output_tokens, reported.output_tokens),
        cache_read_tokens: d(latest.cache_read_tokens, reported.cache_read_tokens),
        cache_write_tokens: d(latest.cache_write_tokens, reported.cache_write_tokens),
        reasoning_tokens: d(latest.reasoning_tokens, reported.reasoning_tokens),
    }
}

/// A response rebuilt from `assistant` messages, for a message whose stream
/// never arrived.
struct Whole {
    id: String,
    model: String,
    usage: Usage,
    stop_reason: Option<String>,
    item_ids: Vec<String>,
}

#[derive(Default)]
pub struct Translator {
    /// The streamed response in progress.
    decoder: Option<Decoder>,
    /// Messages whose stream was seen: their `assistant` echoes are skipped.
    streamed: HashSet<String>,
    whole: Option<Whole>,
    /// Tool results waiting for the response that called them.
    held: Vec<EngineEvent>,
    pub init: Option<Init>,
    pub outcome: Option<Outcome>,
    /// The subagents' calls: the process's, lent to the turn.
    pub meter: Meter,
    /// What Claude Code last said of the account's rate limit.
    pub limit: Option<LimitStatus>,
}

/// A `rate_limit_event`'s `rate_limit_info`, as krowk's limit status:
/// `allowed`, `allowed_warning` (near it) or `rejected` (reached), over a
/// window such as `five_hour`, with when it resets (seconds since the
/// epoch) and how much is used (`utilization`, 0–1), when it says.
pub fn rate_limit(info: &Value) -> Option<LimitStatus> {
    let status = match str_of(info, "status") {
        "allowed" => LimitState::Allowed,
        "allowed_warning" => LimitState::Warning,
        "rejected" => LimitState::Limited,
        _ => return None,
    };
    let window = Some(str_of(info, "rateLimitType").to_string()).filter(|w| !w.is_empty());
    let resets_at_ms = info.get("resetsAt").and_then(Value::as_i64).map(|s| s * 1000);
    let used_percent = info.get("utilization").and_then(Value::as_f64).map(|u| if u <= 1.0 { u * 100.0 } else { u });
    Some(LimitStatus { status, window, used_percent, resets_at_ms })
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A blob Claude Code produced replays only to Claude Code: the host never
/// sends it to the Messages API.
fn retag(ev: EngineEvent) -> EngineEvent {
    match ev {
        EngineEvent::ItemCompleted { item_id, item: Item::Reasoning { text, blob: Some(b) } } => {
            EngineEvent::ItemCompleted { item_id, item: Item::Reasoning { text, blob: Some(ProviderBlob { wire_api: WireApi::ClaudeCode, ..b }) } }
        }
        ev => ev,
    }
}

impl Translator {
    /// Folds one stream-json line in; returns what the host should be told.
    /// Control messages are the engine's, and never reach here.
    pub fn apply(&mut self, msg: &Value) -> Result<Vec<EngineEvent>, EngineError> {
        let mut out = Vec::new();
        if msg.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
            if str_of(msg, "type") == "assistant" {
                self.meter.see(msg, Some(&mut out));
            }
            return Ok(out);
        }
        match str_of(msg, "type") {
            "system" if str_of(msg, "subtype") == "init" => self.init = Some(init(msg)),
            "stream_event" => self.stream_event(msg.get("event").unwrap_or(&Value::Null), &mut out)?,
            "assistant" => self.assistant(msg.get("message").unwrap_or(&Value::Null), &mut out),
            "user" => {
                // A turn's own tool results; a prompt echoed back, or Claude
                // Code's "[Request interrupted by user]" note, is not one.
                let content = msg.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                let results: Vec<EngineEvent> = content.iter().filter(|b| str_of(b, "type") == "tool_result").flat_map(tool_result).collect();
                if self.whole.is_some() {
                    self.close_whole(&mut out);
                }
                if self.decoder.is_some() {
                    self.held.extend(results);
                } else {
                    out.extend(results);
                }
            }
            "result" => {
                self.finish(&mut out);
                if let Some(usd) = msg.get("total_cost_usd").and_then(Value::as_f64) {
                    out.push(EngineEvent::ReportedCost { usd });
                }
                self.outcome = Some(Outcome {
                    subtype: str_of(msg, "subtype").into(),
                    is_error: msg.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    text: str_of(msg, "result").into(),
                    errors: msg.get("errors").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default(),
                    session_id: str_of(msg, "session_id").into(),
                    api_status: msg.get("api_error_status").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()),
                    unprompted: msg.get("origin").is_some_and(|o| !o.is_null()),
                });
            }
            // How near the account is to its plan's limit (R-INST-6): shown
            // per instance, and what a limited turn's failure names.
            "rate_limit_event" => {
                if let Some(l) = msg.get("rate_limit_info").and_then(rate_limit) {
                    self.limit = Some(l.clone());
                    out.push(EngineEvent::Limits(l));
                }
            }
            // Status lines, hook lifecycle, thinking-token estimates: Claude
            // Code's own bookkeeping, not the conversation.
            _ => {}
        }
        Ok(out)
    }

    fn stream_event(&mut self, ev: &Value, out: &mut Vec<EngineEvent>) -> Result<(), EngineError> {
        let kind = str_of(ev, "type");
        if kind == "message_start" {
            // A response that never stopped is closed with what it had.
            self.finish(out);
            if let Some(id) = ev.pointer("/message/id").and_then(Value::as_str) {
                self.streamed.insert(id.into());
            }
            self.decoder = Some(Decoder::default());
        }
        let Some(d) = self.decoder.as_mut() else { return Ok(()) };
        let evs = d.apply(&SseEvent { event: kind.into(), data: ev.to_string() })?;
        out.extend(evs.into_iter().map(retag));
        if d.done {
            self.close_stream(out);
        }
        Ok(())
    }

    /// Logs the streamed response, then the tool results it was waiting on.
    fn close_stream(&mut self, out: &mut Vec<EngineEvent>) {
        let Some(mut d) = self.decoder.take() else { return };
        if !d.done {
            out.extend(d.interrupt().into_iter().map(retag));
        }
        out.push(EngineEvent::ResponseCompleted {
            response_id: d.response_id.clone(),
            model: d.model.clone(),
            usage: d.usage,
            stop_reason: d.stop_reason.clone(),
            item_ids: d.items.iter().map(|(id, _)| id.clone()).collect(),
        });
        out.append(&mut self.held);
    }

    fn assistant(&mut self, m: &Value, out: &mut Vec<EngineEvent>) {
        let id = str_of(m, "id").to_string();
        if self.streamed.contains(&id) {
            return;
        }
        if self.whole.as_ref().is_some_and(|w| w.id != id) {
            self.close_whole(out);
        }
        let w = self.whole.get_or_insert_with(|| Whole { id: id.clone(), model: String::new(), usage: Usage::default(), stop_reason: None, item_ids: Vec::new() });
        w.model = str_of(m, "model").into();
        if let Some(u) = m.get("usage") {
            w.usage = usage(u);
        }
        if let Some(r) = m.get("stop_reason").and_then(Value::as_str) {
            w.stop_reason = Some(r.into());
        }
        for b in m.get("content").and_then(Value::as_array).into_iter().flatten() {
            if let Some(item) = block(b) {
                let item_id = krowk_store::new_id();
                out.push(EngineEvent::ItemStarted { item_id: item_id.clone(), kind: item.kind() });
                out.push(EngineEvent::ItemCompleted { item_id: item_id.clone(), item });
                w.item_ids.push(item_id);
            }
        }
    }

    fn close_whole(&mut self, out: &mut Vec<EngineEvent>) {
        if let Some(w) = self.whole.take() {
            out.push(EngineEvent::ResponseCompleted { response_id: Some(w.id).filter(|i| !i.is_empty()), model: w.model, usage: w.usage, stop_reason: w.stop_reason, item_ids: w.item_ids });
        }
    }

    /// Closes whatever is open: the turn is over, or a new response began.
    /// Text cut off mid-stream is kept as far as it got; half a tool input
    /// or unsigned reasoning is not (the decoder's rule).
    pub fn finish(&mut self, out: &mut Vec<EngineEvent>) {
        self.close_stream(out);
        self.close_whole(out);
        out.append(&mut self.held);
        self.meter.report_all(out);
    }
}

/// What Claude Code's subagents' calls have cost, kept for the process
/// rather than a turn: a background agent's messages go on between turns
/// and into the next one, and a message counted in one turn is not counted
/// again in the next.
///
/// Each subagent message seen, in the order first seen: its model, the
/// latest usage Claude Code sent for it, and how much of that has been
/// reported. Claude Code sends one `assistant` line per content block, each
/// with the message's usage so far, and parallel agents interleave theirs,
/// so what is reported is the growth since the last report — when another
/// message's line arrives, and at a turn's end — and no message is ever
/// counted twice.
#[derive(Default)]
pub struct Meter {
    subagents: Vec<SubagentMessage>,
    /// The message the last subagent line was for.
    at: Option<usize>,
}

impl Meter {
    /// A subagent's `assistant` line. In a turn (`out`) the message before
    /// it, when it was another, is reported; between turns nothing is —
    /// there is no turn to log it in — and the next turn reports it.
    pub fn see(&mut self, msg: &Value, out: Option<&mut Vec<EngineEvent>>) {
        let Some(m) = msg.get("message") else { return };
        let Some(u) = m.get("usage") else { return };
        let id = str_of(m, "id").to_string();
        let at = match self.subagents.iter().position(|s| s.id == id && !id.is_empty()) {
            Some(at) => at,
            None => {
                self.subagents.push(SubagentMessage { id, model: String::new(), latest: Usage::default(), reported: Usage::default() });
                self.subagents.len() - 1
            }
        };
        if let Some(out) = out
            && let Some(prev) = self.at.filter(|p| *p != at)
        {
            self.report(prev, out);
        }
        let sm = &mut self.subagents[at];
        sm.model = str_of(m, "model").into();
        sm.latest = usage(u);
        self.at = Some(at);
    }

    /// Reports what every message has grown by since it was last reported.
    pub fn report_all(&mut self, out: &mut Vec<EngineEvent>) {
        for at in 0..self.subagents.len() {
            self.report(at, out);
        }
        self.at = None;
    }

    /// Reports what a subagent message has grown by since it was last
    /// reported, if anything.
    fn report(&mut self, at: usize, out: &mut Vec<EngineEvent>) {
        let sm = &mut self.subagents[at];
        let more = growth(&sm.latest, &sm.reported);
        if more == Usage::default() {
            return;
        }
        sm.reported = sm.latest;
        out.push(EngineEvent::SubagentResponse { response_id: (!sm.id.is_empty()).then(|| sm.id.clone()), model: sm.model.clone(), usage: more });
    }
}

fn init(msg: &Value) -> Init {
    let strings = |k: &str| msg.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
    Init {
        session_id: str_of(msg, "session_id").into(),
        model: str_of(msg, "model").into(),
        cwd: str_of(msg, "cwd").into(),
        tools: strings("tools"),
        capabilities: strings("capabilities"),
        api_key_source: str_of(msg, "apiKeySource").into(),
        permission_mode: str_of(msg, "permissionMode").into(),
        mcp_servers: msg
            .get("mcp_servers")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|s| (str_of(s, "name").to_string(), str_of(s, "status").to_string())).collect())
            .unwrap_or_default(),
    }
}

/// A Messages API usage block in krowk's five columns, reasoning split out
/// of output where Claude Code says how much of it was thinking.
fn usage(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let out = n("output_tokens");
    let thinking = u.pointer("/output_tokens_details/thinking_tokens").and_then(Value::as_i64).unwrap_or(0).clamp(0, out.max(0));
    Usage { input_tokens: n("input_tokens"), output_tokens: out - thinking, cache_read_tokens: n("cache_read_input_tokens"), cache_write_tokens: n("cache_creation_input_tokens"), reasoning_tokens: thinking }
}

/// One content block of a whole `assistant` message as an item.
fn block(b: &Value) -> Option<Item> {
    let blob = |data: Value| Some(ProviderBlob { provider: crate::anthropic::stream::PROVIDER.into(), wire_api: WireApi::ClaudeCode, data });
    Some(match str_of(b, "type") {
        "text" => Item::AssistantText { text: str_of(b, "text").into() },
        "thinking" => Item::Reasoning { text: str_of(b, "thinking").into(), blob: blob(json!({"type": "thinking", "signature": str_of(b, "signature")})) },
        "redacted_thinking" => Item::Reasoning { text: String::new(), blob: blob(json!({"type": "redacted_thinking", "data": str_of(b, "data")})) },
        "tool_use" => Item::ToolCall { call_id: str_of(b, "id").into(), name: str_of(b, "name").into(), input: b.get("input").cloned().unwrap_or_else(|| json!({})) },
        _ => return None,
    })
}

/// A `tool_result` block as a started and completed result item. Its
/// content is text, or blocks: text is kept as it is, anything else (an
/// image, a tool reference) is named in brackets.
fn tool_result(b: &Value) -> Vec<EngineEvent> {
    let call_id = str_of(b, "tool_use_id").to_string();
    let output = match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match str_of(p, "type") {
                "text" => str_of(p, "text").to_string(),
                "tool_reference" => format!("[tool reference: {}]", str_of(p, "tool_name")),
                other => format!("[{}]", if other.is_empty() { "content" } else { other }),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let item = Item::ToolResult { call_id: call_id.clone(), output, is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false) };
    let item_id = krowk_store::new_id();
    vec![EngineEvent::ItemStarted { item_id: item_id.clone(), kind: ItemKind::ToolResult { call_id } }, EngineEvent::ItemCompleted { item_id, item }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut Translator, lines: &str) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        for l in lines.lines().filter(|l| !l.trim().is_empty()) {
            out.extend(t.apply(&serde_json::from_str(l).unwrap()).unwrap());
        }
        out
    }

    fn completed(evs: &[EngineEvent]) -> Vec<Item> {
        evs.iter().filter_map(|e| if let EngineEvent::ItemCompleted { item, .. } = e { Some(item.clone()) } else { None }).collect()
    }

    const TOOL_TURN: &str = r#"
{"type":"system","subtype":"init","cwd":"/repo","session_id":"cc-1","tools":["Read","mcp__krowk__session_info"],"mcp_servers":[{"name":"krowk","status":"connected"}],"model":"claude-haiku-4-5","apiKeySource":"none","capabilities":["interrupt_receipt_v1"]}
{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1","model":"claude-haiku-4-5","usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":1}}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"c2ln"}},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_1","model":"claude-haiku-4-5","content":[{"type":"thinking","thinking":"","signature":"c2ln"}]},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp__krowk__session_info","input":{}}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null}
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":[{"type":"text","text":"krowk session: s-1"}]}]},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":40,"output_tokens_details":{"thinking_tokens":12}}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_sub","content":[{"type":"text","text":"a subagent's words"}]},"parent_tool_use_id":"toolu_task"}
{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_2","model":"claude-haiku-4-5","usage":{"input_tokens":3,"output_tokens":1}}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"s-"}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1"}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"s-1","session_id":"cc-1"}
"#;

    #[test]
    fn r_budget_1_a_subagents_calls_are_metered_once_each_and_the_reported_total_comes_with_the_result() {
        let mut t = Translator::default();
        let sub = |id: &str, out: i64| format!(r#"{{"type":"assistant","parent_tool_use_id":"toolu_task","session_id":"s","message":{{"id":"{id}","model":"claude-haiku-4-5","role":"assistant","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":5,"output_tokens":{out}}}}}}}"#);
        // Two parallel Tasks, their lines interleaved: 1, 1, 2, 1 again.
        let lines = [sub("msg_sub_1", 3), sub("msg_sub_1", 40), sub("msg_sub_2", 7), sub("msg_sub_1", 55), r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s","total_cost_usd":0.25}"#.to_string()].join("\n");
        let evs = feed(&mut t, &lines);
        let metered: Vec<(Option<&str>, i64)> = evs
            .iter()
            .filter_map(|e| match e {
                EngineEvent::SubagentResponse { response_id, usage, .. } => Some((response_id.as_deref(), usage.output_tokens)),
                _ => None,
            })
            .collect();
        assert_eq!(metered, [(Some("msg_sub_1"), 40), (Some("msg_sub_2"), 7), (Some("msg_sub_1"), 15)], "each message's growth, reported once");
        let total = |id: &str| {
            evs.iter()
                .filter_map(|e| match e {
                    EngineEvent::SubagentResponse { response_id: Some(r), usage, .. } if r == id => Some((usage.input_tokens, usage.output_tokens)),
                    _ => None,
                })
                .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
        };
        assert_eq!((total("msg_sub_1"), total("msg_sub_2")), ((5, 55), (5, 7)), "every message counted at its final usage, none twice");
        assert!(completed(&evs).is_empty(), "the subagent's conversation is its own");
        assert_eq!(evs.last(), Some(&EngineEvent::ReportedCost { usd: 0.25 }));
    }

    #[test]
    fn r_back_5_a_streamed_tool_turn_becomes_the_same_items_a_native_turn_logs() {
        let mut t = Translator::default();
        let evs = feed(&mut t, TOOL_TURN);
        let init = t.init.clone().unwrap();
        assert_eq!((init.session_id.as_str(), init.api_key_source.as_str()), ("cc-1", "none"));
        assert_eq!(init.capabilities, ["interrupt_receipt_v1"]);
        assert_eq!(init.mcp_servers, [("krowk".to_string(), "connected".to_string())]);
        let items = completed(&evs);
        assert_eq!(items.len(), 4, "{items:?}");
        let Item::Reasoning { blob: Some(b), .. } = &items[0] else { panic!("{items:?}") };
        assert_eq!((b.wire_api, b.data["signature"].as_str()), (WireApi::ClaudeCode, Some("c2ln")), "the signature whole, replayable only to Claude Code");
        assert!(matches!(&items[1], Item::ToolCall { name, call_id, .. } if name == "mcp__krowk__session_info" && call_id == "toolu_1"));
        assert_eq!(items[2], Item::ToolResult { call_id: "toolu_1".into(), output: "krowk session: s-1".into(), is_error: false });
        assert_eq!(items[3], Item::AssistantText { text: "s-1".into() });
        // The result is logged after the response that called the tool.
        let order: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                EngineEvent::ItemCompleted { item: Item::ToolResult { .. }, .. } => Some("result"),
                EngineEvent::ResponseCompleted { .. } => Some("response"),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["response", "result", "response"]);
        let EngineEvent::ResponseCompleted { usage, stop_reason, response_id, item_ids, .. } = evs.iter().find(|e| matches!(e, EngineEvent::ResponseCompleted { .. })).unwrap() else { unreachable!() };
        assert_eq!((usage.output_tokens, usage.reasoning_tokens, usage.cache_read_tokens), (28, 12, 100));
        assert_eq!((stop_reason.as_deref(), response_id.as_deref(), item_ids.len()), (Some("tool_use"), Some("msg_1"), 2));
        assert!(evs.iter().any(|e| matches!(e, EngineEvent::ItemDelta { .. })), "deltas stream");
        assert!(!format!("{evs:?}").contains("a subagent's words"), "a subagent's lines are its own");
        assert_eq!(t.outcome.unwrap().text, "s-1");
    }

    #[test]
    fn r_back_5_without_partial_messages_whole_messages_are_the_items_and_an_abort_keeps_its_text() {
        let mut t = Translator::default();
        let evs = feed(
            &mut t,
            r##"
{"type":"assistant","message":{"id":"msg_1","model":"m","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"README.md"}}],"usage":{"input_tokens":4,"output_tokens":2}},"parent_tool_use_id":null}
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"# krowk","is_error":false}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_2","model":"m","content":[{"type":"text","text":"done"}]},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"cc-2"}
"##,
        );
        let kinds: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                EngineEvent::ItemCompleted { item: Item::ToolCall { .. }, .. } => Some("call"),
                EngineEvent::ItemCompleted { item: Item::ToolResult { .. }, .. } => Some("result"),
                EngineEvent::ItemCompleted { item: Item::AssistantText { .. }, .. } => Some("text"),
                EngineEvent::ResponseCompleted { .. } => Some("response"),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["call", "response", "result", "text", "response"]);

        // An interrupt: the stream stops mid-text, and the result follows.
        let mut t = Translator::default();
        let evs = feed(
            &mut t,
            r##"
{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_3","model":"m","usage":{"input_tokens":1,"output_tokens":1}}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1\n2"}},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_3","model":"m","content":[{"type":"text","text":"1\n2"}]},"parent_tool_use_id":null,"aborted":true}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null}
{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"cc-3","errors":["aborted"]}
"##,
        );
        assert_eq!(completed(&evs), [Item::AssistantText { text: "1\n2".into() }]);
        assert!(matches!(evs.last(), Some(EngineEvent::ResponseCompleted { item_ids, .. }) if item_ids.len() == 1));
        let o = t.outcome.unwrap();
        assert_eq!((o.subtype.as_str(), o.is_error, o.errors), ("error_during_execution", true, vec!["aborted".to_string()]));
    }
}
