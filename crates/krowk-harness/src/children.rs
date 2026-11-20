//! Child sessions, whoever starts them — krowk's own subagents, a
//! backend's agents, `--worktree` sessions: how many may run at once, the
//! machine-wide agent slots they hold, and the watch that frames where each
//! stands for the parent's client (R-SUB-2, R-SUB-9).

use crate::engine::EngineEvent;
use crate::protocol::{ChildState, ChildTool, Item, ItemKind, LiveEvent, Waiting};
use std::collections::HashMap;

/// Subagents at once, per parent turn, when the config does not say.
pub const MAX_PARALLEL: usize = 4;

/// The pool of the machine's agent slots (`subagents.maxHost`).
pub const AGENT_SLOTS: &str = "agent-slots";

/// Agents at once on the machine when the config does not say: twice the
/// cores or twice the GiB of memory, whichever is fewer — an agent's
/// builds want cores, its language servers memory — kept to 4 … 64. Twice
/// the cores alone when the memory cannot be read.
pub fn max_host() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let memory = memory_bytes().map_or(usize::MAX, |b| usize::try_from(b / (1 << 30)).unwrap_or(usize::MAX));
    (cores * 2).min(memory.saturating_mul(2)).clamp(4, 64)
}

/// The machine's memory, in bytes, when it can be read.
#[cfg(unix)]
fn memory_bytes() -> Option<u64> {
    // SAFETY: sysconf has no preconditions; -1 is its failure.
    let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    (pages > 0 && size > 0).then(|| pages as u64 * size as u64)
}

#[cfg(windows)]
fn memory_bytes() -> Option<u64> {
    // Filled by the call; only the total is read.
    #[repr(C)]
    #[allow(dead_code)]
    struct MemoryStatusEx {
        length: u32,
        load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx { length: std::mem::size_of::<MemoryStatusEx>() as u32, load: 0, total_phys: 0, avail_phys: 0, total_page_file: 0, avail_page_file: 0, total_virtual: 0, avail_virtual: 0, avail_extended_virtual: 0 };
    // SAFETY: `status` is a MEMORYSTATUSEX with its length set, as the call asks.
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.total_phys)
}

#[cfg(not(any(unix, windows)))]
fn memory_bytes() -> Option<u64> {
    None
}

/// What a subagent waiting for an agent slot says on its call.
pub fn waiting_line(in_use: usize) -> String {
    format!("waiting for an agent slot ({in_use} in use)\n")
}

/// A child's state as its `subagent.status` frames say it (R-SUB-9), kept
/// by the writer of the child's own turn from the events that turn
/// handles: the call it is in, what it waits on the person for, its tokens
/// and when anything last came from it.
pub(crate) struct ChildWatch {
    session_id: String,
    /// The tool each call the model made names, by call id, until it runs.
    named: HashMap<String, String>,
    /// The calls running, oldest first, by call id.
    tools: Vec<(String, ChildTool)>,
    /// The requests waiting on the person, by request id.
    waiting: Vec<(String, Waiting)>,
    /// The call each waiting request asks about, when the engine said
    /// (`EngineEvent::AskedFor`).
    asked: HashMap<String, String>,
    last_event_ms: i64,
    tokens: i64,
    /// A backend's agent: its items come whole, a call's result started
    /// and completed at once, so its tool runs from its call (`backend`).
    at_call: bool,
}

impl ChildWatch {
    /// The watch of a turn's session when the turn is a subagent's: one
    /// with a parent.
    pub fn of<P>(parent: Option<&P>, session_id: &str) -> Option<ChildWatch> {
        parent.map(|_| ChildWatch::new(session_id))
    }

    pub fn new(session_id: &str) -> ChildWatch {
        ChildWatch { session_id: session_id.into(), named: HashMap::new(), tools: Vec::new(), waiting: Vec::new(), asked: HashMap::new(), last_event_ms: krowk_store::now_ms(), tokens: 0, at_call: false }
    }

    /// The watch of a backend's agent (R-SUB-7), whose items come whole:
    /// a tool runs from its call, the only moment before its result, and
    /// its tokens are the backend's count (`EngineEvent::Progress`).
    pub fn backend(session_id: &str) -> ChildWatch {
        ChildWatch { at_call: true, ..ChildWatch::new(session_id) }
    }

    /// The tokens its frames say it has used.
    pub fn tokens(&self) -> i64 {
        self.tokens
    }

    /// Takes in what the child's engine said at `now`: true when its state
    /// changed, so a frame is due. A text delta, or anything else that only
    /// shows it is alive, changes `lastEventMs` and nothing a frame is sent
    /// for: the next frame carries it.
    pub fn note(&mut self, ev: &EngineEvent, now: i64) -> bool {
        self.last_event_ms = now;
        match ev {
            EngineEvent::ItemCompleted { item: Item::ToolCall { call_id, name, .. }, .. } if self.at_call => {
                self.tools.push((call_id.clone(), ChildTool { name: name.clone(), started_ms: now }));
                true
            }
            EngineEvent::ItemCompleted { item: Item::ToolCall { call_id, name, .. }, .. } => {
                self.named.insert(call_id.clone(), name.clone());
                false
            }
            EngineEvent::ItemStarted { kind: ItemKind::ToolResult { .. }, .. } if self.at_call => false,
            EngineEvent::ItemStarted { kind: ItemKind::ToolResult { call_id }, .. } => {
                let name = self.named.remove(call_id).unwrap_or_default();
                self.tools.push((call_id.clone(), ChildTool { name, started_ms: now }));
                true
            }
            EngineEvent::ItemCompleted { item: Item::ToolResult { call_id, .. }, .. } => {
                let before = self.tools.len();
                self.tools.retain(|(c, _)| c != call_id);
                self.tools.len() != before
            }
            EngineEvent::Approval(r) => {
                let what = if r.questions.is_empty() { Waiting::Approval } else { Waiting::Question };
                self.waiting.push((r.request_id.clone(), what));
                true
            }
            // Answered, the call it held runs from now: the time it waited
            // on the person is not the tool's.
            EngineEvent::ApprovalResolved { request_id, .. } => {
                let before = self.waiting.len();
                self.waiting.retain(|(r, _)| r != request_id);
                let answered = self.waiting.len() != before;
                // The call it asked about when known, else the latest.
                let call = self.asked.remove(request_id);
                let held = match &call {
                    Some(c) => self.tools.iter_mut().find(|(id, _)| id == c),
                    None => self.tools.last_mut(),
                };
                if answered && let Some((_, t)) = held {
                    t.started_ms = now;
                }
                answered
            }
            EngineEvent::AskedFor { request_id, call_id } => {
                self.asked.insert(request_id.clone(), call_id.clone());
                false
            }
            EngineEvent::ResponseCompleted { usage, .. } => {
                self.tokens += usage.total();
                true
            }
            // A count, never less than the last one.
            EngineEvent::Progress { tokens } if *tokens > self.tokens => {
                self.tokens = *tokens;
                true
            }
            _ => false,
        }
    }

    /// The frame for where the child stands: an ended child is in no call
    /// and waits on nobody, and one waiting on the person is in no call
    /// yet — the call it waits to run starts once answered.
    pub fn frame(&self, status: ChildState) -> LiveEvent {
        let running = status == ChildState::Running;
        LiveEvent::SubagentStatus {
            session_id: self.session_id.clone(),
            status,
            tool: self.tools.first().filter(|_| running && self.waiting.is_empty()).map(|(_, t)| t.clone()),
            last_event_ms: self.last_event_ms,
            waiting: self.waiting.first().filter(|_| running).map(|(_, w)| *w),
            tokens: self.tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-SUB-9: a backend's agent's items come whole — its call's result
    /// started and completed at once — so its tool runs from its call, is
    /// none while it waits on the person, runs again from the answer, and
    /// ends at its result; its tokens are the backend's count, never less.
    #[test]
    fn r_sub_9_a_backend_agents_tool_runs_from_its_call_and_its_tokens_are_the_backends_count() {
        use crate::protocol::{ApprovalDecision, ApprovalRequest};
        let mut w = ChildWatch::backend("k");
        let call = Item::ToolCall { call_id: "c1".into(), name: "Bash".into(), input: serde_json::json!({}) };
        let result = Item::ToolResult { call_id: "c1".into(), output: "ok".into(), is_error: false, images: Vec::new() };
        let frame = |w: &ChildWatch| match w.frame(ChildState::Running) {
            LiveEvent::SubagentStatus { tool, waiting, tokens, .. } => (tool.map(|t| (t.name, t.started_ms)), waiting, tokens),
            other => panic!("{other:?}"),
        };
        assert!(w.note(&EngineEvent::Progress { tokens: 1200 }, 1));
        assert!(w.note(&EngineEvent::ItemCompleted { item_id: "i1".into(), item: call }, 2));
        assert_eq!(frame(&w), (Some(("Bash".into(), 2)), None, 1200), "running from its call");
        let req = ApprovalRequest { session_id: "k".into(), turn_id: "t".into(), request_id: "r1".into(), tool: "Bash".into(), input: serde_json::json!({}), summary: "Bash `sleep 30`".into(), reason: "x".into(), remember: Vec::new(), questions: Vec::new() };
        assert!(w.note(&EngineEvent::Approval(req), 3));
        assert_eq!(frame(&w), (None, Some(Waiting::Approval), 1200), "waiting, in no call yet");
        assert!(w.note(&EngineEvent::ApprovalResolved { request_id: "r1".into(), decision: ApprovalDecision::Allow }, 9));
        assert_eq!(frame(&w), (Some(("Bash".into(), 9)), None, 1200), "its clock starts at the answer");
        assert!(!w.note(&EngineEvent::Progress { tokens: 1100 }, 10), "a count never goes down");
        assert!(w.note(&EngineEvent::Progress { tokens: 1300 }, 11));
        assert!(!w.note(&EngineEvent::ItemStarted { item_id: "i2".into(), kind: ItemKind::ToolResult { call_id: "c1".into() } }, 20), "its result's start ends nothing");
        assert!(w.note(&EngineEvent::ItemCompleted { item_id: "i2".into(), item: result }, 20));
        assert_eq!(frame(&w), (None, None, 1300));
        assert!(matches!(w.frame(ChildState::Done), LiveEvent::SubagentStatus { status: ChildState::Done, tool: None, waiting: None, tokens: 1300, .. }));
        // Two calls in one message, the first asked about: its clock starts
        // again at the answer, the other's runs on.
        let mut w = ChildWatch::backend("k");
        for (c, at) in [("c1", 2), ("c2", 3)] {
            w.note(&EngineEvent::ItemCompleted { item_id: c.into(), item: Item::ToolCall { call_id: c.into(), name: "Bash".into(), input: serde_json::json!({}) } }, at);
        }
        assert!(!w.note(&EngineEvent::AskedFor { request_id: "r2".into(), call_id: "c1".into() }, 4));
        w.note(&EngineEvent::Approval(req_for("r2")), 4);
        w.note(&EngineEvent::ApprovalResolved { request_id: "r2".into(), decision: ApprovalDecision::Allow }, 9);
        let started: Vec<(&str, i64)> = w.tools.iter().map(|(c, t)| (c.as_str(), t.started_ms)).collect();
        assert_eq!(started, [("c1", 9), ("c2", 3)]);
    }

    fn req_for(id: &str) -> crate::protocol::ApprovalRequest {
        crate::protocol::ApprovalRequest { session_id: "k".into(), turn_id: "t".into(), request_id: id.into(), tool: "Bash".into(), input: serde_json::json!({}), summary: "Bash `x`".into(), reason: "x".into(), remember: Vec::new(), questions: Vec::new() }
    }
}
