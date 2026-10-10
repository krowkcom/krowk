//! Claude Code's agents as children of the session (R-SUB-7): each of the
//! conversation's own agents — the rule `backend.agents` keeps its list by
//! (`own`) — is reported as events of its own, which the host can write to
//! a child session: `BackendChildStarted` at its `task_started`, the parent's
//! prompt as its first item (R-SUB-8), each of its items, and
//! `BackendChildEnded`.
//!
//! An agent's lines carry no task id: they name the `Agent` call that
//! started it (`parent_tool_use_id`, the `tool_use_id` of its
//! `task_started`). They come as finished `assistant` and `user` messages,
//! never a stream (checked against `claude` 2.1.289, `agents_*.txt`), and
//! each agent's are translated by a `Translator::child` of its own, so
//! parallel agents' messages never mix. A foreground agent's first line is
//! its prompt, which the translator does not take for an item; a background
//! agent's prompt is only in `task_started`. Either way the prompt is the
//! item the started event is followed by.
//!
//! **A line before its `task_started`** — none was, in the recordings — is
//! held, per `parent_tool_use_id` and at most `HOLD` lines an agent, the
//! oldest dropped, and released once the agent has started, after its
//! prompt and a note of what was dropped (`DROPPED`). At most `WAITING`
//! agents' lines are held, the one held longest going whole, and what is
//! held goes at a turn's end unless a background agent runs on: an older
//! binary sends agents' lines with no `task_started`, which nothing would
//! release.
//!
//! **A grandchild** (an agent's own agent, `spawn_depth` above 1 or
//! `owned_by_subagent`) is no child of the session: its lines go to its
//! depth-1 ancestor's transcript, as that agent's tool calls and their
//! results (`Translator::aside`). Every call made inside an agent's
//! conversation is remembered against that agent, so a line naming one is
//! found its ancestor at any depth.
//!
//! **Its end** is its `task_updated` that finishes it or its
//! `task_notification`, whichever comes first: `completed` is done,
//! `killed` and `stopped` interrupted, anything else failed. A background
//! agent `background_tasks_changed` no longer lists has finished: it ends
//! when Claude Code says how, or, never told, done once the conversation
//! goes on (the next `init`, or the turn's end). A foreground
//! agent's last lines can come after that — a call it was stopped in is
//! answered then — so its end is reported once the parent's `Agent` call
//! has its result, or with the turn that ran it. A process let go ends them
//! all, interrupted, with why.
//!
//! Metering is not done here: the session's `Translator` meters every
//! agent's calls once, and a child's responses carry no usage.

use super::stream::Translator;
use crate::engine::EngineEvent;
use crate::protocol::{ChildState, Item, ItemKind, Usage};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// How many lines of one agent are held before its `task_started`.
pub const HOLD: usize = 256;
/// How many agents' lines are held at once: past it the agent held
/// longest goes, whole. A binary that sends an agent's lines and no
/// `task_started`, or an agent `own` turns away, is never released.
pub const WAITING: usize = 32;

/// The note in an agent's transcript where lines held for it were dropped:
/// krowk's words, not the agent's.
pub const DROPPED: &str = "<dropped-lines>";

/// Whether a `task_started` is of one of the conversation's own agents:
/// `local_agent`, not one a subagent started, at the first depth (or none
/// said) — not a shell, and not an agent's agent.
pub fn own(msg: &Value) -> bool {
    msg["task_type"] == "local_agent" && msg["owned_by_subagent"] != true && msg.get("spawn_depth").and_then(Value::as_u64).is_none_or(|d| d <= 1)
}

struct Child {
    task_id: String,
    /// None once it ended: a line of it after that is dropped.
    translator: Option<Translator>,
    background: bool,
    /// The parent's `Agent` call has its result: no line of it follows.
    answered: bool,
    /// How it ended, once Claude Code said, not reported yet.
    end: Option<(ChildState, Option<String>)>,
    /// Run in the background, and `background_tasks_changed` no longer
    /// lists it: it has finished, and ends at the latest when Claude Code
    /// goes on with the conversation.
    unlisted: bool,
}

#[derive(Default)]
struct Held {
    lines: VecDeque<Value>,
    dropped: usize,
}

/// The process's agents, by the `Agent` call that started each.
#[derive(Default)]
pub struct Children {
    agents: HashMap<String, Child>,
    /// Every call made inside an agent's conversation, and its own, to the
    /// depth-1 agent's call it is under.
    under: HashMap<String, String>,
    /// Lines naming a call no agent is known by yet, and those calls in
    /// the order they were first held.
    held: HashMap<String, Held>,
    waiting: VecDeque<String>,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A task's status in Claude Code's words, as a child's end.
fn state(status: &str) -> ChildState {
    match status {
        "completed" => ChildState::Done,
        "killed" | "stopped" => ChildState::Interrupted,
        _ => ChildState::Failed,
    }
}

fn wrap(task_id: &str, evs: Vec<EngineEvent>) -> impl Iterator<Item = EngineEvent> + '_ {
    evs.into_iter().map(move |ev| {
        // The agent's spend is the parent's `SubagentResponse`, counted once.
        let ev = match ev {
            EngineEvent::ResponseCompleted { response_id, model, stop_reason, item_ids, .. } => EngineEvent::ResponseCompleted { response_id, model, usage: Usage::default(), stop_reason, item_ids },
            ev => ev,
        };
        EngineEvent::BackendChild { task_id: task_id.to_string(), event: Box::new(ev) }
    })
}

fn item(task_id: &str, item: Item) -> [EngineEvent; 2] {
    let item_id = krowk_store::new_id();
    [
        EngineEvent::BackendChild { task_id: task_id.into(), event: Box::new(EngineEvent::ItemStarted { item_id: item_id.clone(), kind: ItemKind::UserText }) },
        EngineEvent::BackendChild { task_id: task_id.into(), event: Box::new(EngineEvent::ItemCompleted { item_id, item }) },
    ]
}

impl Children {
    /// Folds one line of Claude Code's stream in (not a control message);
    /// returns the agents' events it makes.
    pub fn apply(&mut self, msg: &Value) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        if msg["type"] == "system" {
            self.system(msg, &mut out);
            return out;
        }
        match msg.get("parent_tool_use_id").and_then(Value::as_str) {
            Some(call) => self.line(call, msg.clone(), &mut out),
            None => self.answered(msg, &mut out),
        }
        out
    }

    fn system(&mut self, msg: &Value, out: &mut Vec<EngineEvent>) {
        let task = s(msg, "task_id");
        match s(msg, "subtype") {
            "task_started" if own(msg) => {
                let call = s(msg, "tool_use_id");
                if call.is_empty() || task.is_empty() || self.agents.contains_key(call) {
                    return;
                }
                let prompt = s(msg, "prompt").to_string();
                out.push(EngineEvent::BackendChildStarted {
                    task_id: task.into(),
                    call_id: call.into(),
                    description: s(msg, "description").into(),
                    agent: Some(s(msg, "subagent_type").to_string()).filter(|a| !a.is_empty()),
                    prompt: prompt.clone(),
                    background: msg["is_backgrounded"] == true,
                });
                out.extend(item(task, Item::user(prompt)));
                self.agents.insert(call.into(), Child { task_id: task.into(), translator: Some(Translator::child()), background: msg["is_backgrounded"] == true, answered: false, end: None, unlisted: false });
                self.under.insert(call.into(), call.into());
                self.release(call, out);
            }
            "task_updated" => {
                let patch = &msg["patch"];
                if patch["is_backgrounded"] == true
                    && let Some(c) = self.agents.values_mut().find(|c| c.task_id == task)
                {
                    c.background = true;
                }
                let st = s(patch, "status");
                if !matches!(st, "" | "pending" | "running") {
                    let error = patch.get("error").and_then(Value::as_str).map(String::from);
                    self.finished(task, state(st), error, out);
                }
            }
            "task_notification" => {
                let st = state(s(msg, "status"));
                let error = (st == ChildState::Failed).then(|| Some(s(msg, "summary").to_string()).filter(|e| !e.is_empty())).flatten();
                self.finished(task, st, error, out);
            }
            // Claude Code takes a background agent off its list before it
            // says how the agent ended (recorded: `task_updated` follows),
            // so its end waits for that word, or for the conversation to
            // go on without it.
            "background_tasks_changed" => {
                let listed: Vec<&str> = msg["tasks"].as_array().map(|a| a.iter().filter_map(|t| t["task_id"].as_str()).collect()).unwrap_or_default();
                let gone: Vec<String> = self.agents.iter_mut().filter(|(_, c)| c.background && c.translator.is_some() && !listed.contains(&c.task_id.as_str())).map(|(k, c)| {
                    c.unlisted = true;
                    k.clone()
                }).collect();
                for call in gone {
                    if self.agents.get(&call).is_some_and(|c| c.end.is_some()) {
                        self.end(&call, out);
                    }
                }
            }
            "init" => self.unlisted(out),
            _ => {}
        }
    }

    /// Background agents no longer listed, whose end Claude Code never
    /// said: finished, as the list says.
    fn unlisted(&mut self, out: &mut Vec<EngineEvent>) {
        let open: Vec<String> = self.agents.iter().filter(|(_, c)| c.unlisted && c.translator.is_some()).map(|(k, _)| k.clone()).collect();
        for call in open {
            if let Some(c) = self.agents.get_mut(&call) {
                c.end.get_or_insert((ChildState::Done, None));
            }
            self.end(&call, out);
        }
    }

    /// Claude Code said a task ended: its end is reported now, or once
    /// nothing more of it can come.
    fn finished(&mut self, task: &str, status: ChildState, error: Option<String>, out: &mut Vec<EngineEvent>) {
        let Some((call, c)) = self.agents.iter_mut().find(|(_, c)| c.task_id == task && c.translator.is_some()) else { return };
        if c.end.is_none() {
            c.end = Some((status, error));
        }
        if c.background || c.answered {
            let call = call.clone();
            self.end(&call, out);
        }
    }

    /// A line of the conversation's own: a tool result answering an
    /// agent's call means no line of that agent follows.
    fn answered(&mut self, msg: &Value, out: &mut Vec<EngineEvent>) {
        if msg["type"] != "user" {
            return;
        }
        let content = msg.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
        for b in content.iter().filter(|b| b["type"] == "tool_result") {
            let call = s(b, "tool_use_id");
            let Some(c) = self.agents.get_mut(call) else { continue };
            c.answered = true;
            if c.end.is_some() && !c.background {
                self.end(call, out);
            }
        }
    }

    /// A line naming `call`: an agent's own, a grandchild's, or one whose
    /// agent has not started yet.
    fn line(&mut self, call: &str, msg: Value, out: &mut Vec<EngineEvent>) {
        let Some(ancestor) = self.under.get(call).cloned() else {
            if !self.held.contains_key(call) {
                if self.waiting.len() == WAITING
                    && let Some(oldest) = self.waiting.pop_front()
                {
                    self.held.remove(&oldest);
                }
                self.waiting.push_back(call.into());
            }
            let h = self.held.entry(call.into()).or_default();
            if h.lines.len() == HOLD {
                h.lines.pop_front();
                h.dropped += 1;
            }
            h.lines.push_back(msg);
            return;
        };
        let Some(c) = self.agents.get_mut(&ancestor) else { return };
        let Some(t) = c.translator.as_mut() else { return };
        let evs = if ancestor == call { t.apply(&msg).unwrap_or_default() } else { t.aside(&msg) };
        out.extend(wrap(&c.task_id, evs));
        // The calls it makes are its own, and a line naming one is its.
        let calls: Vec<String> = msg.pointer("/message/content").and_then(Value::as_array).into_iter().flatten().filter(|b| b["type"] == "tool_use").map(|b| s(b, "id").to_string()).filter(|id| !id.is_empty()).collect();
        for id in calls {
            self.under.entry(id.clone()).or_insert_with(|| ancestor.clone());
            self.release(&id, out);
        }
    }

    /// Lines held for `call`, now that it is known: what was dropped said
    /// first, in the agent's transcript.
    fn release(&mut self, call: &str, out: &mut Vec<EngineEvent>) {
        let Some(h) = self.held.remove(call) else { return };
        self.waiting.retain(|w| w != call);
        if h.dropped > 0
            && let Some(c) = self.under.get(call).and_then(|a| self.agents.get(a))
        {
            let note = format!("{DROPPED}{} of this agent's first lines came before Claude Code said it started, more than krowk holds, and are not in this transcript.</dropped-lines>", h.dropped);
            out.extend(item(&c.task_id.clone(), Item::user(note)));
        }
        for l in h.lines {
            self.line(call, l, out);
        }
    }

    /// Reports an agent's end: what its translator still holds, then how.
    fn end(&mut self, call: &str, out: &mut Vec<EngineEvent>) {
        let Some(c) = self.agents.get_mut(call) else { return };
        let Some(mut t) = c.translator.take() else { return };
        let mut rest = Vec::new();
        t.finish(&mut rest);
        out.extend(wrap(&c.task_id, rest));
        let (status, error) = c.end.take().unwrap_or((ChildState::Interrupted, None));
        out.push(EngineEvent::BackendChildEnded { task_id: c.task_id.clone(), status, error });
    }

    /// A turn's `result` was read: its foreground agents end with it, as
    /// Claude Code said, or interrupted when it said nothing; a background
    /// one no longer listed, finished. What is left of the ended agents
    /// goes, and the lines held for an agent not known yet go too, unless
    /// a background agent runs on, whose agents' lines may still come.
    pub fn turn_ended(&mut self) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        let open: Vec<String> = self.agents.iter().filter(|(_, c)| !c.background && c.translator.is_some()).map(|(k, _)| k.clone()).collect();
        for call in open {
            if let Some(c) = self.agents.get_mut(&call) {
                c.end.get_or_insert((ChildState::Interrupted, Some("the turn that ran it ended before Claude Code said it finished".into())));
            }
            self.end(&call, &mut out);
        }
        self.unlisted(&mut out);
        let ended: Vec<String> = self.agents.iter().filter(|(_, c)| c.translator.is_none()).map(|(k, _)| k.clone()).collect();
        self.agents.retain(|_, c| c.translator.is_some());
        self.under.retain(|_, a| !ended.contains(a));
        if self.agents.is_empty() {
            self.held.clear();
            self.waiting.clear();
        }
        out
    }

    /// The process is gone, and every agent still running with it: each
    /// ends interrupted, saying `why`. Nothing of it is kept.
    pub fn gone(&mut self, why: &str) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        let open: Vec<String> = self.agents.iter().filter(|(_, c)| c.translator.is_some()).map(|(k, _)| k.clone()).collect();
        for call in open {
            if let Some(c) = self.agents.get_mut(&call) {
                c.end.get_or_insert((ChildState::Interrupted, Some(why.into())));
            }
            self.end(&call, &mut out);
        }
        *self = Children::default();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started(task: &str, call: &str, depth: u64, owned: bool) -> Value {
        let mut m = json!({"type": "system", "subtype": "task_started", "task_id": task, "tool_use_id": call, "description": format!("{task} work"), "subagent_type": "general-purpose", "is_backgrounded": false, "spawn_depth": depth, "task_type": "local_agent", "prompt": format!("do {task}")});
        if owned {
            m["owned_by_subagent"] = json!(true);
        }
        m
    }

    fn says(call: &str, id: &str, content: Value) -> Value {
        json!({"type": "assistant", "parent_tool_use_id": call, "message": {"id": id, "model": "m", "content": [content], "usage": {"input_tokens": 3, "output_tokens": 5}}})
    }

    fn result(call: &str, of: &str, text: &str) -> Value {
        json!({"type": "user", "parent_tool_use_id": call, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": of, "content": text}]}})
    }

    fn feed(c: &mut Children, lines: &[Value]) -> Vec<EngineEvent> {
        lines.iter().flat_map(|l| c.apply(l)).collect()
    }

    /// Each agent's items, in order, by task id.
    fn items(evs: &[EngineEvent], task: &str) -> Vec<Item> {
        evs.iter()
            .filter_map(|e| match e {
                EngineEvent::BackendChild { task_id, event } if task_id == task => match &**event {
                    EngineEvent::ItemCompleted { item, .. } => Some(item.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn r_sub_7_a_grandchilds_calls_are_its_depth_1_ancestors_and_it_is_no_child_of_its_own() {
        let mut c = Children::default();
        let evs = feed(
            &mut c,
            &[
                started("a1", "call_a", 1, false),
                says("call_a", "m1", json!({"type": "tool_use", "id": "call_g", "name": "Agent", "input": {"prompt": "go deeper"}})),
                started("g1", "call_g", 2, true),
                says("call_g", "m2", json!({"type": "text", "text": "the grandchild's words"})),
                // The ancestor's message goes on after a grandchild's line:
                // still one response.
                says("call_a", "m1", json!({"type": "text", "text": "waiting on it"})),
                says("call_g", "m3", json!({"type": "tool_use", "id": "call_g_bash", "name": "Bash", "input": {"command": "ls"}})),
                result("call_g", "call_g_bash", "a.txt"),
                result("call_a", "call_g", "deeper done"),
                says("call_a", "m4", json!({"type": "text", "text": "all done"})),
                json!({"type": "system", "subtype": "task_notification", "task_id": "g1", "status": "completed"}),
                json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "status": "completed"}),
                json!({"type": "user", "parent_tool_use_id": null, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_a", "content": "ok"}]}}),
            ],
        );
        let starts: Vec<&str> = evs.iter().filter_map(|e| if let EngineEvent::BackendChildStarted { task_id, .. } = e { Some(task_id.as_str()) } else { None }).collect();
        assert_eq!(starts, ["a1"], "one child: the grandchild is its");
        let got = items(&evs, "a1");
        let names: Vec<String> = got
            .iter()
            .map(|i| match i {
                Item::UserText { text, .. } => format!("user {text}"),
                Item::ToolCall { name, call_id, .. } => format!("call {name} {call_id}"),
                Item::ToolResult { call_id, output, .. } => format!("result {call_id} {output}"),
                Item::AssistantText { text } => format!("text {text}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(names, ["user do a1", "call Agent call_g", "text waiting on it", "call Bash call_g_bash", "result call_g_bash a.txt", "result call_g deeper done", "text all done"]);
        let responses: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                EngineEvent::BackendChild { event, .. } => match &**event {
                    EngineEvent::ResponseCompleted { response_id, item_ids, .. } => Some(format!("{} {}", response_id.as_deref().unwrap_or_default(), item_ids.len())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(responses, ["m1 2", "m4 1"], "the ancestor's own responses, each once, none for the grandchild");
        assert!(matches!(evs.last(), Some(EngineEvent::BackendChildEnded { task_id, status: ChildState::Done, error: None }) if task_id == "a1"));
        assert_eq!(evs.iter().filter(|e| matches!(e, EngineEvent::BackendChildEnded { .. })).count(), 1);
    }

    #[test]
    fn r_sub_7_at_most_256_lines_are_held_before_task_started_and_the_drop_is_said_after_the_prompt() {
        let mut c = Children::default();
        let early: Vec<Value> = (0..HOLD + 3).map(|i| says("call_a", &format!("m{i}"), json!({"type": "text", "text": format!("line {i}")}))).collect();
        assert!(feed(&mut c, &early).is_empty(), "held, nothing said yet");
        let evs = c.apply(&started("a1", "call_a", 1, false));
        assert!(matches!(&evs[0], EngineEvent::BackendChildStarted { task_id, prompt, .. } if task_id == "a1" && prompt == "do a1"));
        let got = items(&evs, "a1");
        assert_eq!(got[0], Item::user("do a1"), "the prompt first");
        assert!(matches!(&got[1], Item::UserText { text, .. } if text.starts_with(DROPPED) && text.contains("3 of this agent's first lines")), "{:?}", got[1]);
        assert_eq!(got[2], Item::AssistantText { text: "line 3".into() }, "the oldest dropped");
        assert_eq!((got.len(), got.last()), (2 + HOLD, Some(&Item::AssistantText { text: format!("line {}", HOLD + 2) })));
        // Each response is the agent's, with no usage: metering is the session's.
        assert!(evs.iter().all(|e| !matches!(e, EngineEvent::BackendChild { event, .. } if matches!(&**event, EngineEvent::ResponseCompleted { usage, .. } if *usage != Usage::default()))));
        // The process let go: it ends interrupted, saying why.
        let gone = c.gone("Claude Code was let go");
        assert!(matches!(gone.last(), Some(EngineEvent::BackendChildEnded { status: ChildState::Interrupted, error: Some(e), .. }) if e == "Claude Code was let go"));
        assert!(matches!(&gone[0], EngineEvent::BackendChild { event, .. } if matches!(&**event, EngineEvent::ResponseCompleted { response_id: Some(r), .. } if *r == format!("m{}", HOLD + 2))), "the last response closed first: {gone:?}");
    }

    #[test]
    fn r_sub_7_at_most_32_agents_lines_are_held_and_a_turns_end_lets_them_go_unless_a_background_agent_runs() {
        let mut c = Children::default();
        // An older binary: agents' lines, and no task_started for any.
        let lines: Vec<Value> = (0..WAITING + 2).map(|i| says(&format!("call_{i}"), &format!("m{i}"), json!({"type": "text", "text": "x"}))).collect();
        assert!(feed(&mut c, &lines).is_empty());
        assert_eq!((c.held.len(), c.waiting.len()), (WAITING, WAITING));
        assert!(!c.held.contains_key("call_0") && !c.held.contains_key("call_1") && c.held.contains_key("call_2"), "the agents held longest went, whole");
        // Started late, an evicted agent gets nothing held; a kept one does.
        assert!(items(&c.apply(&started("a0", "call_0", 1, false)), "a0").len() == 1);
        assert_eq!(items(&c.apply(&started("a2", "call_2", 1, false)), "a2").len(), 2);
        // The turn ends: its agents end, what is left of them goes, and so
        // does every line still held.
        let ended = c.turn_ended();
        assert_eq!(ended.iter().filter(|e| matches!(e, EngineEvent::BackendChildEnded { .. })).count(), 2);
        assert!(c.held.is_empty() && c.waiting.is_empty() && c.agents.is_empty() && c.under.is_empty());
        // With a background agent running on, its lines may still come.
        let mut bg = started("b1", "call_b", 1, false);
        bg["is_backgrounded"] = json!(true);
        c.apply(&bg);
        c.apply(&says("call_x", "mx", json!({"type": "text", "text": "x"})));
        assert!(c.turn_ended().is_empty());
        assert_eq!((c.held.len(), c.agents.len()), (1, 1));
    }

    #[test]
    fn r_sub_7_a_background_agent_claude_code_stops_listing_ends_with_the_status_it_says_or_as_finished() {
        let mut c = Children::default();
        let mut bg = started("b1", "call_b", 1, false);
        bg["is_backgrounded"] = json!(true);
        let unlisted = json!({"type": "system", "subtype": "background_tasks_changed", "tasks": []});
        // As recorded: off the list, then its status.
        let evs = feed(&mut c, &[bg.clone(), unlisted.clone()]);
        assert!(!evs.iter().any(|e| matches!(e, EngineEvent::BackendChildEnded { .. })), "it waits for the word on how");
        let evs = c.apply(&json!({"type": "system", "subtype": "task_updated", "task_id": "b1", "patch": {"status": "failed", "error": "boom"}}));
        assert!(matches!(evs.last(), Some(EngineEvent::BackendChildEnded { status: ChildState::Failed, error: Some(e), .. }) if e == "boom"));
        // Never told: done once Claude Code goes on with the conversation.
        let mut c = Children::default();
        bg["task_id"] = json!("b2");
        feed(&mut c, &[bg, json!({"type": "system", "subtype": "background_tasks_changed", "tasks": [{"task_id": "b2"}]})]);
        assert!(feed(&mut c, &[unlisted]).is_empty());
        let evs = c.apply(&json!({"type": "system", "subtype": "init"}));
        assert!(matches!(evs.last(), Some(EngineEvent::BackendChildEnded { task_id, status: ChildState::Done, error: None }) if task_id == "b2"));
    }
}
