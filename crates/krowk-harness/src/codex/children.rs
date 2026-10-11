//! Codex's child threads as children of the session (R-SUB-7): each direct
//! child of the session's thread is reported as events of its own, which
//! the host writes to a child session — `BackendChildStarted` (its
//! `task_id` the thread id), the spawn's prompt as its first item
//! (R-SUB-8), each of its items, and `BackendChildEnded`.
//!
//! **Checked against the schema, not live.** Everything here is read off
//! the schema the pinned Codex (`schema/codex/`, 0.154.0) generates, and
//! tested against a mock app-server fed messages written from it
//! (`tests/fixtures/codex/subagents.jsonl`); nobody has run it against a
//! real Codex, so the order the messages come in is the schema's guess, not
//! a recording's, and the rules below take them in any order.
//!
//! **A direct child** is a thread the session's thread spawned: one its
//! `collabAgentToolCall` `spawnAgent` names (`senderThreadId` the session's
//! thread, `receiverThreadIds` the child), one whose `thread/started` names
//! it as the spawner (`source.subAgent.thread_spawn.parent_thread_id`, at
//! depth 1), or one its `subAgentActivity` item names (`agentThreadId`)
//! whose `agentPath` is one segment under the root's (`/root/<name>`; a
//! deeper path is a grandchild's). It starts at the first of these. Its prompt is the spawn's: the
//! `spawnAgent` naming it, else the one spawn of the session's still in
//! progress, which names its thread only once it is done; else the child's
//! own first `userMessage`, the input Codex gave it. Its description is
//! the nickname Codex gave it, else its role, else its prompt's first line;
//! its agent its role (`agent_role`).
//!
//! **Its lines** — every notification naming its `threadId` — are
//! translated by a `Translator` of its own, so parallel children never
//! mix; a tool starts at its call item (`item/started`), which the
//! translator completes at once, and ends at its result. Its responses
//! carry no usage — its calls are metered once, as the session's
//! `subagent.response` (`stream::SubThreads`) — and its
//! `thread/tokenUsage/updated` total is its `Progress`, the tokens on its
//! `subagent.status` (R-SUB-9). A `userMessage` after the first is what
//! the parent sent it since, its `userText`.
//!
//! **A line before its start** is held, per thread, at most `HOLD` lines a
//! thread (the oldest dropped) and `WAITING` threads (the one held longest
//! going whole, its count kept), until the thread is known a child —
//! released after its prompt, and what was dropped of it said there — or
//! known none, or the turn ends.
//!
//! **A grandchild** — a thread a child spawns, or any other thread Codex
//! starts (a review, a compaction, depth above 1) — is no child of the
//! session: its spawn is a `spawn_agent` call in its parent child's
//! transcript, and its own lines are not followed.
//!
//! **Its end** is the session's `subAgentActivity` `completed` (done) or
//! `interrupted`, or a collab call of the session's (a `wait`, say) whose
//! `agentsStates` says it ended: `completed` done, `interrupted` and
//! `shutdown` interrupted, `errored` and `notFound` failed — whichever
//! comes first. krowk reads Codex only while a turn runs, and sends Codex
//! nothing for a child at the turn's end: a child the parent's turn did not
//! see end is no longer followed from there — ended in its log as
//! interrupted, saying so (`UNFINISHED`) — though Codex may still be
//! running it. What it does later is not shown; its spend still is, as the
//! session's, whenever a later turn reads it. A later turn that spawns,
//! messages or resumes it again (a `spawnAgent`, `sendInput`,
//! `followupTask` or `resumeAgent` naming it, a `subAgentActivity`
//! `interacted`) does not reopen it: its child session has ended, and its
//! lines are dropped. Reopening one would need a second turn in a child
//! log the host has closed, which no backend does yet.
//!
//! **Its requests** — an approval or a question whose `threadId` is an open
//! child's — are asked under the child's session and answered on its
//! thread (`lend`); a publish's notice and the run it opens stay the
//! session's (`super::as_childs`). **Stopping it** (R-SUB-10) is
//! `turn/interrupt` with its thread and the turn it runs: the last `turnId`
//! its own lines named, none once its own `turn/completed` said that turn
//! is over (`turn_of`). A child lent to answer its own request still runs,
//! and is stopped the same way, its question taken back. Once krowk has
//! sent that, its own `turn/completed` `interrupted` ends it interrupted,
//! unless the session said it ended first.

use super::stream::Translator;
use crate::engine::EngineEvent;
use crate::protocol::{ChildState, Item, ItemKind, Usage};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

/// How many lines of one thread are held before it is known a child.
pub const HOLD: usize = 256;
/// How many threads' lines are held at once: past it the one held longest
/// goes, whole.
pub const WAITING: usize = 32;

/// Why a child still open at its parent's turn's end is no longer followed.
pub const UNFINISHED: &str = "krowk stopped following it when the turn that ran it ended, before Codex said it finished — Codex may still be running it; what it does after this is not shown here, and its spend is still counted";

/// How many evicted threads' dropped lines are remembered.
const LOST: usize = 256;

/// The note in a child's transcript where lines held for it were dropped:
/// krowk's words, not the child's.
pub const DROPPED: &str = "<dropped-lines>";

struct Child {
    /// None once it ended — a line of it after that is dropped — or while
    /// it is lent to answer one of its requests (`lend`).
    translator: Option<Translator>,
    /// It ended: reported once, and nothing of it is stopped or lent.
    ended: bool,
    /// Its prompt is in its log.
    prompted: bool,
    /// Its first `userMessage` was seen: the prompt Codex gave it.
    echoed: bool,
    /// The Codex turn it runs: the last its lines named, none once that
    /// turn completed.
    turn: Option<String>,
    /// A turn of it completed: with none running, it has none to stop.
    idle: bool,
    /// krowk sent it `turn/interrupt`.
    stopping: bool,
}

/// What `thread/started` said of a thread.
#[derive(Default, Clone)]
struct Said {
    nickname: Option<String>,
    role: Option<String>,
    model: Option<String>,
}

#[derive(Default)]
struct Held {
    lines: VecDeque<(String, Value)>,
    dropped: usize,
}

/// The session's child threads, by thread id, for as long as its process
/// lives.
#[derive(Default)]
pub struct Children {
    kids: HashMap<String, Child>,
    /// Threads that are no child of the session: their lines are dropped.
    others: HashSet<String>,
    said: HashMap<String, Said>,
    /// The session's `spawnAgent` calls not done yet, which name no thread
    /// yet: their ids and prompts.
    spawning: Vec<(String, String)>,
    held: HashMap<String, Held>,
    waiting: VecDeque<String>,
    /// Threads whose held lines went whole past `WAITING`: how many, said
    /// if the thread starts after all. At most `LOST`, the oldest going.
    lost: VecDeque<(String, usize)>,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn opt(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from)
}

fn receivers(item: &Value) -> Vec<String> {
    item.get("receiverThreadIds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).filter(|t| !t.is_empty()).map(String::from).collect()).unwrap_or_default()
}

/// A `CollabAgentStatus` as a child's end, when it is one.
fn ended(status: &str) -> Option<ChildState> {
    match status {
        "completed" => Some(ChildState::Done),
        "interrupted" | "shutdown" => Some(ChildState::Interrupted),
        "errored" | "notFound" => Some(ChildState::Failed),
        _ => None,
    }
}

fn item(thread: &str, item: Item) -> [EngineEvent; 2] {
    let item_id = krowk_store::new_id();
    [
        EngineEvent::BackendChild { task_id: thread.into(), event: Box::new(EngineEvent::ItemStarted { item_id: item_id.clone(), kind: ItemKind::UserText }) },
        EngineEvent::BackendChild { task_id: thread.into(), event: Box::new(EngineEvent::ItemCompleted { item_id, item }) },
    ]
}

fn wrap(thread: &str, evs: Vec<EngineEvent>, out: &mut Vec<EngineEvent>) {
    out.extend(evs.into_iter().map(|ev| {
        // Its spend is the session's `SubagentResponse`, counted once.
        let ev = match ev {
            EngineEvent::ResponseCompleted { response_id, model, stop_reason, item_ids, .. } => EngineEvent::ResponseCompleted { response_id, model, usage: Usage::default(), stop_reason, item_ids },
            ev => ev,
        };
        EngineEvent::BackendChild { task_id: thread.into(), event: Box::new(ev) }
    }));
}

/// A `userMessage`'s text: its text inputs, one per line.
fn user_text(item: &Value) -> String {
    item.get("content").and_then(Value::as_array).map(|a| a.iter().filter(|c| c["type"] == "text").filter_map(|c| c.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")).unwrap_or_default()
}

/// A title for a child Codex gave no nickname nor role: its prompt's first
/// line, cut short.
fn title(prompt: Option<&str>) -> String {
    let line = prompt.and_then(|p| p.lines().find(|l| !l.trim().is_empty())).unwrap_or("Codex agent").trim();
    match line.char_indices().nth(60) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

impl Children {
    /// Folds one notification of Codex's in, `own` being the session's
    /// thread and `model` the session's model; returns the children's
    /// events it makes.
    pub fn apply(&mut self, own: &str, model: &str, method: &str, params: &Value) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        if method == "thread/started" {
            self.thread_started(own, model, params.get("thread").unwrap_or(&Value::Null), &mut out);
            return out;
        }
        let thread = s(params, "threadId");
        if thread.is_empty() || thread == own {
            if matches!(method, "item/started" | "item/completed") {
                self.own_item(own, model, method == "item/completed", params.get("item").unwrap_or(&Value::Null), &mut out);
            }
            return out;
        }
        self.line(thread, method, params, &mut out);
        out
    }

    fn thread_started(&mut self, own: &str, model: &str, th: &Value, out: &mut Vec<EngineEvent>) {
        let id = s(th, "id");
        if id.is_empty() || id == own || self.kids.contains_key(id) {
            return;
        }
        let spawn = th.pointer("/source/subAgent/thread_spawn");
        let said = Said {
            nickname: opt(th.get("agentNickname")).or_else(|| opt(spawn.and_then(|s| s.get("agent_nickname")))),
            role: opt(th.get("agentRole")).or_else(|| opt(spawn.and_then(|s| s.get("agent_role")))),
            model: opt(th.get("model")),
        };
        match spawn {
            Some(sp) if s(sp, "parent_thread_id") == own && sp.get("depth").and_then(Value::as_i64).is_none_or(|d| d <= 1) => {
                self.said.insert(id.into(), said);
                self.start(id, None, model, out);
            }
            _ => self.not_a_child(id),
        }
    }

    /// An item of the session's own thread: a spawn, an agent's activity,
    /// or a collab call saying where its agents stand.
    fn own_item(&mut self, own: &str, model: &str, completed: bool, it: &Value, out: &mut Vec<EngineEvent>) {
        match s(it, "type") {
            "collabAgentToolCall" if s(it, "senderThreadId").is_empty() || s(it, "senderThreadId") == own => {
                let id = s(it, "id");
                let to = receivers(it);
                if s(it, "tool") == "spawnAgent" {
                    let prompt = opt(it.get("prompt"));
                    if to.is_empty() && !completed && !self.spawning.iter().any(|(i, _)| i == id) {
                        self.spawning.push((id.into(), prompt.clone().unwrap_or_default()));
                    }
                    if !to.is_empty() || completed {
                        self.spawning.retain(|(i, _)| i != id);
                    }
                    for th in &to {
                        self.others.remove(th);
                        self.start(th, Some((id, prompt.as_deref())), model, out);
                    }
                }
                if completed {
                    let states: Vec<(String, ChildState, Option<String>)> = it
                        .get("agentsStates")
                        .and_then(Value::as_object)
                        .into_iter()
                        .flatten()
                        .filter_map(|(th, st)| ended(s(st, "status")).map(|e| (th.clone(), e, opt(st.get("message")).filter(|_| e != ChildState::Done))))
                        .collect();
                    for (th, status, error) in states {
                        self.end(&th, status, error, out);
                    }
                }
            }
            "subAgentActivity" => {
                let th = s(it, "agentThreadId");
                if th.is_empty() || self.others.contains(th) {
                    return;
                }
                // Only a path one segment under the root's names a direct
                // child: a deeper one is a grandchild's, never started here.
                let direct = s(it, "agentPath").split('/').filter(|p| !p.is_empty()).count() == 2;
                let start = |c: &mut Children, out: &mut Vec<EngineEvent>| {
                    if direct || c.kids.contains_key(th) {
                        c.start(th, None, model, out);
                    }
                };
                match s(it, "kind") {
                    "started" | "interacted" => start(self, out),
                    kind @ ("completed" | "interrupted") => {
                        start(self, out);
                        self.end(th, if kind == "completed" { ChildState::Done } else { ChildState::Interrupted }, None, out);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// A thread known to be no child of the session: what is held of it,
    /// and what comes later, goes.
    fn not_a_child(&mut self, thread: &str) {
        if !self.kids.contains_key(thread) {
            self.others.insert(thread.into());
            self.held.remove(thread);
            self.waiting.retain(|w| w != thread);
            self.lost.retain(|(t, _)| t != thread);
        }
    }

    /// Starts `thread` as a child, once: by the spawn `(call, prompt)` that
    /// names it, else the one spawn in progress, else with no prompt yet. A
    /// child started with none takes the spawn's that names it later.
    fn start(&mut self, thread: &str, spawn: Option<(&str, Option<&str>)>, model: &str, out: &mut Vec<EngineEvent>) {
        if let Some(c) = self.kids.get_mut(thread) {
            if let Some((_, Some(p))) = spawn
                && !c.prompted
                && !c.ended
            {
                c.prompted = true;
                out.extend(item(thread, Item::user(p)));
            }
            return;
        }
        let (call, prompt) = match spawn {
            Some((call, prompt)) => (call.to_string(), prompt.map(String::from)),
            None => match self.spawning.as_slice() {
                [(call, prompt)] => {
                    let got = (call.clone(), Some(prompt.clone()));
                    self.spawning.clear();
                    got
                }
                _ => (String::new(), None),
            },
        };
        let prompt = prompt.filter(|p| !p.is_empty());
        let said = self.said.remove(thread).unwrap_or_default();
        let description = said.nickname.clone().or_else(|| said.role.clone()).unwrap_or_else(|| title(prompt.as_deref()));
        out.push(EngineEvent::BackendChildStarted { task_id: thread.into(), call_id: call, description, agent: said.role, prompt: prompt.clone().unwrap_or_default(), background: false });
        if let Some(p) = &prompt {
            out.extend(item(thread, Item::user(p.clone())));
        }
        self.kids.insert(thread.into(), Child { translator: Some(Translator::child(said.model.as_deref().unwrap_or(model))), prompted: prompt.is_some(), echoed: false, ended: false, turn: None, idle: false, stopping: false });
        self.release(thread, out);
    }

    /// A notification naming `thread`: a child's, or one of a thread not
    /// known yet, held.
    fn line(&mut self, thread: &str, method: &str, params: &Value, out: &mut Vec<EngineEvent>) {
        if self.others.contains(thread) {
            return;
        }
        if !self.kids.contains_key(thread) {
            if !self.held.contains_key(thread) {
                if self.waiting.len() == WAITING
                    && let Some(oldest) = self.waiting.pop_front()
                    && let Some(h) = self.held.remove(&oldest)
                {
                    if self.lost.len() == LOST {
                        self.lost.pop_front();
                    }
                    self.lost.push_back((oldest, h.lines.len() + h.dropped));
                }
                self.waiting.push_back(thread.into());
            }
            let h = self.held.entry(thread.into()).or_default();
            if h.lines.len() == HOLD {
                h.lines.pop_front();
                h.dropped += 1;
            }
            h.lines.push_back((method.into(), params.clone()));
            return;
        }
        let it = params.get("item").unwrap_or(&Value::Null);
        // The threads its own spawns name are its, not the session's.
        if s(it, "type") == "collabAgentToolCall" {
            for th in receivers(it) {
                self.not_a_child(&th);
            }
        }
        let Some(c) = self.kids.get_mut(thread) else { return };
        // The turn it runs, kept whatever else happens to the line: its own
        // turn's end leaves it none to stop.
        let finished = method == "turn/completed";
        let named = if finished { None } else { opt(params.get("turnId")).or_else(|| opt(params.pointer("/turn/id")).filter(|_| method == "turn/started")) };
        if finished {
            c.turn = None;
            c.idle = true;
        } else if let Some(turn) = named {
            c.turn = Some(turn);
        }
        if c.ended {
            return;
        }
        // Stopped by krowk: its turn's end is its own.
        if finished && c.stopping && params.pointer("/turn/status").and_then(Value::as_str) == Some("interrupted") {
            self.end(thread, ChildState::Interrupted, None, out);
            return;
        }
        let Some(c) = self.kids.get_mut(thread) else { return };
        let Some(t) = c.translator.as_mut() else { return };
        if s(it, "type") == "userMessage" {
            if method == "item/completed" {
                let first = !std::mem::replace(&mut c.echoed, true);
                // Its first is the prompt, in its log already when a spawn
                // said it; every later one is what the parent sent it since.
                if !(first && c.prompted) {
                    c.prompted = true;
                    let text = user_text(it);
                    if !text.is_empty() {
                        out.extend(item(thread, Item::user(text)));
                    }
                }
            }
            return;
        }
        let evs = t.apply(method, params);
        wrap(thread, evs, out);
        if method == "thread/tokenUsage/updated"
            && let Some(tokens) = params.pointer("/tokenUsage/total/totalTokens").and_then(Value::as_i64)
        {
            out.push(EngineEvent::BackendChild { task_id: thread.into(), event: Box::new(EngineEvent::Progress { tokens }) });
        }
    }

    /// Lines held for `thread`, now that it is a child: what was dropped
    /// said first, in its transcript.
    fn release(&mut self, thread: &str, out: &mut Vec<EngineEvent>) {
        let lost: usize = self.lost.iter().filter(|(t, _)| t == thread).map(|(_, n)| n).sum();
        self.lost.retain(|(t, _)| t != thread);
        let h = self.held.remove(thread).unwrap_or_default();
        self.waiting.retain(|w| w != thread);
        let dropped = h.dropped + lost;
        if dropped > 0 {
            let note = format!("{DROPPED}{} of this agent's first lines came before Codex said it started, more than krowk holds, and are not in this transcript.</dropped-lines>", dropped);
            out.extend(item(thread, Item::user(note)));
        }
        for (method, params) in h.lines {
            self.line(thread, &method, &params, out);
        }
    }

    /// Reports a child's end, once: what its translator still holds, then how.
    fn end(&mut self, thread: &str, status: ChildState, error: Option<String>, out: &mut Vec<EngineEvent>) {
        let Some(c) = self.kids.get_mut(thread).filter(|c| !c.ended) else { return };
        c.ended = true;
        if let Some(mut t) = c.translator.take() {
            let mut rest = Vec::new();
            t.finish(&mut rest);
            wrap(thread, rest, out);
        }
        out.push(EngineEvent::BackendChildEnded { task_id: thread.into(), status, error });
    }

    /// Lends an open child's translator for a request of its thread, so the
    /// request is asked as the child's and its answer read off the child's
    /// items; given back with `give_back`. None for any other thread.
    pub fn lend(&mut self, thread: &str) -> Option<Translator> {
        self.kids.get_mut(thread).filter(|c| !c.ended).and_then(|c| c.translator.take())
    }

    pub fn give_back(&mut self, thread: &str, t: Translator) {
        if let Some(c) = self.kids.get_mut(thread).filter(|c| !c.ended) {
            c.translator = Some(t);
        }
    }

    /// The turn to interrupt to stop `thread` alone (R-SUB-10), or why
    /// there is none.
    pub fn turn_of(&self, thread: &str) -> Result<String, String> {
        match self.kids.get(thread) {
            None => Err("krowk does not follow that agent in this turn".into()),
            Some(c) if c.ended => Err("it has ended".into()),
            Some(Child { turn: None, idle: true, .. }) => Err("no turn of it is running — Codex said its last one completed, so there is nothing to interrupt".into()),
            Some(Child { turn: None, .. }) => Err("Codex has not said which turn it runs yet — try again in a moment, or interrupt the session that started it".into()),
            Some(Child { turn: Some(t), .. }) => Ok(t.clone()),
        }
    }

    /// krowk sent `thread` its `turn/interrupt`.
    pub fn stopping(&mut self, thread: &str) {
        if let Some(c) = self.kids.get_mut(thread) {
            c.stopping = true;
        }
    }

    /// The parent's turn is over: every child still open ends, interrupted,
    /// saying `why`; what is held goes. The ended stay known, so a late
    /// line of theirs is dropped.
    pub fn turn_ended(&mut self, why: &str) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        let mut open: Vec<String> = self.kids.iter().filter(|(_, c)| !c.ended).map(|(k, _)| k.clone()).collect();
        open.sort();
        for th in open {
            self.end(&th, ChildState::Interrupted, Some(why.into()), &mut out);
        }
        self.held.clear();
        self.waiting.clear();
        self.spawning.clear();
        self.said.clear();
        self.lost.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWN: &str = "own";

    fn spawn(id: &str, sender: &str, to: &[&str], prompt: &str) -> Value {
        json!({"type": "collabAgentToolCall", "id": id, "tool": "spawnAgent", "senderThreadId": sender, "receiverThreadIds": to, "prompt": prompt, "status": if to.is_empty() { "inProgress" } else { "completed" }, "agentsStates": {}})
    }

    fn note(method: &str, thread: &str, item: Value) -> (String, Value) {
        (method.into(), json!({"threadId": thread, "turnId": "t", "item": item}))
    }

    fn started(id: &str, parent: &str, depth: i64) -> (String, Value) {
        ("thread/started".into(), json!({"thread": {"id": id, "agentNickname": format!("nick-{id}"), "agentRole": "explorer", "source": {"subAgent": {"thread_spawn": {"parent_thread_id": parent, "depth": depth}}}}}))
    }

    fn feed(c: &mut Children, lines: &[(String, Value)]) -> Vec<EngineEvent> {
        lines.iter().flat_map(|(m, p)| c.apply(OWN, "gpt-5.5", m, p)).collect()
    }

    fn items(evs: &[EngineEvent], thread: &str) -> Vec<Item> {
        evs.iter()
            .filter_map(|e| match e {
                EngineEvent::BackendChild { task_id, event } if task_id == thread => match &**event {
                    EngineEvent::ItemCompleted { item, .. } => Some(item.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn ends(evs: &[EngineEvent]) -> Vec<(&str, ChildState)> {
        evs.iter().filter_map(|e| if let EngineEvent::BackendChildEnded { task_id, status, .. } = e { Some((task_id.as_str(), *status)) } else { None }).collect()
    }

    /// Whether some response of a child's carries usage.
    fn spent(evs: &[EngineEvent]) -> bool {
        evs.iter().any(|e| matches!(e, EngineEvent::BackendChild { event, .. } if matches!(&**event, EngineEvent::ResponseCompleted { usage, .. } if *usage != Usage::default())))
    }

    fn starts(evs: &[EngineEvent]) -> Vec<(String, String, String)> {
        evs.iter().filter_map(|e| if let EngineEvent::BackendChildStarted { task_id, call_id, description, .. } = e { Some((task_id.clone(), call_id.clone(), description.clone())) } else { None }).collect()
    }

    /// Two spawns of the session's thread, one before Codex names its
    /// thread, one after its lines; a grandchild; a late line.
    fn two_children() -> Vec<(String, Value)> {
        vec![
            note("item/started", OWN, spawn("call-1", OWN, &[], "count the files")),
            started("kid-1", OWN, 1),
            note("item/started", "kid-1", json!({"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "count the files"}]})),
            note("item/completed", "kid-1", json!({"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "count the files"}]})),
            note("item/completed", OWN, spawn("call-1", OWN, &["kid-1"], "count the files")),
            // The second's lines come before anything names it: held.
            note("item/started", "kid-2", json!({"type": "agentMessage", "id": "a2", "text": ""})),
            note("item/completed", "kid-2", json!({"type": "agentMessage", "id": "a2", "text": "reading"})),
            note("item/completed", OWN, spawn("call-2", OWN, &["kid-2"], "read the README")),
            note("item/started", "kid-1", json!({"type": "commandExecution", "id": "c1", "command": "ls", "cwd": "/", "commandActions": [], "status": "inProgress"})),
            // kid-2 spawns its own agent: in its transcript, no child.
            note("item/started", "kid-2", spawn("call-g", "kid-2", &[], "go deeper")),
            started("grand-1", "kid-2", 2),
            note("item/completed", "kid-2", spawn("call-g", "kid-2", &["grand-1"], "go deeper")),
            note("item/completed", "grand-1", json!({"type": "agentMessage", "id": "g1", "text": "deep"})),
            note("item/completed", "kid-1", json!({"type": "commandExecution", "id": "c1", "command": "ls", "cwd": "/", "commandActions": [], "status": "completed", "exitCode": 0, "aggregatedOutput": "a\nb\n"})),
            ("thread/tokenUsage/updated".into(), json!({"threadId": "kid-1", "turnId": "t", "tokenUsage": {"last": {"inputTokens": 40, "outputTokens": 2, "totalTokens": 42}, "total": {"totalTokens": 42}}})),
            note("item/completed", OWN, json!({"type": "subAgentActivity", "id": "s1", "kind": "completed", "agentThreadId": "kid-1", "agentPath": "/root/kid-1"})),
            // Its late line is dropped, not held.
            note("item/completed", "kid-1", json!({"type": "agentMessage", "id": "late", "text": "late"})),
        ]
    }

    /// R-SUB-7, R-SUB-8: two spawns of the session's thread are two
    /// children, each its prompt first — the one spawn in progress gives
    /// its prompt to the thread Codex starts for it before the spawn names
    /// it — its items its own, its responses without usage, and its end
    /// as Codex's `subAgentActivity` says; a grandchild is no child.
    #[test]
    fn r_sub_7_codexs_direct_child_threads_are_children_their_prompt_first_and_a_grandchild_is_not() {
        let mut c = Children::default();
        let evs = feed(&mut c, &two_children());
        assert_eq!(starts(&evs), [("kid-1".to_string(), "call-1".to_string(), "nick-kid-1".to_string()), ("kid-2".into(), "call-2".into(), "read the README".into())], "the spawn named by the thread it started; a grandchild is no child");
        let one = items(&evs, "kid-1");
        assert_eq!(one[0], Item::user("count the files"), "the prompt first, once: {one:?}");
        assert!(matches!(&one[1], Item::ToolCall { name, .. } if name == "shell"));
        assert!(matches!(&one[2], Item::ToolResult { output, .. } if output == "a\nb\n"));
        assert_eq!(one.len(), 3, "{one:?}");
        let two = items(&evs, "kid-2");
        assert_eq!(two[0], Item::user("read the README"));
        assert_eq!(two[1], Item::AssistantText { text: "reading".into() }, "held, then after its prompt");
        assert!(matches!(&two[2], Item::ToolCall { name, input, .. } if name == "spawn_agent" && input["prompt"] == "go deeper"), "its own spawn is its tool call: {two:?}");
        assert!(matches!(&two[3], Item::ToolResult { output, .. } if output == "agents: grand-1"));
        assert!(items(&evs, "grand-1").is_empty());
        assert!(evs.iter().any(|e| matches!(e, EngineEvent::BackendChild { task_id, event } if task_id == "kid-1" && matches!(**event, EngineEvent::Progress { tokens: 42 }))));
        assert!(!spent(&evs), "its spend is the session's");
        assert_eq!(ends(&evs), [("kid-1", ChildState::Done)]);
    }

    /// R-SUB-7: a child the parent's turn did not see end is no longer
    /// followed after it — its log ended as interrupted, saying why, once —
    /// and a later turn naming it again does not reopen it.
    #[test]
    fn r_sub_7_a_codex_child_still_open_is_no_longer_followed_after_its_parents_turn() {
        let mut c = Children::default();
        feed(&mut c, &two_children());
        let rest = c.turn_ended(UNFINISHED);
        assert!(matches!(rest.last(), Some(EngineEvent::BackendChildEnded { task_id, status: ChildState::Interrupted, error: Some(e) }) if task_id == "kid-2" && e == UNFINISHED), "{rest:?}");
        assert_eq!(ends(&rest).len(), 1, "kid-1 had ended");
        assert!(c.turn_ended(UNFINISHED).is_empty(), "once");
        assert!(feed(&mut c, &[note("item/completed", "kid-2", json!({"type": "agentMessage", "id": "late", "text": "late"}))]).is_empty() && c.held.is_empty(), "a late line is dropped");
        let again = [
            note("item/completed", OWN, json!({"type": "subAgentActivity", "id": "s9", "kind": "interacted", "agentThreadId": "kid-2", "agentPath": "/root/turing"})),
            note("item/completed", OWN, json!({"type": "collabAgentToolCall", "id": "f", "tool": "followupTask", "senderThreadId": OWN, "receiverThreadIds": ["kid-2"], "status": "completed", "agentsStates": {}})),
        ];
        assert!(feed(&mut c, &again).is_empty(), "not reopened");
    }

    /// R-SUB-7, R-SUB-8: a child no spawn named yet takes its own first
    /// `userMessage` for its prompt, and a spawn naming it later gives no
    /// second; it ends as a collab call's `agentsStates` says, failed with
    /// Codex's words. Lines past `HOLD` are dropped, said after the prompt.
    #[test]
    fn r_sub_7_a_codex_child_ends_as_its_agent_state_says_and_its_held_lines_keep_its_prompt_first() {
        let mut c = Children::default();
        let said = |th: &str, i: usize| note("item/completed", th, json!({"type": "agentMessage", "id": format!("{th}-{i}"), "text": format!("line {i}")}));
        let lines = [note("item/completed", "kid-a", json!({"type": "userMessage", "id": "u", "content": [{"type": "text", "text": "fix the test"}]})), said("kid-a", 0)];
        assert!(feed(&mut c, &lines).is_empty(), "held");
        let evs = feed(&mut c, &[note("item/started", OWN, json!({"type": "subAgentActivity", "id": "s", "kind": "started", "agentThreadId": "kid-a", "agentPath": "/root/kid-a"}))]);
        assert!(matches!(&evs[0], EngineEvent::BackendChildStarted { task_id, prompt, description, .. } if task_id == "kid-a" && prompt.is_empty() && description == "Codex agent"));
        assert_eq!(items(&evs, "kid-a"), [Item::user("fix the test"), Item::AssistantText { text: "line 0".into() }]);
        let evs = feed(&mut c, &[note("item/completed", OWN, spawn("call-a", OWN, &["kid-a"], "fix the test"))]);
        assert!(evs.is_empty(), "no second prompt, no second start: {evs:?}");
        let wait = json!({"type": "collabAgentToolCall", "id": "w", "tool": "wait", "senderThreadId": OWN, "receiverThreadIds": ["kid-a"], "status": "completed", "agentsStates": {"kid-a": {"status": "errored", "message": "model refused"}}});
        let evs = feed(&mut c, &[note("item/completed", OWN, wait)]);
        assert!(matches!(evs.last(), Some(EngineEvent::BackendChildEnded { status: ChildState::Failed, error: Some(e), .. }) if e == "model refused"), "{evs:?}");
        // More than krowk holds of one thread: the oldest go, and it says so.
        let lines: Vec<(String, Value)> = (0..HOLD + 2).map(|i| said("kid-b", i)).collect();
        assert!(feed(&mut c, &lines).is_empty());
        let got = items(&feed(&mut c, &[note("item/completed", OWN, spawn("call-b", OWN, &["kid-b"], "tidy up"))]), "kid-b");
        assert_eq!(got[0], Item::user("tidy up"), "the prompt first");
        assert!(matches!(&got[1], Item::UserText { text, .. } if text.starts_with(DROPPED) && text.contains("2 of this agent's first lines")), "{:?}", got[1]);
        assert_eq!((got[2].clone(), got.len()), (Item::AssistantText { text: "line 2".into() }, 2 + HOLD));
        // Another thread's start that is no spawn of the session's: nothing
        // of it is held or followed.
        let evs = feed(&mut c, &[("thread/started".into(), json!({"thread": {"id": "review", "source": {"subAgent": "review"}}})), note("item/completed", "review", json!({"type": "agentMessage", "id": "r", "text": "x"}))]);
        assert!(evs.is_empty() && c.held.is_empty());
    }

    /// R-SUB-7: a `subAgentActivity` deeper than one segment under the
    /// root names a grandchild, never started; a thread whose held lines
    /// went whole past `WAITING` says how many when it starts after all.
    #[test]
    fn r_sub_7_a_deep_agent_path_starts_no_child_and_an_evicted_threads_lines_are_said() {
        let mut c = Children::default();
        let deep = json!({"type": "subAgentActivity", "id": "s", "kind": "started", "agentThreadId": "grand", "agentPath": "/root/ada/hopper"});
        assert!(feed(&mut c, &[note("item/completed", OWN, deep)]).is_empty());
        let done = json!({"type": "subAgentActivity", "id": "s2", "kind": "completed", "agentThreadId": "grand", "agentPath": "/root/ada/hopper"});
        assert!(feed(&mut c, &[note("item/completed", OWN, done)]).is_empty(), "nor ended");
        // The first thread's three lines go whole once 32 others are held.
        let line = |th: &str, i: usize| note("item/completed", th, json!({"type": "agentMessage", "id": format!("{th}-{i}"), "text": "x"}));
        let mut lines: Vec<(String, Value)> = (0..3).map(|i| line("first", i)).collect();
        lines.extend((0..WAITING).map(|i| line(&format!("other-{i}"), 0)));
        assert!(feed(&mut c, &lines).is_empty());
        assert!(!c.held.contains_key("first"));
        let got = items(&feed(&mut c, &[note("item/completed", OWN, spawn("call", OWN, &["first"], "late start"))]), "first");
        assert_eq!(got[0], Item::user("late start"));
        assert!(matches!(&got[1], Item::UserText { text, .. } if text.starts_with(DROPPED) && text.contains("3 of this agent's first lines")), "{got:?}");
        assert_eq!(got.len(), 2);
    }

    /// R-SUB-10: a child is stopped on the turn its own lines last named,
    /// none before they name one, nor once that turn completed; lent to
    /// answer its own request, it is still stopped. Once krowk sent it
    /// `turn/interrupt`, its turn's `interrupted` end is its end — not
    /// before — and an ended child, or a thread that is none, has nothing
    /// to stop or lend.
    #[test]
    fn r_sub_10_a_codex_child_is_stopped_on_the_turn_its_lines_name_and_ends_at_that_turns_interrupt() {
        let mut c = Children::default();
        feed(&mut c, &[note("item/completed", OWN, spawn("call", OWN, &["kid"], "go"))]);
        assert!(c.turn_of("kid").unwrap_err().contains("not said which turn"));
        feed(&mut c, &[("turn/started".into(), json!({"threadId": "kid", "turn": {"id": "d1", "items": [], "status": "inProgress"}}))]);
        assert_eq!(c.turn_of("kid").as_deref(), Ok("d1"));
        feed(&mut c, &[note("item/started", "kid", json!({"type": "reasoning", "id": "r", "summary": [], "content": []}))]);
        assert_eq!(c.turn_of("kid").as_deref(), Ok("t"), "the last its lines named");
        // Asking the person, its translator lent: still running, still stopped.
        let lent = c.lend("kid").expect("open");
        assert_eq!(c.turn_of("kid").as_deref(), Ok("t"), "lent is not ended");
        c.give_back("kid", lent);
        let interrupted = |turn: &str| ("turn/completed".to_string(), json!({"threadId": "kid", "turn": {"id": turn, "items": [], "status": "interrupted"}}));
        assert!(ends(&feed(&mut c, &[interrupted("t")])).is_empty(), "not stopped by krowk: Codex's own say ends it");
        assert!(c.turn_of("kid").unwrap_err().contains("no turn of it is running"), "its turn completed: nothing to interrupt");
        feed(&mut c, &[("item/started".into(), json!({"threadId": "kid", "turnId": "t2", "item": {"type": "reasoning", "id": "r2", "summary": [], "content": []}}))]);
        assert_eq!(c.turn_of("kid").as_deref(), Ok("t2"), "a later turn of it");
        c.stopping("kid");
        assert_eq!(ends(&feed(&mut c, &[interrupted("t2")])), [("kid", ChildState::Interrupted)]);
        assert!(c.turn_of("kid").unwrap_err().contains("ended") && c.lend("kid").is_none());
        assert!(c.turn_of(OWN).is_err() && c.lend(OWN).is_none() && c.lend("nobody").is_none());
    }

    /// R-SUB-9: a child's tool starts at its call item, before its result:
    /// the translator completes the call at `item/started`.
    #[test]
    fn r_sub_9_a_codex_childs_tool_starts_at_its_call_item() {
        let mut c = Children::default();
        feed(&mut c, &[note("item/completed", OWN, spawn("call", OWN, &["kid"], "go"))]);
        let evs = feed(&mut c, &[note("item/started", "kid", json!({"type": "commandExecution", "id": "c1", "command": "sleep 60", "cwd": "/", "commandActions": [], "status": "inProgress"}))]);
        assert!(evs.iter().any(|e| matches!(e, EngineEvent::BackendChild { event, .. } if matches!(&**event, EngineEvent::ItemCompleted { item: Item::ToolCall { name, .. }, .. } if name == "shell"))), "{evs:?}");
        assert!(!evs.iter().any(|e| matches!(e, EngineEvent::BackendChild { event, .. } if matches!(&**event, EngineEvent::ItemStarted { kind: ItemKind::ToolResult { .. }, .. }))));
    }
}
