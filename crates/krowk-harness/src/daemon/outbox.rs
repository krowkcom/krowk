//! What the daemon has for one client and has not handed to its transport
//! yet: one queue per session it follows, bounded, so a client that stops
//! reading — a TUI suspended with ^Z, a phone in a tunnel — costs the
//! daemon a fixed amount, never a buffer that grows with the session
//! (R-LAG-4, R-LAG-10).
//!
//! Every frame is encoded once, when it is published, and queued as the
//! bytes it goes out as. Three kinds of frame are queued differently:
//!
//! - A progress frame — `cost`, `limits`, `backend.agents` — is a state,
//!   not an event: a newer one for the same slot replaces the one waiting
//!   (R-LAG-2), so a subagent reporting its spend a thousand times a second
//!   costs one frame, not a thousand.
//! - Any other `line` is queued in order. When a session's queue passes its
//!   cap, every line waiting in it is dropped and the session is marked
//!   behind: lines published while it is are not queued at all, and the
//!   server catches the client up from its cursor — the last line handed
//!   on, by its `seq` and its last logged event — from the log and the
//!   running turn, as an `attach` would, a page of at most the cap at a
//!   time, the next page once the client has read the last. The client
//!   sees one gap-free stream; the daemon holds at most about twice the cap
//!   for it (a page, and the control frames it has not read). An `attach`
//!   is the same catching up, from the cursor it names.
//! - A control frame — `done`, `attached`, `settled`, `welcome`, `status` —
//!   is never dropped: it answers something the client asked, and there
//!   are only as many as it asked for. One the client never reads still
//!   counts, and past the cap the client is let go.
//!
//! A session that is being caught up holds its control frames, each marked
//! with where the session stood when it was queued (`Out::mark`), and puts
//! each back among the catching up at that place: a turn's `done` comes
//! after its lines and before the next turn's.

use crate::protocol::{LiveEvent, StreamLine};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use tokio::sync::Notify;

/// How much one client may have waiting for one session before it counts as
/// fallen behind, and for control frames before it is let go.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub session_bytes: usize,
    pub control_bytes: usize,
}

impl Default for Caps {
    /// Four megabytes of one session is a couple of minutes of a model
    /// typing flat out: a client that is that far behind is caught up from
    /// its cursor faster than it would read the backlog.
    fn default() -> Caps {
        Caps { session_bytes: 4 << 20, control_bytes: 1 << 20 }
    }
}

/// One frame, encoded, and what the queue needs to know about it.
#[derive(Debug, Clone)]
pub struct Out {
    /// The frame as it goes out: one JSON object and its newline.
    pub bytes: Rc<[u8]>,
    /// Its `line.seq`; 0 for a frame with none.
    pub seq: u64,
    /// The id of the followed session's own logged event it carries: with
    /// `seq`, the cursor a client that fell behind is caught up from.
    pub log: Option<Rc<str>>,
    /// A `line`: dropped when its session falls behind, since the catching
    /// up brings it again. Anything else is a control frame.
    pub line: bool,
    /// The slot a progress frame overwrites.
    pub slot: Option<Slot>,
    /// A control frame's place: where its session stood (its last own
    /// logged event and `seq`) when it was queued.
    pub mark: Cursor,
}

/// What a progress frame is the latest value of: its kind and the session
/// (and instance) it is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Slot(&'static str, String);

/// The slot `line` overwrites, when it is a progress frame.
pub fn slot_of(line: &StreamLine) -> Option<Slot> {
    match line {
        StreamLine::Live(LiveEvent::Cost { session_id, .. }) => Some(Slot("cost", session_id.clone())),
        StreamLine::Live(LiveEvent::Limits { session_id, instance, .. }) => Some(Slot("limits", format!("{session_id}\u{0}{instance}"))),
        StreamLine::Live(LiveEvent::BackendAgents { session_id, .. }) => Some(Slot("agents", session_id.clone())),
        StreamLine::Live(LiveEvent::Background { session_id, .. }) => Some(Slot("background", session_id.clone())),
        StreamLine::Live(LiveEvent::SubagentStatus { session_id, .. }) => Some(Slot("subagent.status", session_id.clone())),
        _ => None,
    }
}

/// Where a client stands in a session: the last `seq` and logged event
/// handed to its transport.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cursor {
    pub seq: u64,
    pub log: Option<Rc<str>>,
}

/// What a push did.
#[derive(Debug, PartialEq)]
pub enum Pushed {
    Queued,
    /// The session's queue passed its cap just now: its lines are dropped,
    /// and it waits to be caught up from this cursor (`resynced`).
    FellBehind(Cursor),
    /// Control frames past their cap: the client does not read, and is to
    /// be let go.
    Overflow,
    /// A line of a session that is behind, or of a closed outbox: not
    /// queued.
    Dropped,
}

#[derive(Default)]
struct Queue {
    /// Frames in order, each with its place (`ord`) among this queue's.
    fifo: VecDeque<(u64, Out)>,
    /// The latest frame of each progress slot, with its place.
    slots: HashMap<Slot, (u64, Out)>,
    bytes: usize,
    next: u64,
    behind: bool,
    /// Behind, and its next page asked for (`due`).
    resyncing: bool,
    /// Control frames held while it is behind, in order.
    held: Vec<Out>,
    /// The `attach` this catching up answers, if it is one.
    attach: Option<u64>,
    /// What has been handed on.
    sent: Cursor,
}

impl Queue {
    fn push(&mut self, out: Out) {
        let ord = self.next;
        self.next += 1;
        self.bytes += out.bytes.len();
        match out.slot.clone() {
            Some(s) => {
                if let Some((_, old)) = self.slots.insert(s, (ord, out)) {
                    self.bytes -= old.bytes.len();
                }
            }
            None => self.fifo.push_back((ord, out)),
        }
    }

    /// The next frame by place, from the fifo or a slot.
    fn pop(&mut self) -> Option<Out> {
        let slot = self.slots.iter().min_by_key(|(_, (ord, _))| *ord).map(|(k, (ord, _))| (k.clone(), *ord));
        let out = match (self.fifo.front().map(|(o, _)| *o), slot) {
            (Some(f), Some((_, s))) if f < s => self.fifo.pop_front().map(|(_, o)| o),
            (_, Some((k, _))) => self.slots.remove(&k).map(|(_, o)| o),
            (Some(_), None) => self.fifo.pop_front().map(|(_, o)| o),
            (None, None) => None,
        }?;
        self.bytes -= out.bytes.len();
        if out.seq > 0 {
            self.sent.seq = self.sent.seq.max(out.seq);
        }
        if out.log.is_some() {
            self.sent.log = out.log.clone();
        }
        Some(out)
    }

    fn is_empty(&self) -> bool {
        self.fifo.is_empty() && self.slots.is_empty()
    }
}

#[derive(Default)]
struct Inner {
    /// The session each queue is for, `""` for frames of no session; in
    /// the order they were first used, which the round robin follows.
    queues: Vec<(String, Queue)>,
    /// Where the round robin goes on from.
    turn: usize,
    closed: bool,
}

pub struct Outbox {
    inner: RefCell<Inner>,
    ready: Notify,
    caps: Caps,
}

impl Outbox {
    pub fn new(caps: Caps) -> Outbox {
        Outbox { inner: RefCell::default(), ready: Notify::new(), caps }
    }

    /// Queues `out` for `session` (`""`: none).
    pub fn push(&self, session: &str, out: Out) -> Pushed {
        let mut inner = self.inner.borrow_mut();
        if inner.closed {
            return Pushed::Dropped;
        }
        let i = match inner.queues.iter().position(|(s, _)| s == session) {
            Some(i) => i,
            None => {
                inner.queues.push((session.to_string(), Queue::default()));
                inner.queues.len() - 1
            }
        };
        let q = &mut inner.queues[i].1;
        let line = out.line;
        if q.behind {
            if line {
                return Pushed::Dropped;
            }
            q.bytes += out.bytes.len();
            q.held.push(out);
        } else {
            q.push(out);
        }
        let control = |q: &Queue| q.fifo.iter().filter(|(_, o)| !o.line).chain(q.slots.values()).filter(|(_, o)| !o.line).map(|(_, o)| o.bytes.len()).sum::<usize>() + q.held.iter().map(|o| o.bytes.len()).sum::<usize>();
        let pushed = if line && !q.behind && q.bytes > self.caps.session_bytes {
            // The lines go; the control frames are held, to go back in at
            // their places among the catching up.
            let mut held: Vec<Out> = q.fifo.drain(..).map(|(_, o)| o).filter(|o| !o.line).collect();
            held.append(&mut q.held);
            q.held = held;
            q.slots.clear();
            q.bytes = q.held.iter().map(|o| o.bytes.len()).sum();
            q.behind = true;
            q.resyncing = false;
            Pushed::FellBehind(q.sent.clone())
        } else if !line && control(q) > self.caps.control_bytes {
            Pushed::Overflow
        } else {
            Pushed::Queued
        };
        drop(inner);
        self.ready.notify_one();
        pushed
    }

    /// Starts catching `session` up from `cursor`, as an `attach` (`id`)
    /// does: the queue is behind until the last page is in, and live lines
    /// meanwhile come through the pages instead.
    pub fn begin(&self, session: &str, cursor: Cursor, attach: u64) {
        let mut inner = self.inner.borrow_mut();
        let i = match inner.queues.iter().position(|(s, _)| s == session) {
            Some(i) => i,
            None => {
                inner.queues.push((session.to_string(), Queue::default()));
                inner.queues.len() - 1
            }
        };
        let q = &mut inner.queues[i].1;
        // A second attach of one session: the first's pages serve both.
        if let Some(old) = q.attach.replace(attach) {
            q.attach = Some(old.max(attach));
        }
        if !q.behind {
            let mut held: Vec<Out> = q.fifo.drain(..).map(|(_, o)| o).filter(|o| !o.line).collect();
            held.append(&mut q.held);
            q.held = held;
            q.slots.clear();
            q.bytes = q.held.iter().map(|o| o.bytes.len()).sum();
            q.behind = true;
            q.resyncing = false;
            q.sent = cursor;
        }
        drop(inner);
        self.ready.notify_one();
    }

    /// Whether `session` is being caught up and waits for its next page:
    /// a client that left it (`forget`) is not given one.
    pub fn expects(&self, session: &str) -> bool {
        self.inner.borrow().queues.iter().any(|(s, q)| s == session && q.behind && q.resyncing)
    }

    /// The control frames `session` holds while behind, taken to be placed
    /// among a page (`resynced` gives back those that belong after it).
    pub fn held(&self, session: &str) -> Vec<Out> {
        let mut inner = self.inner.borrow_mut();
        let Some((_, q)) = inner.queues.iter_mut().find(|(s, _)| s == session) else { return Vec::new() };
        let held = std::mem::take(&mut q.held);
        q.bytes -= held.iter().map(|o| o.bytes.len()).sum::<usize>();
        held
    }

    /// A page of catching up, the held control frames already placed in
    /// it; `rest` are those that belong after it. The last page (`last`)
    /// ends the catching up: live lines are queued again from here, and the
    /// `attach` it answered, if any, is handed back to be answered after it.
    pub fn resynced(&self, session: &str, page: Vec<Out>, rest: Vec<Out>, last: bool) -> Option<u64> {
        let mut inner = self.inner.borrow_mut();
        let (_, q) = inner.queues.iter_mut().find(|(s, _)| s == session)?;
        for o in page {
            q.push(o);
        }
        q.resyncing = false;
        let mut attach = None;
        if last {
            q.behind = false;
            attach = q.attach.take();
            for o in rest {
                q.push(o);
            }
        } else {
            q.bytes += rest.iter().map(|o| o.bytes.len()).sum::<usize>();
            q.held = rest;
        }
        drop(inner);
        self.ready.notify_one();
        attach
    }

    /// Whether `session` waits to be caught up.
    pub fn behind(&self, session: &str) -> bool {
        self.inner.borrow().queues.iter().any(|(s, q)| s == session && q.behind)
    }

    /// The sessions behind whose catching up is due, each with its cursor:
    /// asked by the transport once it has handed on all it could, so a
    /// client that is still not reading is caught up once when it reads
    /// again, not over and over while it does not.
    pub fn due(&self) -> Vec<(String, Cursor)> {
        let mut inner = self.inner.borrow_mut();
        let mut due = Vec::new();
        for (s, q) in inner.queues.iter_mut() {
            // Due once what it was sent last has been handed on: a client
            // that still does not read is not paged at again and again.
            if q.behind && !q.resyncing && q.is_empty() {
                q.resyncing = true;
                due.push((s.clone(), q.sent.clone()));
            }
        }
        due
    }

    /// Waits until there is something to hand on, or a catching up is due;
    /// false once closed and drained.
    pub async fn ready(&self) -> bool {
        loop {
            {
                let inner = self.inner.borrow();
                if inner.queues.iter().any(|(_, q)| !q.is_empty() || (q.behind && !q.resyncing)) {
                    return true;
                }
                if inner.closed {
                    return false;
                }
            }
            self.ready.notified().await;
        }
    }

    /// Bytes waiting, over every session.
    pub fn bytes(&self) -> usize {
        self.inner.borrow().queues.iter().map(|(_, q)| q.bytes).sum()
    }

    /// Up to about `max` bytes of what waits, per session, taken round
    /// robin so no session waits behind another's backlog: each session
    /// gets a fair share of the batch before any gets more.
    pub fn take(&self, max: usize) -> Vec<(String, Vec<Out>)> {
        let mut inner = self.inner.borrow_mut();
        let n = inner.queues.len();
        let mut out: Vec<(String, Vec<Out>)> = Vec::new();
        let mut taken = 0;
        let start = inner.turn;
        loop {
            let mut any = false;
            for k in 0..n {
                let i = (start + k) % n;
                let (session, q) = &mut inner.queues[i];
                // A share at a time: a few frames, then the next session.
                let mut share = 0;
                while share < 16 * 1024 && taken < max {
                    let Some(o) = q.pop() else { break };
                    share += o.bytes.len();
                    taken += o.bytes.len();
                    any = true;
                    match out.iter_mut().find(|(s, _)| s == session) {
                        Some((_, v)) => v.push(o),
                        None => out.push((session.clone(), vec![o])),
                    }
                }
            }
            if !any || taken >= max {
                break;
            }
        }
        // A queue stays while its session is followed, empty or not: it
        // holds the cursor.
        inner.turn = if n == 0 { 0 } else { (start + 1) % n };
        out
    }

    /// Forgets `session`: the client left it.
    pub fn forget(&self, session: &str) {
        let mut inner = self.inner.borrow_mut();
        inner.queues.retain(|(s, _)| s != session);
        inner.turn = 0;
    }

    /// Whether it still takes frames.
    pub fn open(&self) -> bool {
        !self.inner.borrow().closed
    }

    /// Nothing more is queued; the writer ends once it has handed on what
    /// waits.
    pub fn close(&self) {
        self.inner.borrow_mut().closed = true;
        self.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Delta, LiveEvent};

    fn delta(session: &str, seq: u64, text: &str) -> Out {
        let line = StreamLine::Live(LiveEvent::ItemDelta { session_id: session.into(), turn_id: "t".into(), item_id: "i".into(), delta: Delta::Text { text: text.into() } });
        Out { bytes: Rc::from(serde_json::to_vec(&line).unwrap()), seq, log: None, line: true, slot: None, mark: Cursor::default() }
    }

    fn cost(session: &str, seq: u64, usd: f64) -> Out {
        let line = StreamLine::Live(LiveEvent::Cost { session_id: session.into(), turn_id: "t".into(), cost_usd: Some(usd), turn_cost_usd: None, generated_tokens: 0 });
        Out { bytes: Rc::from(serde_json::to_vec(&line).unwrap()), seq, log: None, line: true, slot: slot_of(&line), mark: Cursor::default() }
    }

    fn control(text: &str) -> Out {
        Out { bytes: Rc::from(text.as_bytes()), seq: 0, log: None, line: false, slot: None, mark: Cursor::default() }
    }

    fn seqs(batch: &[(String, Vec<Out>)], session: &str) -> Vec<u64> {
        batch.iter().filter(|(s, _)| s == session).flat_map(|(_, v)| v.iter().map(|o| o.seq)).collect()
    }

    /// R-LAG-2: a subagent flooding its spend costs one queued frame, which
    /// goes out in its place among the lines, and never holds another
    /// session's lines back.
    #[test]
    fn r_lag_2_progress_overwrites_its_slot_and_one_session_never_waits_on_another() {
        let o = Outbox::new(Caps::default());
        o.push("a", delta("a", 1, "x"));
        for i in 0..100_000u64 {
            assert_eq!(o.push("sub", cost("sub", 2 + i, i as f64)), Pushed::Queued);
        }
        o.push("a", delta("a", 100_002, "y"));
        // One frame for the whole flood, and it is the last one.
        assert!(o.bytes() < 1024, "{} bytes queued for one slot", o.bytes());
        let batch = o.take(1 << 20);
        assert_eq!(seqs(&batch, "sub"), vec![100_001]);
        assert_eq!(seqs(&batch, "a"), vec![1, 100_002]);
        // A session with a backlog shares the batch: a small take still
        // carries the quiet session's frame.
        for i in 0..10_000 {
            o.push("busy", delta("busy", i + 1, "some text of a token"));
        }
        o.push("quiet", delta("quiet", 1, "hi"));
        let batch = o.take(20 * 1024);
        assert_eq!(seqs(&batch, "quiet"), vec![1], "the quiet session is in the first batch");
    }

    /// R-SUB-9: a child's `subagent.status` is progress — a newer one for
    /// the same child replaces the one queued, in its place — and never
    /// one of another child's.
    #[test]
    fn r_sub_9_a_newer_status_replaces_the_one_queued_for_its_child_alone() {
        let status = |child: &str, seq: u64, tokens: i64| {
            let line = StreamLine::Live(LiveEvent::SubagentStatus { session_id: child.into(), status: crate::protocol::ChildState::Running, tool: None, last_event_ms: 1, waiting: None, tokens });
            Out { bytes: Rc::from(serde_json::to_vec(&line).unwrap()), seq, log: None, line: true, slot: slot_of(&line), mark: Cursor::default() }
        };
        let o = Outbox::new(Caps::default());
        o.push("p", status("k1", 1, 10));
        o.push("p", status("k2", 2, 20));
        o.push("p", delta("p", 3, "x"));
        o.push("p", status("k1", 4, 30));
        let batch = o.take(1 << 20);
        let sent: Vec<(u64, String)> = batch.iter().flat_map(|(_, v)| v.iter().map(|o| (o.seq, String::from_utf8_lossy(&o.bytes).into_owned()))).collect();
        assert_eq!(sent.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![2, 3, 4], "k1's first frame gave way to its newer one");
        assert!(sent[2].1.contains("\"tokens\":30"), "{}", sent[2].1);
    }

    /// R-LAG-4 / R-LAG-10: a client that stops reading costs at most the
    /// cap; past it, its session is dropped to a cursor, control frames are
    /// held, and catching up puts the backlog back ahead of them.
    #[test]
    fn r_lag_4_a_client_past_its_cap_falls_behind_to_its_cursor() {
        let o = Outbox::new(Caps { session_bytes: 4096, control_bytes: 4096 });
        o.push("a", delta("a", 1, "first"));
        assert_eq!(seqs(&o.take(1 << 20), "a"), vec![1]);
        let mut fell = None;
        for i in 2..10_000u64 {
            match o.push("a", delta("a", i, "a token of text")) {
                Pushed::Queued => {}
                Pushed::FellBehind(c) => {
                    fell = Some((i, c));
                }
                Pushed::Dropped => {}
                Pushed::Overflow => panic!("lines never overflow"),
            }
            assert!(o.bytes() <= 4096 + 256, "bounded: {} bytes", o.bytes());
        }
        let (at, cursor) = fell.expect("it fell behind");
        assert!(at < 100, "at the cap, not later: {at}");
        assert_eq!(cursor.seq, 1, "the cursor is what was handed on");
        assert!(o.behind("a"));
        // Held while behind; the catching up is asked for once, and again
        // only once its page has been read.
        o.push("a", control("{\"type\":\"done\"}\n"));
        assert!(o.take(1 << 20).is_empty(), "nothing of a session behind goes out");
        assert_eq!(o.due(), vec![("a".to_string(), cursor.clone())]);
        assert!(o.due().is_empty());
        let held = o.held("a");
        assert_eq!(held.len(), 1);
        o.resynced("a", vec![delta("a", 9_990, "a page")], held, false);
        assert!(o.behind("a"), "a page is not the last");
        assert!(o.due().is_empty(), "not before the page is read");
        assert_eq!(seqs(&o.take(1 << 20), "a"), vec![9_990]);
        let (_, next) = o.due().pop().expect("the next page is due once it is read");
        assert_eq!(next.seq, 9_990, "from where the page left the cursor");
        let held = o.held("a");
        o.resynced("a", vec![delta("a", 9_998, "caught"), delta("a", 9_999, "up")], held, true);
        let expected: usize = [delta("a", 9_998, "caught"), delta("a", 9_999, "up")].iter().map(|o| o.bytes.len()).sum::<usize>() + "{\"type\":\"done\"}\n".len();
        assert_eq!(o.bytes(), expected, "every frame counted once");
        o.push("a", delta("a", 10_000, "live"));
        let batch = o.take(1 << 20);
        let a: Vec<&Out> = batch.iter().filter(|(s, _)| s == "a").flat_map(|(_, v)| v).collect();
        assert_eq!(a.iter().map(|o| o.seq).collect::<Vec<_>>(), vec![9_998, 9_999, 0, 10_000]);
        assert!(!a[2].line);
        // A session the client left is not caught up.
        for i in 0..1000 {
            o.push("b", delta("b", i + 1, "some text of a token"));
        }
        assert!(o.behind("b"));
        assert_eq!(o.due().len(), 1);
        o.forget("b");
        assert!(!o.expects("b"));
    }

    #[test]
    fn r_lag_10_control_frames_nobody_reads_let_the_client_go() {
        let o = Outbox::new(Caps { session_bytes: 4096, control_bytes: 1024 });
        let mut over = false;
        for _ in 0..100 {
            over |= o.push("", control(&"x".repeat(100))) == Pushed::Overflow;
        }
        assert!(over);
    }
}
