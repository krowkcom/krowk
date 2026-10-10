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
//! prompt and a note of what was dropped.
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
//! `killed` and `stopped` interrupted, anything else failed. A foreground
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
    /// Lines naming a call no agent is known by yet.
    held: HashMap<String, Held>,
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
                self.agents.insert(call.into(), Child { task_id: task.into(), translator: Some(Translator::child()), background: msg["is_backgrounded"] == true, answered: false, end: None });
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
            _ => {}
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
        if h.dropped > 0
            && let Some(c) = self.under.get(call).and_then(|a| self.agents.get(a))
        {
            let note = format!("{}{} of this agent's first lines came before Claude Code said it started, more than krowk holds, and are not in this transcript.</unprompted>", super::UNPROMPTED, h.dropped);
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
    /// Claude Code said, or interrupted when it said nothing.
    pub fn turn_ended(&mut self) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        let open: Vec<String> = self.agents.iter().filter(|(_, c)| !c.background && c.translator.is_some()).map(|(k, _)| k.clone()).collect();
        for call in open {
            if let Some(c) = self.agents.get_mut(&call) {
                c.end.get_or_insert((ChildState::Interrupted, Some("the turn that ran it ended before Claude Code said it finished".into())));
            }
            self.end(&call, &mut out);
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
        assert_eq!(names, ["user do a1", "call Agent call_g", "call Bash call_g_bash", "result call_g_bash a.txt", "result call_g deeper done", "text all done"]);
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
        assert!(matches!(&got[1], Item::UserText { text, .. } if text.starts_with(super::super::UNPROMPTED) && text.contains("3 of this agent's first lines")), "{:?}", got[1]);
        assert_eq!(got[2], Item::AssistantText { text: "line 3".into() }, "the oldest dropped");
        assert_eq!((got.len(), got.last()), (2 + HOLD, Some(&Item::AssistantText { text: format!("line {}", HOLD + 2) })));
        // Each response is the agent's, with no usage: metering is the session's.
        assert!(evs.iter().all(|e| !matches!(e, EngineEvent::BackendChild { event, .. } if matches!(&**event, EngineEvent::ResponseCompleted { usage, .. } if *usage != Usage::default()))));
        // The process let go: it ends interrupted, saying why.
        let gone = c.gone("Claude Code was let go");
        assert!(matches!(gone.last(), Some(EngineEvent::BackendChildEnded { status: ChildState::Interrupted, error: Some(e), .. }) if e == "Claude Code was let go"));
        assert!(matches!(&gone[0], EngineEvent::BackendChild { event, .. } if matches!(&**event, EngineEvent::ResponseCompleted { response_id: Some(r), .. } if *r == format!("m{}", HOLD + 2))), "the last response closed first: {gone:?}");
    }
}
