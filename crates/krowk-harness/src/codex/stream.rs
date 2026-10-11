//! `codex app-server`'s notifications as krowk's events. Codex reports a
//! turn as thread items — an agent message, reasoning, a command it ran, a
//! patch it applied, an MCP or dynamic tool call — each `item/started`,
//! streamed by its own `…/delta` notifications, then `item/completed`; and
//! each model call's tokens as `thread/tokenUsage/updated`. The translator
//! turns that into the event model every engine keeps:
//!
//! - an agent message is `assistantText`, reasoning is `reasoning` (its
//!   summary, or its text when Codex streams that: Codex keeps the
//!   encrypted reasoning itself, so there is never a blob to replay);
//! - every tool Codex runs is a `toolCall`, completed as soon as Codex says
//!   what it is, and a `toolResult` when Codex says how it came out — a
//!   command as `shell`, a patch as `apply_patch`, an MCP tool as
//!   `mcp__<server>__<tool>`, krowk's own bridged tools as
//!   `mcp__krowk__<tool>` like every backend's;
//! - a token-usage update closes one model call: `response.completed`
//!   names the model output that completed since the last one.
//!
//! Codex's own items are keyed by its ids; krowk mints its own for the log,
//! and a tool call keeps Codex's id as its `callId`. The prompt Codex echoes
//! as a `userMessage` is not repeated: the host logged it already, and
//! steering is logged by the engine where it was accepted.

use crate::engine::EngineEvent;
use crate::protocol::{Delta, Item, ItemKind, Usage};
use serde_json::{json, Value};
use std::collections::HashMap;

/// The most of a command's output a tool result keeps: a build log can be
/// megabytes, and the log is not where it is read back from — Codex's own
/// transcript has it whole.
pub const OUTPUT_CAP: usize = 64 * 1024;

/// One turn's translation.
#[derive(Debug, Default)]
pub struct Translator {
    /// The model Codex says answers, for `response.completed`.
    pub model: String,
    /// Codex item id → krowk item id, for the items started and not yet
    /// completed. A tool call's result gets an id of its own.
    open: HashMap<String, Open>,
    /// Model output completed since the last model call closed.
    pending: Vec<String>,
    /// The thread's total tokens at the last usage update: an update that
    /// does not move it is not another model call.
    total: Option<i64>,
    /// A patch's files by Codex item id, as the approval request is
    /// answered from them.
    pub changes: HashMap<String, Vec<String>>,
    /// Why krowk declined an item Codex asked about, for its result.
    pub declined: HashMap<String, String>,
    /// The last error Codex reported that it will not retry.
    pub error: Option<Value>,
    /// A child thread's (`codex::children`): its own `collabAgentToolCall`s
    /// are tool calls of its transcript, where a grandchild shows. The
    /// session's are not: each child is its own row, ended by its own end.
    collab: bool,
}

#[derive(Debug)]
struct Open {
    id: String,
    kind: OpenKind,
}

#[derive(Debug, PartialEq)]
enum OpenKind {
    Text(String),
    Reasoning(String),
    /// A tool call, already completed: the result is what is pending.
    Call,
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// Codex's token counts in krowk's five columns. Codex reports input with
/// the cached part in it and output with the reasoning part, as the
/// Responses API does; krowk prices each column once, so they are split.
pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
    let (input, cached, written) = (n("inputTokens"), n("cachedInputTokens"), n("cacheWriteInputTokens"));
    let (output, reasoning) = (n("outputTokens"), n("reasoningOutputTokens"));
    let cached = cached.min(input);
    let written = written.min(input - cached);
    Usage {
        input_tokens: input - cached - written,
        cache_read_tokens: cached,
        cache_write_tokens: written,
        output_tokens: output - reasoning.min(output),
        reasoning_tokens: reasoning.min(output),
    }
}

fn capped(s: &str) -> String {
    if s.len() <= OUTPUT_CAP {
        return s.to_string();
    }
    let mut cut = OUTPUT_CAP;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more bytes in Codex's transcript]", &s[..cut], s.len() - cut)
}

/// Every file a patch writes: each change's path, and where an update moves
/// it to (`kind.move_path`) — a move writes its destination, so an approval
/// that judged only the source would let a patch move a file into `.git`.
/// Takes the thread item's `changes` array, or the first protocol's
/// `fileChanges` map (path → change, the move in the change's `move_path`).
pub fn change_paths(changes: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |path: &str, change: &Value| {
        if !path.is_empty() {
            out.push(path.to_string());
        }
        let to = change.pointer("/kind/move_path").or_else(|| change.get("move_path")).and_then(Value::as_str).filter(|t| !t.is_empty());
        if let Some(to) = to {
            out.push(to.to_string());
        }
    };
    match changes {
        Value::Array(a) => a.iter().for_each(|c| add(str_of(c, "path"), c)),
        Value::Object(m) => m.iter().for_each(|(p, c)| add(p, c)),
        _ => {}
    }
    out
}

/// The text of MCP-style content: its text parts, one per line.
fn content_text(parts: Option<&Value>) -> String {
    parts
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

impl Translator {
    pub fn new(model: &str) -> Translator {
        Translator { model: model.into(), ..Translator::default() }
    }

    /// A child thread's translator: its collab calls are its tool calls.
    pub fn child(model: &str) -> Translator {
        Translator { collab: true, ..Translator::new(model) }
    }

    /// The tool call an item is, when it is one: its name and input.
    fn call(&self, item: &Value) -> Option<(String, Value)> {
        let args = || item.get("arguments").cloned().unwrap_or(Value::Null);
        Some(match str_of(item, "type") {
            // `spawnAgent` as `spawn_agent`, Codex's own name for the tool.
            "collabAgentToolCall" if self.collab => {
                let tool: String = str_of(item, "tool").chars().flat_map(|c| if c.is_ascii_uppercase() { vec!['_', c.to_ascii_lowercase()] } else { vec![c] }).collect();
                let mut input = json!({});
                for k in ["prompt", "model"] {
                    if let Some(v) = item.get(k).filter(|v| v.is_string()) {
                        input[k] = v.clone();
                    }
                }
                (tool, input)
            }
            "commandExecution" => ("shell".into(), json!({"command": str_of(item, "command"), "cwd": str_of(item, "cwd")})),
            "fileChange" => {
                let changes: Vec<Value> = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map(|c| {
                        c.iter()
                            .map(|c| {
                                let mut v = json!({"path": str_of(c, "path"), "kind": c.pointer("/kind/type").cloned().unwrap_or(Value::Null)});
                                if let Some(to) = c.pointer("/kind/move_path").and_then(Value::as_str) {
                                    v["movePath"] = json!(to);
                                }
                                v
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ("apply_patch".into(), json!({ "changes": changes }))
            }
            "mcpToolCall" => (format!("mcp__{}__{}", str_of(item, "server"), str_of(item, "tool")), args()),
            "dynamicToolCall" => match item.get("namespace").and_then(Value::as_str) {
                Some(ns) if !ns.is_empty() => (format!("mcp__{ns}__{}", str_of(item, "tool")), args()),
                _ => (str_of(item, "tool").to_string(), args()),
            },
            "webSearch" => ("web_search".into(), json!({"query": str_of(item, "query")})),
            _ => return None,
        })
    }

    /// What a finished tool call produced, and whether it failed.
    fn result(&self, item: &Value, codex_id: &str) -> (String, bool) {
        let status = str_of(item, "status");
        let declined = |what: &str| self.declined.get(codex_id).cloned().unwrap_or_else(|| format!("{what} was declined"));
        match str_of(item, "type") {
            "commandExecution" => {
                let out = item.get("aggregatedOutput").and_then(Value::as_str).unwrap_or_default();
                let code = item.get("exitCode").and_then(Value::as_i64);
                match status {
                    "declined" => (declined("the command"), true),
                    _ => {
                        let failed = status == "failed" || code.is_some_and(|c| c != 0);
                        let tail = match code {
                            Some(c) if c != 0 => format!("{}(exit code {c})", if out.is_empty() || out.ends_with('\n') { "" } else { "\n" }),
                            _ => String::new(),
                        };
                        (capped(&format!("{out}{tail}")), failed)
                    }
                }
            }
            "fileChange" => {
                let paths: Vec<&str> = item.get("changes").and_then(Value::as_array).map(|c| c.iter().map(|c| str_of(c, "path")).collect()).unwrap_or_default();
                match status {
                    "completed" => (format!("applied: {}", paths.join(", ")), false),
                    "declined" => (declined("the change"), true),
                    s => (format!("the change was not applied ({s}): {}", paths.join(", ")), true),
                }
            }
            "mcpToolCall" => match item.get("error").filter(|e| !e.is_null()) {
                Some(e) => (str_of(e, "message").to_string(), true),
                None => (capped(&content_text(item.pointer("/result/content"))), status == "failed"),
            },
            "dynamicToolCall" => (capped(&content_text(item.get("contentItems"))), item.get("success").and_then(Value::as_bool) == Some(false) || status == "failed"),
            "webSearch" => (item.get("results").filter(|r| !r.is_null()).map(|r| capped(&r.to_string())).unwrap_or_else(|| format!("searched: {}", str_of(item, "query"))), false),
            // Each agent it reached and where that agent stands.
            "collabAgentToolCall" => {
                let states: Vec<String> = item
                    .get("agentsStates")
                    .and_then(Value::as_object)
                    .map(|m| m.iter().map(|(th, st)| format!("{th}: {}{}", str_of(st, "status"), st.get("message").and_then(Value::as_str).map(|m| format!(" — {m}")).unwrap_or_default())).collect())
                    .unwrap_or_default();
                let receivers: Vec<&str> = item.get("receiverThreadIds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let text = if !states.is_empty() { states.join("\n") } else if !receivers.is_empty() { format!("agents: {}", receivers.join(", ")) } else { status.to_string() };
                (capped(&text), status == "failed")
            }
            _ => (String::new(), false),
        }
    }

    fn start(&mut self, item: &Value, out: &mut Vec<EngineEvent>) {
        let codex_id = str_of(item, "id").to_string();
        if codex_id.is_empty() || self.open.contains_key(&codex_id) {
            return;
        }
        let id = krowk_store::new_id();
        match str_of(item, "type") {
            "agentMessage" => {
                out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::AssistantText });
                self.open.insert(codex_id, Open { id, kind: OpenKind::Text(String::new()) });
            }
            "reasoning" => {
                out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::Reasoning });
                self.open.insert(codex_id, Open { id, kind: OpenKind::Reasoning(String::new()) });
            }
            _ => {
                let Some((name, input)) = self.call(item) else { return };
                if str_of(item, "type") == "fileChange" {
                    self.note_changes(&codex_id, item);
                }
                // The call is whole as soon as Codex names it: it is logged
                // now, with the model call it came out of, and its result
                // follows when Codex reports one.
                out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::ToolCall { call_id: codex_id.clone(), name: name.clone() } });
                out.push(EngineEvent::ItemCompleted { item_id: id.clone(), item: Item::ToolCall { call_id: codex_id.clone(), name, input } });
                self.pending.push(id);
                self.open.insert(codex_id, Open { id: String::new(), kind: OpenKind::Call });
            }
        }
    }

    fn note_changes(&mut self, codex_id: &str, item: &Value) {
        if let Some(c) = item.get("changes").filter(|c| c.as_array().is_some_and(|a| !a.is_empty())) {
            self.changes.insert(codex_id.to_string(), change_paths(c));
        }
    }

    fn complete(&mut self, item: &Value, out: &mut Vec<EngineEvent>) {
        let codex_id = str_of(item, "id").to_string();
        let kind = str_of(item, "type");
        if !matches!(kind, "agentMessage" | "reasoning") && self.call(item).is_none() {
            return;
        }
        // An item Codex completes without having started it is started now.
        if !self.open.contains_key(&codex_id) {
            self.start(item, out);
        }
        let Some(open) = self.open.remove(&codex_id) else { return };
        match open.kind {
            OpenKind::Text(_) => {
                out.push(EngineEvent::ItemCompleted { item_id: open.id.clone(), item: Item::AssistantText { text: str_of(item, "text").to_string() } });
                self.pending.push(open.id);
            }
            OpenKind::Reasoning(streamed) => {
                let join = |k: &str| item.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n")).unwrap_or_default();
                let text = [join("summary"), join("content"), streamed].into_iter().find(|t| !t.trim().is_empty()).unwrap_or_default();
                out.push(EngineEvent::ItemCompleted { item_id: open.id.clone(), item: Item::Reasoning { text, blob: None } });
                self.pending.push(open.id);
            }
            OpenKind::Call => {
                if kind == "fileChange" {
                    self.note_changes(&codex_id, item);
                }
                let (output, is_error) = self.result(item, &codex_id);
                let id = krowk_store::new_id();
                out.push(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::ToolResult { call_id: codex_id.clone() } });
                out.push(EngineEvent::ItemCompleted { item_id: id, item: Item::ToolResult { call_id: codex_id, output, is_error } });
            }
        }
    }

    fn delta(&mut self, params: &Value, out: &mut Vec<EngineEvent>) {
        let Some(open) = self.open.get_mut(str_of(params, "itemId")) else { return };
        let text = str_of(params, "delta");
        match &mut open.kind {
            OpenKind::Text(t) | OpenKind::Reasoning(t) if !text.is_empty() => {
                t.push_str(text);
                out.push(EngineEvent::ItemDelta { item_id: open.id.clone(), delta: Delta::Text { text: text.into() } });
            }
            _ => {}
        }
    }

    /// One notification of the thread's current turn in; what the host
    /// should be told out.
    pub fn apply(&mut self, method: &str, params: &Value) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        match method {
            "item/started" => self.start(params.get("item").unwrap_or(&Value::Null), &mut out),
            "item/completed" => self.complete(params.get("item").unwrap_or(&Value::Null), &mut out),
            "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => self.delta(params, &mut out),
            "item/reasoning/summaryPartAdded" => {
                if let Some(Open { kind: OpenKind::Reasoning(t), .. }) = self.open.get_mut(str_of(params, "itemId"))
                    && !t.is_empty()
                {
                    t.push_str("\n\n");
                }
            }
            "item/fileChange/patchUpdated" => {
                let codex_id = str_of(params, "itemId").to_string();
                if let Some(c) = params.get("changes") {
                    self.note_changes(&codex_id, &json!({ "changes": c }));
                }
            }
            "thread/tokenUsage/updated" => {
                let total = params.pointer("/tokenUsage/total/totalTokens").and_then(Value::as_i64);
                if total.is_some() && total == self.total {
                    return out;
                }
                self.total = total;
                let last = params.pointer("/tokenUsage/last").cloned().unwrap_or(Value::Null);
                out.push(EngineEvent::ResponseCompleted { response_id: None, model: self.model.clone(), usage: usage(&last), stop_reason: None, item_ids: std::mem::take(&mut self.pending) });
            }
            "model/rerouted" => {
                let to = str_of(params, "toModel");
                if !to.is_empty() {
                    self.model = to.into();
                }
            }
            "error" if params.get("willRetry").and_then(Value::as_bool) != Some(true) => self.error = params.get("error").cloned(),
            _ => {}
        }
        out
    }

    /// The turn is over, however it ended: a message cut off keeps its text
    /// as far as it got, and model output no usage update closed is closed
    /// here, so the last answer is always on a `response.completed`.
    pub fn finish(&mut self, out: &mut Vec<EngineEvent>) {
        let mut open: Vec<(String, Open)> = self.open.drain().collect();
        open.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        for (_, o) in open {
            match o.kind {
                OpenKind::Text(t) if !t.is_empty() => {
                    out.push(EngineEvent::ItemCompleted { item_id: o.id.clone(), item: Item::AssistantText { text: t } });
                    self.pending.push(o.id);
                }
                // Reasoning cut off, or a call whose result never came: the
                // call is logged, and a result nobody saw is not invented.
                _ => {}
            }
        }
        if !self.pending.is_empty() {
            out.push(EngineEvent::ResponseCompleted { response_id: None, model: self.model.clone(), usage: Usage::default(), stop_reason: None, item_ids: std::mem::take(&mut self.pending) });
        }
    }
}

/// Codex's own subagents: threads other than the session's, which Codex
/// spawns and keeps. Their items are their conversation, not the
/// session's, but every model call one makes is the session's spend
/// (R-SUB-4), so each thread's `thread/tokenUsage/updated` is metered as a
/// `SubagentResponse`, once per move of that thread's total, and priced by
/// the model the thread said it runs.
#[derive(Debug, Default)]
pub struct SubThreads {
    /// Each thread seen: its model, and the total last metered.
    threads: HashMap<String, (String, Option<i64>)>,
}

impl SubThreads {
    /// One notification of a thread other than `own`: what to meter.
    pub fn apply(&mut self, own: &str, method: &str, params: &Value) -> Option<EngineEvent> {
        match method {
            "thread/started" => {
                let th = params.get("thread")?;
                let id = str_of(th, "id");
                if id.is_empty() || id == own {
                    return None;
                }
                self.threads.entry(id.into()).or_default().0 = str_of(th, "model").into();
                None
            }
            "model/rerouted" => {
                let to = str_of(params, "toModel");
                if !to.is_empty() {
                    self.threads.entry(str_of(params, "threadId").into()).or_default().0 = to.into();
                }
                None
            }
            "thread/tokenUsage/updated" => {
                let entry = self.threads.entry(str_of(params, "threadId").into()).or_default();
                let total = params.pointer("/tokenUsage/total/totalTokens").and_then(Value::as_i64);
                if total.is_some() && total == entry.1 {
                    return None;
                }
                entry.1 = total;
                let last = params.pointer("/tokenUsage/last").cloned().unwrap_or(Value::Null);
                Some(EngineEvent::SubagentResponse { response_id: None, model: entry.0.clone(), usage: usage(&last) })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(method: &str, params: Value) -> (String, Value) {
        (method.into(), params)
    }

    #[test]
    fn r_sub_4_codexs_own_subagent_threads_are_metered_once_per_call_at_their_model() {
        let mut subs = SubThreads::default();
        let usage = |total: i64, out: i64| json!({"threadId": "sub-1", "turnId": "x", "tokenUsage": {"last": {"inputTokens": 50, "cachedInputTokens": 20, "outputTokens": out, "reasoningOutputTokens": 0, "totalTokens": 50 + out}, "total": {"totalTokens": total}}});
        assert_eq!(subs.apply("own", "thread/started", &json!({"thread": {"id": "own", "model": "gpt-5.5"}})), None, "the session's own thread is not a subagent");
        assert_eq!(subs.apply("own", "thread/started", &json!({"thread": {"id": "sub-1", "model": "gpt-5.4-mini"}})), None);
        let first = subs.apply("own", "thread/tokenUsage/updated", &usage(60, 10));
        assert_eq!(
            first,
            Some(EngineEvent::SubagentResponse { response_id: None, model: "gpt-5.4-mini".into(), usage: Usage { input_tokens: 30, output_tokens: 10, cache_read_tokens: 20, ..Usage::default() } })
        );
        assert_eq!(subs.apply("own", "thread/tokenUsage/updated", &usage(60, 10)), None, "the same total again is the same call");
        assert!(subs.apply("own", "thread/tokenUsage/updated", &usage(130, 20)).is_some());
        assert_eq!(subs.apply("own", "item/agentMessage/delta", &json!({"threadId": "sub-1", "delta": "hi"})), None, "its items are its own conversation");
    }

    /// The pinned schema gives each thread its own `ThreadTokenUsage`, and
    /// krowk meters a call by its `last`, never by the difference of two
    /// `total`s — `total` only tells a repeat from a new call. So even a
    /// Codex whose parent total counted its sub-threads' tokens too would
    /// not have them counted twice: the parent's call is its `last`.
    #[test]
    fn r_sub_4_a_parent_total_that_includes_its_sub_threads_still_meters_each_call_once() {
        let mut t = Translator::new("gpt-5.5");
        let mut subs = SubThreads::default();
        let sub = subs.apply("own", "thread/tokenUsage/updated", &json!({"threadId": "sub-1", "turnId": "x", "tokenUsage": {"last": {"inputTokens": 300, "outputTokens": 7, "totalTokens": 307}, "total": {"totalTokens": 307}}}));
        // The parent's own call is 100 in and 10 out; its total says 417,
        // as if it held the sub-thread's 307 as well.
        let own = t.apply("thread/tokenUsage/updated", &json!({"threadId": "own", "turnId": "y", "tokenUsage": {"last": {"inputTokens": 100, "outputTokens": 10, "totalTokens": 110}, "total": {"totalTokens": 417}}}));
        let usage_of = |evs: &[EngineEvent]| evs.iter().find_map(|e| match e { EngineEvent::ResponseCompleted { usage, .. } | EngineEvent::SubagentResponse { usage, .. } => Some(*usage), _ => None });
        let sub = usage_of(&sub.into_iter().collect::<Vec<_>>()).unwrap();
        let own = usage_of(&own).unwrap();
        assert_eq!((sub.input_tokens, sub.output_tokens), (300, 7));
        assert_eq!((own.input_tokens, own.output_tokens), (100, 10), "the parent's call is its last, whatever its total holds");
    }

    #[test]
    fn r_back_5_codex_items_become_krowks_events_in_the_same_shape_as_a_native_turn() {
        let mut t = Translator::new("gpt-5.5");
        let mut out = Vec::new();
        for (m, p) in [
            n("item/started", json!({"item": {"type": "userMessage", "id": "u1", "content": []}})),
            n("item/started", json!({"item": {"type": "reasoning", "id": "r1", "summary": [], "content": []}})),
            n("item/reasoning/summaryTextDelta", json!({"itemId": "r1", "delta": "Looking at the tree", "summaryIndex": 0})),
            n("item/completed", json!({"item": {"type": "reasoning", "id": "r1", "summary": ["Looking at the tree"], "content": []}})),
            n("item/started", json!({"item": {"type": "commandExecution", "id": "c1", "command": "ls", "cwd": "/repo", "commandActions": [], "status": "inProgress"}})),
            n("thread/tokenUsage/updated", json!({"tokenUsage": {"last": {"inputTokens": 100, "cachedInputTokens": 60, "outputTokens": 30, "reasoningOutputTokens": 20, "totalTokens": 130}, "total": {"inputTokens": 100, "cachedInputTokens": 60, "outputTokens": 30, "reasoningOutputTokens": 20, "totalTokens": 130}}})),
            n("item/completed", json!({"item": {"type": "commandExecution", "id": "c1", "command": "ls", "cwd": "/repo", "commandActions": [], "status": "completed", "exitCode": 0, "aggregatedOutput": "README.md\n"}})),
            n("item/started", json!({"item": {"type": "agentMessage", "id": "a1", "text": ""}})),
            n("item/agentMessage/delta", json!({"itemId": "a1", "delta": "There is "})),
            n("item/agentMessage/delta", json!({"itemId": "a1", "delta": "a README."})),
            n("item/completed", json!({"item": {"type": "agentMessage", "id": "a1", "text": "There is a README."}})),
            n("thread/tokenUsage/updated", json!({"tokenUsage": {"last": {"inputTokens": 150, "cachedInputTokens": 100, "outputTokens": 8, "reasoningOutputTokens": 0, "totalTokens": 158}, "total": {"totalTokens": 288}}})),
            n("thread/tokenUsage/updated", json!({"tokenUsage": {"last": {"inputTokens": 150, "outputTokens": 8, "totalTokens": 158}, "total": {"totalTokens": 288}}})),
        ] {
            out.extend(t.apply(&m, &p));
        }
        t.finish(&mut out);
        let kinds: Vec<String> = out
            .iter()
            .map(|e| match e {
                EngineEvent::ItemStarted { kind, .. } => format!("started {}", serde_json::to_value(kind).unwrap()["kind"].as_str().unwrap()),
                EngineEvent::ItemDelta { .. } => "delta".into(),
                EngineEvent::ItemCompleted { item, .. } => format!("completed {}", serde_json::to_value(item).unwrap()["kind"].as_str().unwrap()),
                EngineEvent::ResponseCompleted { item_ids, .. } => format!("response {}", item_ids.len()),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "started reasoning", "delta", "completed reasoning",
                "started toolCall", "completed toolCall",
                "response 2",
                "started toolResult", "completed toolResult",
                "started assistantText", "delta", "delta", "completed assistantText",
                "response 1",
            ],
            "the prompt is not echoed, the second, unchanged usage update is not a call, and nothing is closed twice"
        );
        let EngineEvent::ResponseCompleted { usage, model, .. } = &out[5] else { panic!() };
        assert_eq!((usage.input_tokens, usage.cache_read_tokens, usage.output_tokens, usage.reasoning_tokens, model.as_str()), (40, 60, 10, 20, "gpt-5.5"));
        let EngineEvent::ItemCompleted { item: Item::ToolCall { call_id, name, input }, .. } = &out[4] else { panic!() };
        assert_eq!((call_id.as_str(), name.as_str(), input["command"].as_str()), ("c1", "shell", Some("ls")));
        let EngineEvent::ItemCompleted { item: Item::ToolResult { output, is_error, .. }, .. } = &out[7] else { panic!() };
        assert_eq!((output.as_str(), *is_error), ("README.md\n", false));
    }

    #[test]
    fn r_back_5_tool_results_say_how_each_kind_of_call_came_out() {
        let mut t = Translator::new("m");
        t.declined.insert("c2".into(), "krowk declined it".into());
        let results: Vec<(String, bool)> = [
            json!({"type": "commandExecution", "id": "c1", "command": "false", "cwd": "/", "status": "failed", "exitCode": 1, "aggregatedOutput": "no"}),
            json!({"type": "commandExecution", "id": "c2", "command": "rm -rf /", "cwd": "/", "status": "declined"}),
            json!({"type": "fileChange", "id": "f1", "status": "completed", "changes": [{"path": "/repo/a.txt", "kind": {"type": "update"}, "diff": ""}]}),
            json!({"type": "mcpToolCall", "id": "m1", "server": "docs", "tool": "search", "arguments": {}, "status": "failed", "error": {"message": "server gone"}}),
            json!({"type": "dynamicToolCall", "id": "d1", "namespace": "krowk", "tool": "session_info", "arguments": {}, "status": "completed", "success": true, "contentItems": [{"type": "inputText", "text": "krowk session: s"}]}),
        ]
        .iter()
        .filter_map(|item| {
            let events = t.apply("item/completed", &json!({ "item": item }));
            events.into_iter().find_map(|e| match e {
                EngineEvent::ItemCompleted { item: Item::ToolResult { output, is_error, .. }, .. } => Some((output, is_error)),
                _ => None,
            })
        })
        .collect();
        assert_eq!(results[0], ("no\n(exit code 1)".to_string(), true));
        assert_eq!(results[1], ("krowk declined it".to_string(), true));
        assert_eq!(results[2], ("applied: /repo/a.txt".to_string(), false));
        assert_eq!(results[3], ("server gone".to_string(), true));
        assert_eq!(results[4], ("krowk session: s".to_string(), false));
        assert_eq!(t.changes["f1"], ["/repo/a.txt"]);
        let mut named = Vec::new();
        for item in [json!({"type": "dynamicToolCall", "id": "d2", "namespace": "krowk", "tool": "session_info", "arguments": {}}), json!({"type": "mcpToolCall", "id": "m2", "server": "docs", "tool": "search", "arguments": {}})] {
            for e in t.apply("item/started", &json!({ "item": item })) {
                if let EngineEvent::ItemCompleted { item: Item::ToolCall { name, .. }, .. } = e {
                    named.push(name);
                }
            }
        }
        assert_eq!(named, ["mcp__krowk__session_info", "mcp__docs__search"], "bridged tools are named as on every backend");
        assert!(capped(&"x".repeat(OUTPUT_CAP + 10)).ends_with("[10 more bytes in Codex's transcript]"));
    }

    #[test]
    fn r_back_5_an_interrupted_message_keeps_its_text_and_is_closed() {
        let mut t = Translator::new("m");
        let mut out = t.apply("item/started", &json!({"item": {"type": "agentMessage", "id": "a1", "text": ""}}));
        out.extend(t.apply("item/agentMessage/delta", &json!({"itemId": "a1", "delta": "1, 2, "})));
        out.extend(t.apply("error", &json!({"error": {"message": "stream gone"}, "willRetry": true})));
        assert!(t.error.is_none(), "an error Codex retries is not the turn's");
        t.finish(&mut out);
        assert!(matches!(&out[2], EngineEvent::ItemCompleted { item: Item::AssistantText { text }, .. } if text == "1, 2, "));
        assert!(matches!(&out[3], EngineEvent::ResponseCompleted { item_ids, .. } if item_ids.len() == 1));
    }
}
