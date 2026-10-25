//! What a viewer of a synced session shows, decided without IO
//! (R-CLIENT-1; engineering/harness.md → Sync): the updates a screen draws
//! from, a stream batch applied to them once, the logged events deduped,
//! the ack to the relay, the direct-path timings, and the catch-ups a gap
//! asks for. The harness's viewer runs them on its tokio loop; the phone's
//! viewer will run the same. The clock is the caller's: nothing here reads
//! it.
//!
//! `ViewerCore` is the whole viewer built from them, sans IO (R-CLIENT-3):
//! the bytes that arrived and the time go in, and the bytes to send, the
//! updates to show and the reads and joins to run come out.

use super::commands::CommandBook;
use super::join::read_control;
use super::store::{Attached, Head, ReadError};
use super::{Answer, Batch, Candidate, ViewerFrame, Welcome, DEAD, FRAME, PING};
use crate::e2e::{self, DeviceId, SessionKeys};
use crate::protocol::frame::{HEADER, KIND_ACK, KIND_BATCH, KIND_RELAY};
use crate::protocol::{Command, LiveEvent, StreamLine};
use crate::relay_link::{Outbound, ViewerLink};
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

/// What a viewer hands its screen, one `Vec` a display frame.
// A line is most of what a viewer is handed, so it stays unboxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Update {
    /// The session as the chunks hold it, once, as the viewer attaches.
    Attached { events: Vec<serde_json::Value>, head: Option<Head> },
    Line(StreamLine),
    /// A host is on the channel (and welcomed this viewer), or none is: the
    /// viewer is read-only until one is.
    Host(bool),
    /// A command, shown at once; `queued` when no host is there to take it.
    Sent { id: String, queued: bool },
    /// The host took it, or refused it.
    Acked { id: String, error: Option<String> },
    /// Events the chunks or the host held that the live stream did not
    /// bring.
    CaughtUp(Vec<serde_json::Value>),
    /// The path the session now comes by: `relay`, `direct over Tailscale`
    /// or `direct over LAN`, and the address when direct (R-NET-2).
    Path { path: String, via: Option<String> },
    /// The relay dropped a batch: the host is asked for what it held, and
    /// `CaughtUp` follows.
    Gap,
    /// Something the viewer cannot go on from.
    Failed(String),
    /// Something to tell the person beside the session: why the relay
    /// cannot be joined, said once as it comes or changes. Its own update,
    /// not a line on stderr, so a screen drawing the session shows it where
    /// it draws rather than having it written over its frame.
    Note(String),
    /// The host's answer to a handoff this viewer asked for (R-HAND-1):
    /// what this device takes the session up with, or why it stays.
    Handoff(Result<super::Handed, String>),
    /// Where this viewer stands as of the `Show` it ends, the last update
    /// of one that moved it: what a device keeping its place across
    /// launches saves with the events that `Show` holds, in one write.
    Saved(Cursor),
}

/// A viewer's place (engineering/harness.md → Sync cursors): the stream,
/// the last batch of it applied and shown, and the newest logged event the
/// viewer holds every event up to — older than the newest held while a
/// catch-up for a gap is still out, so a place kept across launches never
/// catches up from past a gap.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    pub stream: String,
    pub after_seq: u64,
    pub newest: Option<String>,
}

/// The last-applied-batch ack a viewer sends the relay (relay.md → Flow
/// control): kind 3, in the clear, `seq` the last batch it applied.
pub fn ack(session: &[u8; 16], seq: u64) -> Vec<u8> {
    let mut b = vec![crate::protocol::frame::V, KIND_ACK, 0, 0];
    b.extend_from_slice(session);
    b.extend_from_slice(&seq.to_be_bytes());
    b
}

/// A viewer acks at least every this many batches shown, well inside the
/// relay's window of 16, and at the next display frame after any it
/// applied. A frame holding more than the window's batches holds the relay
/// back until it is shown: flow control, not a stall.
pub const ACK_EVERY: u64 = 4;
const _: () = assert!(ACK_EVERY < 16, "inside the relay's window of 16");

/// Keeps the newest logged event id: ids are UUIDv7, so the greatest is
/// the latest.
pub fn newest(last: &mut Option<String>, id: &str) {
    if last.as_deref().is_none_or(|l| id > l) {
        *last = Some(id.to_string());
    }
}

/// How long after a race nobody won, or a direct path lost, the candidates
/// are tried again: the network may have changed meanwhile.
pub const REPROBE: Duration = Duration::from_secs(10);

/// The longest a viewer waits between races: a phone off the tailnet asks
/// the registry for a ticket and tries each candidate this often, no more.
pub const REPROBE_MAX: Duration = Duration::from_secs(300);

/// Batches a move to a direct path may hold before its welcome.
pub const HELD_MAX: usize = 1024;

/// The next race's time, `backoff` after `now`, and the wait after it.
pub fn later(now: Instant, backoff: Duration) -> (Instant, Duration) {
    (now + backoff, (backoff * 2).min(REPROBE_MAX))
}

/// How long one candidate has to answer the join.
pub const PROBE: Duration = Duration::from_secs(3);

/// The events not seen before, each once, the newest id kept.
pub fn fresh_only(events: Vec<serde_json::Value>, seen: &mut HashSet<String>, last: &mut Option<String>) -> Vec<serde_json::Value> {
    events
        .into_iter()
        .filter(|e| {
            let Some(i) = e["id"].as_str() else { return false };
            if !seen.insert(i.to_string()) {
                return false;
            }
            newest(last, i);
            true
        })
        .collect()
}

/// Whether a catch-up from `a` reaches back at least as far as one from
/// `b`: ids are UUIDv7, compared as strings as `newest` does, and `None`
/// (from the start) is older than any.
fn no_newer(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => a <= b,
    }
}

/// The catch-up asked and not yet answered, and one owed after it. Every
/// gap is caught up: by the one in flight when that one surely covers it,
/// else by one asked as soon as nothing is in flight.
#[derive(Debug, Default)]
pub struct CatchUps {
    in_flight: Option<Option<String>>,
    owed: Option<Option<String>>,
}

impl CatchUps {
    /// A gap from `after`: the `after` to ask from now, if any.
    /// `covered_by_order` is true for a batch replayed from `held` at a
    /// welcome: it arrived before the welcome, so the catch-up asked after
    /// that welcome was answered from a log that holds its events.
    pub fn gap(&mut self, after: Option<String>, covered_by_order: bool) -> Option<Option<String>> {
        let Some(flying) = &self.in_flight else { return Some(after) };
        if covered_by_order && no_newer(flying, &after) {
            return None;
        }
        self.owe(after);
        None
    }

    /// Owed: the oldest, which covers the later ones.
    fn owe(&mut self, after: Option<String>) {
        if self.owed.as_ref().is_none_or(|o| !no_newer(o, &after)) {
            self.owed = Some(after);
        }
    }

    /// A host's welcome: the catch-up to ask after it, from the newest
    /// event held, or from further back when one asked on an earlier link
    /// was never answered or one is owed. Now in flight.
    pub fn welcomed(&mut self, last: Option<String>) -> Option<String> {
        self.reset();
        self.owe(last);
        let after = self.owed.take().expect("owed");
        self.asked(after.clone());
        after
    }

    /// Asked (after a welcome, a resync, or a gap): now in flight from
    /// `after`.
    pub fn asked(&mut self, after: Option<String>) {
        self.in_flight = Some(after);
    }

    /// The last page came: what to ask next, if a gap was owed.
    pub fn done(&mut self) -> Option<Option<String>> {
        self.in_flight = None;
        self.owed.take()
    }

    /// Joined again: nothing in flight. The one that was is owed, since
    /// its answer, sealed for the link that asked it, never opens on the
    /// next.
    pub fn reset(&mut self) {
        if let Some(after) = self.in_flight.take() {
            self.owe(after);
        }
    }

    /// The oldest `after` asked and not yet answered, or owed: every event
    /// after it may not be held yet.
    pub fn outstanding(&self) -> Option<&Option<String>> {
        match (&self.in_flight, &self.owed) {
            (Some(a), Some(b)) => Some(if no_newer(a, b) { a } else { b }),
            (a, b) => a.as_ref().or(b.as_ref()),
        }
    }

    #[cfg(test)]
    fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// A gap from `after`, found as a batch applied: shown, once, and the
/// `after` of the catch-up to ask now, if any.
pub fn gap_found(c: &mut CatchUps, after: Option<String>, covered_by_order: bool, frame: &mut Vec<Update>) -> Option<Option<String>> {
    frame.push(Update::Gap);
    c.gap(after, covered_by_order)
}

/// The batches held for a welcome, applied in order, each gap among them
/// shown once. Answers the first gap's `after`: the oldest, which covers
/// the later ones.
pub fn replay(l: &mut ViewerLink, held: Vec<Vec<u8>>, seen: &mut HashSet<String>, last: &mut Option<String>, asked: &mut HashSet<String>, frame: &mut Vec<Update>, applied: &mut u64) -> Option<Option<String>> {
    let mut first = None;
    for b in held {
        if let Some(after) = apply(l, &b, seen, last, asked, frame, applied) {
            frame.push(Update::Gap);
            first.get_or_insert(after);
        }
    }
    first
}

/// One stream batch: its lines, the log's events among them once each,
/// and `applied` moved to it once they are in `frame`. Answers, when it
/// skipped past a batch this viewer never opened, the newest event held
/// *before* it: what the catch-up asks from, since this batch's own events
/// are newer than the ones the gap swallowed. A batch that opens but does
/// not parse is a gap too: its stream's counter refuses it if it comes
/// again, so a catch-up is all that brings its events.
pub fn apply(l: &mut ViewerLink, b: &[u8], seen: &mut HashSet<String>, last: &mut Option<String>, asked: &mut HashSet<String>, frame: &mut Vec<Update>, applied: &mut u64) -> Option<Option<String>> {
    let before = last.clone();
    let Ok((body, end, skipped)) = l.open_batch_gap(b) else { return None };
    let seq = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
    // Against the last batch opened, or — the first since joining — the
    // relay's count at the join: either way the batch must be the next.
    let gap = (skipped || seq != *applied + 1).then(|| before.clone());
    let Ok(batch) = serde_json::from_slice::<Batch>(&body) else {
        *applied = seq;
        return Some(before);
    };
    for line in batch.lines {
        match &line {
            StreamLine::Log(e) => {
                if !seen.insert(e.id.clone()) {
                    continue;
                }
                newest(last, &e.id);
            }
            StreamLine::Live(LiveEvent::ApprovalRequested(r)) if !asked.insert(r.request_id.clone()) => continue,
            StreamLine::Live(LiveEvent::ApprovalResolved { request_id, .. }) => {
                asked.remove(request_id);
            }
            // A turn's end resolves whatever it still asked.
            StreamLine::Live(LiveEvent::Result(_)) => asked.clear(),
            _ => {}
        }
        frame.push(Update::Line(line));
    }
    if end {
        frame.push(Update::Host(false));
    }
    *applied = seq;
    gap
}

/// What the caller of a `ViewerCore` does next, in the order given.
#[derive(Debug, Clone)]
pub enum Effect {
    /// A binary WebSocket message on the current connection.
    Send(Vec<u8>),
    /// The text message `ping` on the current connection.
    Ping,
    /// Updates for the screen: at most one `Show` per `FRAME` (R-LAG-7),
    /// but for the last one, which says `Failed` before `End`. One that
    /// moved the viewer's place ends with `Saved`.
    Show(Vec<Update>),
    /// Read the chunks written since `attached()` (`store::catch_up` on a
    /// clone of it, blocking, off the main thread), up to `want` when
    /// given, and hand the result to `chunks_read`. Until then the core
    /// holds the stream's batches with those held for the welcome, and
    /// keeps whatever else the connections bring to act on after them, in
    /// order.
    ReadChunks { want: Option<Head> },
    /// Drop the current connection and any leaving one, and pass nothing
    /// more from them (a late close included); at `at_ms` fetch a fresh
    /// relay ticket and join as a viewer with `extra`
    /// (`sync::join::Joining`), then call `joined`, or `join_failed` with
    /// why the ticket or the join failed. A later `Join` replaces one not
    /// yet run.
    Join { extra: serde_json::Value, at_ms: u64 },
    /// Race `candidates()` for a direct path, each joined with
    /// `cursor_extra()` and given `PROBE` to answer; then `move_to` with
    /// the first that joined, or `probe_lost` when none did.
    Probe,
    /// Drop the leaving connection: the move to a direct path is done.
    Leave,
    /// The viewer cannot go on (the `Show` before it says why).
    End,
}

/// An input from the person or a connection. While a `ReadChunks` is out,
/// the stream's batches are held for the welcome and a connection's other
/// inputs are kept, to be acted on in order once it is answered and the
/// held batches replayed, as the harness's loop reads nothing else while it
/// reads the chunks; the person's go at once, queued for the welcome.
#[derive(Debug)]
enum Input {
    Received(Vec<u8>),
    Leaving(Vec<u8>),
    Closed,
    BackToRelay,
    Command(Command),
    Handoff,
}

/// The read a `ReadChunks` is for: a welcome's prefix check, whose welcome
/// is acted on once it is answered, or a resync with no host.
#[derive(Debug)]
enum Reading {
    Welcome(Welcome),
    Resync,
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// How long a viewer waits to join again after the relay closed it or
/// refused the join.
const RETRY_MS: u64 = 1000;

/// The longest a viewer waits to join again after the chunks a welcome
/// asked for could not be read: from `RETRY_MS`, doubling each time in a
/// row, up to this.
pub const REREAD_MAX: Duration = Duration::from_secs(30);

/// How a `Note` about a failed join ends: the cadence of `RETRY_MS`. A
/// caller that times the retries itself (the phone's `supervisor`) says
/// its own.
pub const EVERY_SECOND: &str = "; trying again every second";

/// A viewer of a synced session following its host live, without IO
/// (R-CLIENT-3; engineering/harness.md → Sync): the caller passes in what
/// arrived and the time, a monotonic `now_ms` from any origin, and gets
/// back the `Effect`s to carry out. It opens no socket, starts no thread
/// and reads no clock, so the phone owns its sockets in Swift and the
/// rules run under `cargo test`.
///
/// It joins the relay, says hello, holds batches until the welcome, reads
/// the chunks up to the welcome's head (the prefix check), applies each
/// batch once and acks the relay, catches up on gaps and resyncs, follows
/// presence, sends commands again after a reconnect, and hands the screen
/// one `Show` per display frame. Its timers are `tick`'s: the frame, the
/// heartbeat, a move's welcome and the next race for a direct path, each
/// due by `next_wake`.
pub struct ViewerCore {
    key: SessionKeys,
    raw: [u8; 16],
    relay: String,
    at_rest: Attached,
    seen: HashSet<String>,
    last_id: Option<String>,
    /// Approval requests shown and not yet resolved, by request id: each
    /// welcome names the waiting ones and the stream carries them too.
    asked: HashSet<String>,
    frame: Vec<Update>,
    shown_ms: u64,
    book: CommandBook,
    held: Vec<Vec<u8>>,
    link: Option<ViewerLink>,
    /// The cursor the link had before it was last reconnected: a link not
    /// welcomed yet names no stream, and a join before its welcome still
    /// resumes from here.
    resume: Option<([u8; 16], u64)>,
    /// The stream the relay's `joined` named: the one `applied` counts in
    /// until a batch of it is opened.
    joined_on: Option<[u8; 16]>,
    /// Whether the caller holds a current connection: from `joined` (or a
    /// `move_to`) until a `Join`.
    connected: bool,
    host: bool,
    /// Why the relay could not be joined, said once as it comes or changes.
    unjoined: Option<String>,
    /// Why the chunks a welcome asked for could not be read, said likewise,
    /// and the wait before the next join after one more such failure.
    unread: Option<String>,
    reread_ms: u64,
    heard_ms: u64,
    last_ping_ms: u64,
    // Direct paths (R-NET-2): the candidates the host's welcome named, a
    // race out, the next one's time and the wait after it, the one being
    // moved onto and by when its welcome must come, whether the relay's
    // connection is still read meanwhile, and the one the session is on.
    candidates: Vec<Candidate>,
    probing: bool,
    reprobe_ms: u64,
    backoff_ms: u64,
    moving: Option<Candidate>,
    moving_until_ms: u64,
    leaving: bool,
    on: Option<Candidate>,
    /// The last stream batch applied, the last whose updates went out in a
    /// `Show`, and the last acked to the relay: only a batch shown is.
    applied: u64,
    shown: u64,
    acked: u64,
    /// The place the last `Saved` named.
    saved: Option<Cursor>,
    catch_ups: CatchUps,
    reading: Option<Reading>,
    waiting: VecDeque<Input>,
    now_ms: u64,
    ended: bool,
    out: Vec<Effect>,
}

impl ViewerCore {
    /// A viewer of `session`, attached at rest (`store::attach`): shows
    /// what the chunks hold, as a display frame shown at `now_ms`, and
    /// joins the relay now.
    pub fn new(key: SessionKeys, session: &str, device: DeviceId, relay: &str, at_rest: Attached, now_ms: u64) -> (ViewerCore, Vec<Effect>) {
        let seen: HashSet<String> = at_rest.events.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect();
        let mut last_id = None;
        for id in &seen {
            newest(&mut last_id, id);
        }
        let first = Effect::Show(vec![Update::Attached { events: at_rest.events.clone(), head: at_rest.head }]);
        let mut v = ViewerCore {
            key,
            raw: super::uuid(session),
            relay: relay.to_string(),
            at_rest,
            seen,
            last_id,
            asked: HashSet::new(),
            frame: Vec::new(),
            shown_ms: now_ms,
            book: CommandBook::new(device),
            held: Vec::new(),
            link: None,
            resume: None,
            joined_on: None,
            connected: false,
            host: false,
            unjoined: None,
            unread: None,
            reread_ms: RETRY_MS,
            heard_ms: now_ms,
            last_ping_ms: now_ms,
            candidates: Vec::new(),
            probing: false,
            reprobe_ms: now_ms,
            backoff_ms: ms(REPROBE),
            moving: None,
            moving_until_ms: now_ms,
            leaving: false,
            on: None,
            applied: 0,
            shown: 0,
            acked: 0,
            saved: None,
            catch_ups: CatchUps::default(),
            reading: None,
            waiting: VecDeque::new(),
            now_ms,
            ended: false,
            out: vec![first],
        };
        v.join(now_ms);
        let out = v.take();
        (v, out)
    }

    /// The session as read at rest so far: what a `ReadChunks` reads on
    /// from.
    pub fn attached(&self) -> &Attached {
        &self.at_rest
    }

    /// The direct paths the host's welcome named (R-NET-1).
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// Whether a race for a direct path may start: welcomed on the relay,
    /// with candidates, no race out, no read out.
    pub fn may_probe(&self) -> bool {
        self.on.is_none() && self.moving.is_none() && !self.probing && !self.candidates.is_empty() && self.connected && self.reading.is_none() && self.welcomed()
    }

    /// What a join carries to resume where this viewer stopped: the stream
    /// and the last batch it opened (`ViewerLink::cursor`), else nothing.
    pub fn cursor_extra(&self) -> serde_json::Value {
        match self.cursor() {
            Some((s, n)) => serde_json::json!({"stream": e2e::hex(&s), "afterSeq": n}),
            None => serde_json::json!({}),
        }
    }

    /// When `tick` next has something to do, which may be past already:
    /// then tick at once. None once ended. Ask again after every call.
    pub fn next_wake(&self) -> Option<u64> {
        if self.ended {
            return None;
        }
        let mut at = self.last_ping_ms.saturating_add(ms(PING));
        if !self.frame.is_empty() || (self.applied > self.acked && self.connected) || self.place() != self.saved {
            at = at.min(self.shown_ms.saturating_add(ms(FRAME)));
        }
        if self.moving.is_some() && self.reading.is_none() {
            at = at.min(self.moving_until_ms);
        }
        if self.may_probe() {
            at = at.min(self.reprobe_ms);
        }
        Some(at)
    }

    /// On the relay (`joined` as the relay answered the join): hello said
    /// to the host there, if one is.
    pub fn joined(&mut self, joined: &serde_json::Value, now_ms: u64) -> Vec<Effect> {
        if self.ended {
            return Vec::new();
        }
        self.now_ms = now_ms;
        self.unjoined = None;
        (self.on, self.moving, self.leaving) = (None, None, false);
        // A read out belonged to the connection before; none is, as the
        // caller only joins after a `Join`.
        (self.reading, self.waiting) = (None, VecDeque::new());
        let n = joined["link"].as_u64().unwrap_or(0);
        // The cursor this join resumed from, kept while the reconnected
        // link names no stream — but only on the stream the relay carries:
        // another one's would make every join until a welcome a resync.
        let cursor = self.cursor().filter(|(s, _)| joined["stream"].as_str() == Some(e2e::hex(s).as_str()));
        self.resume = cursor;
        self.joined_on = joined["stream"].as_str().and_then(e2e::unhex).and_then(|b| b.try_into().ok());
        let mut l = match self.link.take() {
            Some(l) => l.reconnect(n),
            None => ViewerLink::new(self.key.current(), self.raw, n),
        };
        self.host = joined["host"].as_bool().unwrap_or(false);
        self.held.clear();
        self.catch_ups.reset();
        self.connected = true;
        self.heard_ms = now_ms;
        if self.host && let Ok(h) = l.hello(b"{}") {
            self.out.push(Effect::Send(h));
        }
        // Where the stream stands for this viewer: its own cursor on a
        // stream it read (which the relay resumes after), else the relay's
        // count, live from the next batch. The cursor is the one it joined
        // with: the link, reconnected, names no stream until its welcome.
        self.applied = match cursor {
            Some((_, n)) => n,
            None => joined["seq"].as_u64().unwrap_or(0),
        };
        (self.shown, self.acked) = (self.applied, self.applied);
        self.link = Some(l);
        if !self.host {
            self.frame.push(Update::Host(false));
        }
        self.take()
    }

    /// The relay could not be joined (the ticket, the dial or the join):
    /// said once as it comes or changes, and joined again a second later.
    pub fn join_failed(&mut self, error: String, now_ms: u64) -> Vec<Effect> {
        if self.ended {
            return Vec::new();
        }
        self.now_ms = now_ms;
        if self.unjoined.as_deref() != Some(error.as_str()) {
            self.frame.push(Update::Note(format!("not on the relay {}: {error}{EVERY_SECOND}", self.relay)));
            self.unjoined = Some(error);
        }
        self.join(now_ms.saturating_add(RETRY_MS));
        self.take()
    }

    /// A binary message on the current connection: a relay control message
    /// (kind 4) or an envelope.
    pub fn received(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Effect> {
        self.input(now_ms, true, Input::Received(bytes.to_vec()))
    }

    /// Any other message on the current connection (`pong`): it lives.
    pub fn alive(&mut self, now_ms: u64) {
        if !self.ended {
            self.now_ms = now_ms;
            self.heard_ms = now_ms;
        }
    }

    /// The current connection closed.
    pub fn closed(&mut self, now_ms: u64) -> Vec<Effect> {
        self.input(now_ms, true, Input::Closed)
    }

    /// A binary message on the leaving connection: the relay's, while the
    /// session moves off it to a direct path. Once that one closes the
    /// caller drops it, and says nothing.
    pub fn leaving(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Effect> {
        self.input(now_ms, false, Input::Leaving(bytes.to_vec()))
    }

    /// The direct path was lost, or its welcome is late by the caller's
    /// reckoning: the relay joined again, from the cursor, at once.
    pub fn back_to_relay(&mut self, now_ms: u64) -> Vec<Effect> {
        self.input(now_ms, false, Input::BackToRelay)
    }

    /// A command of the person's (R-LAG-8): shown at once, sent once a host
    /// has welcomed this viewer, queued for the next welcome until then
    /// (R-HAND-4).
    pub fn command(&mut self, c: Command, now_ms: u64) -> Vec<Effect> {
        self.input(now_ms, false, Input::Command(c))
    }

    /// Asks the host to hand the session to this device (R-HAND-1), now if
    /// a host has welcomed this viewer, else at the next welcome.
    pub fn ask_handoff(&mut self, now_ms: u64) -> Vec<Effect> {
        self.input(now_ms, false, Input::Handoff)
    }

    /// The chunks a `ReadChunks` read: the events in them and where the
    /// reading stopped, or why it failed. A failure the read calls
    /// permanent (`ReadError::permanent`: the session gone, its chunks not
    /// opening under this device's keys, a rollback) ends the viewer. A
    /// transient one during a welcome joins the relay again, from the
    /// cursor, after a wait that doubles each time in a row up to
    /// `REREAD_MAX`; during a resync with no host it is let go.
    pub fn chunks_read(&mut self, read: Result<(Vec<serde_json::Value>, Attached), ReadError>, now_ms: u64) -> Vec<Effect> {
        if self.ended {
            return Vec::new();
        }
        self.now_ms = now_ms;
        match self.reading.take() {
            None => {}
            Some(Reading::Resync) => match read {
                Ok((fresh, a)) => self.caught_up(fresh, a),
                Err(e) if e.permanent => self.fail(e.detail),
                Err(_) => {}
            },
            // The prefix check: the chunks must reach the head the host sealed.
            Some(Reading::Welcome(w)) => match read {
                Ok((fresh, a)) => {
                    (self.unread, self.reread_ms) = (None, RETRY_MS);
                    self.caught_up(fresh, a);
                    self.welcomed_with(w);
                }
                Err(e) if e.permanent => self.fail(e.detail),
                Err(e) => self.read_failed(e.detail),
            },
        }
        while self.reading.is_none() && !self.ended {
            let Some(w) = self.waiting.pop_front() else { break };
            self.run(w);
        }
        self.take()
    }

    /// The direct path a race found, `joined` as its listener answered: the
    /// same hello and welcome as on the relay, over it. With a `Send` (the
    /// hello) the candidate's connection is the current one from here and
    /// the relay's the leaving one, read by `leaving` until the welcome;
    /// with none the caller drops the candidate's connection.
    pub fn move_to(&mut self, c: Candidate, joined: &serde_json::Value, now_ms: u64) -> Vec<Effect> {
        if self.ended {
            return Vec::new();
        }
        self.now_ms = now_ms;
        self.probing = false;
        // Won while a welcome's read is out: the race again once it is in,
        // at the time it was due, with no wait added.
        if self.reading.is_some() {
            return self.take();
        }
        // A candidate joined with no host there (its uplink to the listener
        // not up yet), or the session moved meanwhile: later.
        if self.on.is_some() || self.moving.is_some() || !self.connected || joined["host"].as_bool() != Some(true) {
            self.later();
            return self.take();
        }
        self.resume = self.cursor();
        let Some(old) = self.link.take() else { return self.take() };
        let mut l = old.reconnect(joined["link"].as_u64().unwrap_or(0));
        let hello = l.hello(b"{}");
        self.link = Some(l);
        match hello {
            Ok(h) => {
                self.out.push(Effect::Send(h));
                self.leaving = true;
                self.heard_ms = now_ms;
                self.moving = Some(c);
                self.moving_until_ms = now_ms.saturating_add(ms(PROBE));
            }
            Err(_) => self.relay_again(),
        }
        self.take()
    }

    /// A race nobody won: the next one later, the wait doubled.
    pub fn probe_lost(&mut self, now_ms: u64) {
        if !self.ended {
            self.now_ms = now_ms;
            self.probing = false;
            self.later();
        }
    }

    /// The time passing: the frame shown if a display frame has passed
    /// since the last, ending with the place when it moved, the batches
    /// shown acked, a heartbeat every `PING` (or, with no word for `DEAD`,
    /// the relay joined again), a move whose welcome is late given up, and
    /// a race started when one is due.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Effect> {
        if self.ended {
            return Vec::new();
        }
        self.now_ms = now_ms;
        // Never two `Show`s inside one display frame, however often the
        // caller ticks (R-LAG-7).
        if now_ms >= self.shown_ms.saturating_add(ms(FRAME)) {
            let place = self.place();
            if place != self.saved
                && let Some(c) = place.clone()
            {
                self.frame.push(Update::Saved(c));
            }
            self.saved = place;
            if !self.frame.is_empty() {
                self.shown_ms = now_ms;
                self.out.push(Effect::Show(std::mem::take(&mut self.frame)));
            }
        }
        // With nothing waiting to be shown, every batch applied has been:
        // one whose lines were all seen before showed nothing.
        if self.frame.is_empty() {
            self.shown = self.applied;
        }
        if self.shown > self.acked && self.connected {
            self.acked = self.shown;
            self.out.push(Effect::Send(ack(&self.raw, self.shown)));
        }
        if now_ms >= self.last_ping_ms.saturating_add(ms(PING)) {
            self.last_ping_ms = now_ms;
            if self.connected {
                // The harness hears nothing while it reads the chunks, so
                // what it holds back meanwhile is no silence.
                if self.reading.is_none() && now_ms.saturating_sub(self.heard_ms) > ms(DEAD) {
                    self.join(now_ms);
                } else {
                    self.out.push(Effect::Ping);
                }
            }
        }
        if self.moving.is_some() && self.reading.is_none() && now_ms >= self.moving_until_ms {
            self.relay_again();
        }
        if self.may_probe() && now_ms >= self.reprobe_ms {
            self.probing = true;
            self.out.push(Effect::Probe);
        }
        self.take()
    }

    fn take(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.out)
    }

    /// An input; `heard` when it came on the current connection.
    fn input(&mut self, now_ms: u64, heard: bool, w: Input) -> Vec<Effect> {
        if self.ended {
            return Vec::new();
        }
        self.now_ms = now_ms;
        if heard {
            self.heard_ms = now_ms;
        }
        if self.reading.is_some() {
            match w {
                // A stream batch waits with those held for the welcome: it
                // came before the catch-up the welcome asks for once the
                // read is in, so that catch-up covers a gap among them.
                Input::Received(b) if self.connected && b.len() >= HEADER && b[1] == KIND_BATCH && b[20..28] != [0; 8] => self.held.push(b),
                // The person's: shown at once (R-LAG-8), and with the host's
                // welcome not acted on yet, queued for it.
                w @ (Input::Command(_) | Input::Handoff) => self.run(w),
                w => self.waiting.push_back(w),
            }
        } else {
            self.run(w);
        }
        self.take()
    }

    fn run(&mut self, w: Input) {
        match w {
            Input::Received(b) if self.connected => self.inbound(&b),
            Input::Leaving(b) if self.leaving => self.leaving_in(&b),
            // A direct path gone falls back to the relay at once, from the
            // cursor, which the relay's buffer fills (R-NET-2).
            Input::Closed if self.connected && (self.on.is_some() || self.moving.is_some()) => self.relay_again(),
            Input::Closed if self.connected => self.join(self.now_ms.saturating_add(RETRY_MS)),
            Input::BackToRelay if self.on.is_some() || self.moving.is_some() => self.relay_again(),
            Input::Command(c) => {
                let (now, shown) = self.book.command(c, self.ready());
                self.frame.push(shown);
                if let Some(r) = now {
                    self.send_frame(&ViewerFrame::Command(r));
                }
            }
            Input::Handoff => {
                if let Some(f) = self.book.ask_handoff(self.ready()) {
                    self.send_frame(&f);
                }
            }
            Input::Received(_) | Input::Leaving(_) | Input::Closed | Input::BackToRelay => {}
        }
    }

    /// Drops the connections and joins the relay at `at_ms`, from the cursor.
    fn join(&mut self, at_ms: u64) {
        (self.connected, self.leaving, self.on, self.moving) = (false, false, None, None);
        self.out.push(Effect::Join { extra: self.cursor_extra(), at_ms });
    }

    /// Off a direct path, or one being moved onto: the relay joined again
    /// at once, and the next race put off.
    fn relay_again(&mut self) {
        self.join(self.now_ms);
        self.later();
    }

    fn later(&mut self) {
        self.reprobe_ms = self.now_ms.saturating_add(self.backoff_ms);
        self.backoff_ms = self.backoff_ms.saturating_mul(2).min(ms(REPROBE_MAX));
    }

    /// The stream this viewer reads and the last batch it opened of it:
    /// the link's, else the one it had before it was reconnected. Welcomed
    /// onto a stream it has opened nothing of yet (a new host's), it is
    /// where the relay stood for it at the join, not the stream's first
    /// batch, which would have the relay send it everything it holds of
    /// that stream again (ticket 115).
    fn cursor(&self) -> Option<([u8; 16], u64)> {
        match self.link.as_ref().and_then(|l| l.cursor()) {
            Some((s, 0)) if self.joined_on == Some(s) => Some((s, self.applied)),
            Some(c) => Some(c),
            None => self.resume,
        }
    }

    /// Where this viewer stands, as `Saved` names it: the cursor, and the
    /// newest event held, or the oldest a catch-up still out asks from.
    fn place(&self) -> Option<Cursor> {
        let (stream, after_seq) = self.cursor()?;
        let newest = match self.catch_ups.outstanding() {
            Some(after) if no_newer(after, &self.last_id) => after.clone(),
            _ => self.last_id.clone(),
        };
        Some(Cursor { stream: e2e::hex(&stream), after_seq, newest })
    }

    fn welcomed(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.welcomed())
    }

    /// Whether a host has welcomed this connection, the welcome has been
    /// acted on and the connection is still there: a command or handoff
    /// goes now, else at the next welcome.
    fn ready(&self) -> bool {
        self.host && self.welcomed() && self.reading.is_none() && self.connected
    }

    fn send(&mut self, bytes: Vec<u8>) {
        if self.connected {
            self.out.push(Effect::Send(bytes));
        }
    }

    /// One frame sealed and sent to the host.
    fn send_frame(&mut self, f: &ViewerFrame) {
        let Some(l) = self.link.as_mut() else { return };
        if let Ok(sealed) = l.frame(&serde_json::to_vec(f).expect("json"), false) {
            self.send(sealed);
        }
    }

    /// The chunks a welcome asked for could not be read, for now (a registry
    /// hiccup, the network): the welcome is not acted on, and the relay is
    /// joined again from the cursor, where the host welcomes this viewer
    /// again. The batches held for it and what waited on the read go with
    /// the connection: the relay sends the batches again after the cursor,
    /// and nothing else on a connection dropped is acted on. The wait
    /// doubles each time in a row, so a registry that stays down is never
    /// asked in a loop; a welcome whose read goes through starts it over.
    fn read_failed(&mut self, e: String) {
        if self.unread.as_deref() != Some(e.as_str()) {
            self.frame.push(Update::Note(format!("could not read the session's log: {e}; joining the relay again")));
            self.unread = Some(e);
        }
        (self.held, self.waiting) = (Vec::new(), VecDeque::new());
        let wait = self.reread_ms;
        self.reread_ms = self.reread_ms.saturating_mul(2).min(ms(REREAD_MAX));
        // Welcomed onto a direct path: the next race put off, as when one
        // is lost, or a registry that fails now and then would have the
        // viewer race, move and fall back again every few seconds.
        if self.on.is_some() {
            self.later();
        }
        self.join(self.now_ms.saturating_add(wait));
    }

    /// The viewer cannot go on: shown at once, then the end.
    fn fail(&mut self, e: String) {
        self.frame.push(Update::Failed(e));
        self.out.push(Effect::Show(std::mem::take(&mut self.frame)));
        self.out.push(Effect::End);
        self.ended = true;
    }

    /// What came on the current connection.
    fn inbound(&mut self, b: &[u8]) {
        if b.len() < HEADER {
            return;
        }
        if b[1] == KIND_RELAY {
            match read_control(b) {
                Some(v) if v["type"] == "host" => self.presence(&v),
                // A frame sent as the host went: dropped by the relay, so
                // the host is not there, and what was sent waits,
                // unacknowledged, for the welcome of the next.
                Some(v) if v["type"] == "error" && v["code"] == "host_absent" => {
                    if self.host {
                        self.host = false;
                        self.frame.push(Update::Host(false));
                    }
                }
                Some(v) if v["type"] == "resync" => self.resync(&v),
                _ => {}
            }
        } else if b[1] == KIND_BATCH {
            self.batch(b);
        }
    }

    /// Presence is the relay's word, not the host's. A host arriving is
    /// joined again and said hello to: its welcome sends again every
    /// command not yet acknowledged, and none runs twice.
    fn presence(&mut self, v: &serde_json::Value) {
        let present = v["present"].as_bool().unwrap_or(false);
        let direct = self.on.is_some() || self.moving.is_some();
        if !present && direct {
            // The host's uplink to its direct listener went, not the host:
            // back to the relay, where it may still be.
            self.relay_again();
        } else if present && (!self.welcomed() || !direct) {
            self.join(self.now_ms);
        } else if !present {
            self.host = false;
            self.frame.push(Update::Host(false));
        }
    }

    /// The relay cannot carry the stream on for this viewer.
    fn resync(&mut self, v: &serde_json::Value) {
        if v["reason"] == "stream" {
            // Another stream: a new host, or one that could not carry its
            // count on. Join again and be welcomed onto it.
            self.join(self.now_ms);
        } else if self.welcomed() {
            // The relay cannot fill the gap: the host can, now, or once the
            // catch-up in flight ends.
            if let Some(after) = self.catch_ups.gap(self.last_id.clone(), false) {
                self.ask_catch_up(after);
            }
        } else {
            // No host: the chunks, up to what was written.
            self.reading = Some(Reading::Resync);
            self.out.push(Effect::ReadChunks { want: None });
        }
    }

    /// What the chunks held that was not seen yet, shown.
    fn caught_up(&mut self, fresh: Vec<serde_json::Value>, a: Attached) {
        let fresh = fresh_only(fresh, &mut self.seen, &mut self.last_id);
        self.at_rest = a;
        if !fresh.is_empty() {
            self.frame.push(Update::CaughtUp(fresh));
        }
    }

    /// A batch on the stream, or one routed to this viewer.
    fn batch(&mut self, b: &[u8]) {
        let Some(l) = self.link.as_mut() else { return };
        if b[20..28] == [0; 8] {
            return self.routed(b);
        }
        if !l.welcomed() {
            self.held.push(b.to_vec());
            if self.moving.is_some() && self.held.len() > HELD_MAX {
                self.relay_again();
            }
        } else if let Some((_, have)) = l.cursor() && seq_of(b) <= have {
            // A batch this viewer opened already, by the other uplink: acked
            // all the same, as a direct listener counts its window in what
            // it sent — up to the last shown, and the rest once it is.
            if self.connected {
                self.acked = self.acked.max(self.shown);
                self.out.push(Effect::Send(ack(&self.raw, self.shown)));
            }
        } else {
            let gap = apply(l, b, &mut self.seen, &mut self.last_id, &mut self.asked, &mut self.frame, &mut self.applied);
            // A batch the relay dropped: the host fills the gap, from the
            // newest event held before this batch, now or once the
            // catch-up in flight ends.
            if let Some(after) = gap.and_then(|after| gap_found(&mut self.catch_ups, after, false, &mut self.frame)) {
                self.ask_catch_up(after);
            }
            // Only what a `Show` handed the screen is acked: the rest at
            // the frame that shows it.
            if self.shown >= self.acked + ACK_EVERY && self.connected {
                self.acked = self.shown;
                self.out.push(Effect::Send(ack(&self.raw, self.shown)));
            }
        }
    }

    /// A batch routed to this viewer: the host's welcome, or its answer.
    fn routed(&mut self, b: &[u8]) {
        let Some(l) = self.link.as_mut() else { return };
        match l.open_routed(b) {
            Ok(Outbound::Welcome { body, .. }) => self.welcome(&body),
            Ok(Outbound::Routed { body, .. }) => self.answer(&body),
            Err(_) => {}
        }
    }

    /// The host's welcome: the path shown, and the chunks read up to the
    /// head it sealed before anything else is acted on.
    fn welcome(&mut self, body: &[u8]) {
        let w: Welcome = serde_json::from_slice(body).unwrap_or_default();
        if let Some(c) = self.moving.take() {
            if std::mem::take(&mut self.leaving) {
                self.out.push(Effect::Leave);
            }
            self.backoff_ms = ms(REPROBE);
            self.on = Some(c);
        } else if self.on.is_none() {
            self.candidates = w.candidates.clone();
        }
        self.frame.push(match &self.on {
            Some(c) => Update::Path { path: c.path().into(), via: Some(c.url.clone()) },
            None => Update::Path { path: "relay".into(), via: None },
        });
        self.out.push(Effect::ReadChunks { want: w.head });
        self.reading = Some(Reading::Welcome(w));
    }

    /// The welcome, once the chunks reach its head: the host shown, its
    /// approvals, the catch-up and the commands, then the batches held.
    fn welcomed_with(&mut self, w: Welcome) {
        self.host = true;
        self.frame.push(Update::Host(true));
        for r in w.approvals {
            if self.asked.insert(r.request_id.clone()) {
                self.frame.push(Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))));
            }
        }
        if self.link.is_none() {
            return;
        }
        // What the chunks do not hold yet — the running turn's events — the
        // host sends. What was sent before and never acked goes again, in
        // the order first sent; then what waited for a host, in order; then
        // a handoff asked.
        let mut sends = vec![ViewerFrame::CatchUp { after: self.catch_ups.welcomed(self.last_id.clone()) }];
        sends.extend(self.book.welcomed().into_iter().map(ViewerFrame::Command));
        sends.extend(self.book.handoff_frame());
        for f in &sends {
            self.send_frame(f);
        }
        // Held batches arrived before the welcome, so the catch-up just
        // asked covers a gap among them (by order).
        let Some(l) = self.link.as_mut() else { return };
        let held = std::mem::take(&mut self.held);
        if let Some(after) = replay(l, held, &mut self.seen, &mut self.last_id, &mut self.asked, &mut self.frame, &mut self.applied).and_then(|after| self.catch_ups.gap(after, true)) {
            self.ask_catch_up(after);
        }
        // What was held may all have been batches opened already: acked all
        // the same, up to the last shown, or a direct listener, its window
        // full, sends no more. What is not shown yet is acked once it is.
        if self.link.as_ref().and_then(|l| l.cursor()).is_some() && self.connected {
            self.acked = self.acked.max(self.shown);
            self.out.push(Effect::Send(ack(&self.raw, self.shown)));
        }
    }

    /// Asks the host for the logged events after `after`: a catch-up now
    /// in flight.
    fn ask_catch_up(&mut self, after: Option<String>) {
        let Some(l) = self.link.as_mut() else { return };
        let Ok(f) = l.frame(&serde_json::to_vec(&ViewerFrame::CatchUp { after: after.clone() }).expect("json"), false) else { return };
        self.catch_ups.asked(after);
        self.send(f);
    }

    /// The host's answer to this viewer: a command's ack, a handoff's, or a
    /// page of the catch-up it asked for.
    fn answer(&mut self, body: &[u8]) {
        match serde_json::from_slice(body) {
            Ok(Answer::Ack { id, error }) => self.frame.push(self.book.acked(id, error)),
            Ok(Answer::Handoff { id, handed, error }) => self.frame.extend(self.book.handoff_answered(&id, handed, error)),
            Ok(Answer::CatchUp { events, more }) => {
                let events: Vec<serde_json::Value> = events.iter().filter_map(|e| serde_json::to_value(e).ok()).collect();
                let fresh = fresh_only(events, &mut self.seen, &mut self.last_id);
                if !fresh.is_empty() {
                    self.frame.push(Update::CaughtUp(fresh));
                }
                // The last page: a gap it did not cover is asked now.
                if !more && let Some(after) = self.catch_ups.done() {
                    self.ask_catch_up(after);
                }
            }
            Err(_) => {}
        }
    }

    /// The relay's connection, while the session moves off it: its batches
    /// are the stream's too, and a gap among them is caught up like one on
    /// the live path.
    fn leaving_in(&mut self, b: &[u8]) {
        if b.len() < HEADER || b[1] != KIND_BATCH || b[20..28] == [0; 8] {
            return;
        }
        match self.link.as_mut() {
            // Kept as the harness has it, though the direct welcome lets
            // the relay's connection go as it welcomes the link.
            Some(l) if l.welcomed() => {
                let gap = apply(l, b, &mut self.seen, &mut self.last_id, &mut self.asked, &mut self.frame, &mut self.applied);
                if let Some(after) = gap.and_then(|after| gap_found(&mut self.catch_ups, after, false, &mut self.frame)) {
                    self.ask_catch_up(after);
                }
            }
            _ => {
                self.held.push(b.to_vec());
                if self.held.len() > HELD_MAX {
                    self.relay_again();
                }
            }
        }
    }
}

fn seq_of(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::SessionKey;
    use crate::protocol::frame::HEADER;
    use crate::relay_link::HostLink;
    use serde_json::json;

    /// R-SUB-9: a batch from a newer host, holding a line this viewer does
    /// not know, still applies every other line, and asks for no resync.
    #[test]
    fn r_sub_9_a_line_the_viewer_does_not_know_is_skipped_and_the_rest_of_its_batch_applies() {
        let key = SessionKey::generate();
        let (mut host, mut link) = (HostLink::new(&key, [7; 16]), ViewerLink::new(&key, [7; 16], 1));
        host.open(1, &link.hello(b"{}").unwrap()).unwrap();
        let w = host.welcome(1, b"{}").unwrap();
        link.open_routed(&w[HEADER..]).unwrap();
        let notice = |text: &str| json!({"type": "notice", "sessionId": "s", "turnId": "t", "text": text});
        let body = json!({"lines": [notice("before"), {"type": "subagent.someday", "sessionId": "kid", "what": 1}, {"type": "notice", "sessionId": "s"}, notice("after")], "head": null});
        let b = host.batch(&serde_json::to_vec(&body).unwrap(), false).unwrap();
        let (mut seen, mut last, mut asked, mut frame, mut applied) = (HashSet::new(), None, HashSet::new(), Vec::new(), 0);
        let gap = apply(&mut link, &b, &mut seen, &mut last, &mut asked, &mut frame, &mut applied);
        assert_eq!(gap, None, "opened, in order: no resync asked for");
        let texts: Vec<&str> = frame.iter().filter_map(|u| match u { Update::Line(StreamLine::Live(LiveEvent::Notice { text, .. })) => Some(text.as_str()), _ => None }).collect();
        assert_eq!(texts, ["before", "after"]);
        assert_eq!(frame.len(), 2, "the unknown line, and the one missing its fields, are skipped");
    }

    /// A logged event, its id the UUIDv7-shaped `01900000-…-n`.
    fn logged(n: u8) -> serde_json::Value {
        json!({"id": id(n), "sessionId": "s", "timeMs": 0, "type": "session.started", "cwd": "/", "krowkVersion": "0", "protocolVersion": 1})
    }

    fn id(n: u8) -> String {
        format!("01900000-0000-7000-8000-0000000000{n:02}")
    }

    /// A host and a welcomed viewer, no network, and the host's batches
    /// 1, 2 and 3, each one logged event (ids 1, 2, 3).
    fn three_batches() -> (ViewerLink, [Vec<u8>; 3]) {
        let key = SessionKey::generate();
        let (mut host, mut link) = (HostLink::new(&key, [7; 16]), ViewerLink::new(&key, [7; 16], 1));
        host.open(1, &link.hello(b"{}").unwrap()).unwrap();
        let w = host.welcome(1, b"{}").unwrap();
        link.open_routed(&w[HEADER..]).unwrap();
        let mut batch = |n: u8| host.batch(&serde_json::to_vec(&json!({"lines": [logged(n)], "head": null})).unwrap(), false).unwrap();
        let b = [batch(1), batch(2), batch(3)];
        (link, b)
    }

    fn gaps(frame: &[Update]) -> usize {
        frame.iter().filter(|u| matches!(u, Update::Gap)).count()
    }

    /// R-LAG-4: the ack is the envelope header alone, in the clear — kind
    /// 3, no flags, not sealed, the session's bytes and the last batch
    /// applied, big-endian — so the relay reads it without the key.
    #[test]
    fn r_lag_4_the_ack_is_kind_3_with_the_last_seq() {
        let session: [u8; 16] = std::array::from_fn(|i| i as u8 + 1);
        let b = ack(&session, 0x0102_0304_0506_0708);
        assert_eq!(b.len(), HEADER, "the header and nothing after it");
        assert_eq!(&b[..4], &[crate::protocol::frame::V, 3, 0, 0], "v, kind 3, flags, enc");
        assert_eq!(b[1], KIND_ACK);
        assert_eq!(&b[4..20], &session);
        assert_eq!(&b[20..28], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// An event with no id is dropped, and one already seen, even within
    /// the same call; the newest id is kept whichever order they came in.
    #[test]
    fn fresh_only_drops_events_without_an_id_and_repeats() {
        let mut seen = HashSet::from([id(1)]);
        let mut last = Some(id(1));
        let fresh = fresh_only(vec![logged(1), json!({"type": "notice"}), logged(3), logged(2), logged(3)], &mut seen, &mut last);
        let ids: Vec<&str> = fresh.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, [id(3), id(2)]);
        assert_eq!(last, Some(id(3)));
        assert_eq!(seen.len(), 3);
        assert!(fresh_only(vec![logged(2)], &mut seen, &mut last).is_empty(), "seen in an earlier call");
    }

    /// The next race is `backoff` after the clock the caller passes, and
    /// the wait doubles up to `REPROBE_MAX`.
    #[test]
    fn later_counts_from_the_callers_clock_and_doubles_up_to_reprobe_max() {
        let now = Instant::now();
        assert_eq!(later(now, REPROBE), (now + REPROBE, REPROBE * 2));
        assert_eq!(later(now, Duration::from_secs(200)), (now + Duration::from_secs(200), REPROBE_MAX));
        assert_eq!(later(now, REPROBE_MAX).1, REPROBE_MAX);
    }

    /// R-CLIENT-1, R-PERM-2: an approval request the stream carries again —
    /// as every welcome's approvals and the stream both do — shows once,
    /// across batches; resolved, or once its turn's `result` comes, the
    /// same request id shows again.
    #[test]
    fn r_client_1_apply_shows_an_approval_once_until_its_turn_ends() {
        let key = SessionKey::generate();
        let (mut host, mut link) = (HostLink::new(&key, [7; 16]), ViewerLink::new(&key, [7; 16], 1));
        host.open(1, &link.hello(b"{}").unwrap()).unwrap();
        let w = host.welcome(1, b"{}").unwrap();
        link.open_routed(&w[HEADER..]).unwrap();
        let requested = |r: &str| json!({"type": "approval.requested", "sessionId": "s", "turnId": "t", "requestId": r, "tool": "Bash", "input": {}, "summary": "ls", "reason": "asked"});
        let resolved = |r: &str| json!({"type": "approval.resolved", "sessionId": "s", "turnId": "t", "requestId": r, "decision": "allow"});
        let result = json!({"type": "result", "sessionId": "s", "turnId": "t", "status": "completed", "isError": false, "result": "", "model": {"instance": "i", "model": "m"}, "usage": {"inputTokens": 0, "outputTokens": 0, "cacheReadTokens": 0, "cacheWriteTokens": 0, "reasoningTokens": 0}, "costUsd": null, "durationMs": 0, "numModelCalls": 0});
        let (mut seen, mut last, mut asked, mut applied) = (HashSet::new(), None, HashSet::new(), 0);
        let mut shown = |lines: Vec<serde_json::Value>| {
            let b = host.batch(&serde_json::to_vec(&json!({"lines": lines, "head": null})).unwrap(), false).unwrap();
            let mut frame = Vec::new();
            assert_eq!(apply(&mut link, &b, &mut seen, &mut last, &mut asked, &mut frame, &mut applied), None, "in order");
            frame
                .iter()
                .map(|u| match u {
                    Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) => format!("asked {}", r.request_id),
                    Update::Line(StreamLine::Live(LiveEvent::ApprovalResolved { request_id, .. })) => format!("resolved {request_id}"),
                    Update::Line(StreamLine::Live(LiveEvent::Result(_))) => "result".into(),
                    u => format!("{u:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shown(vec![requested("r1")]), ["asked r1"]);
        assert_eq!(shown(vec![requested("r1"), requested("r2")]), ["asked r2"], "r1 shown once, across batches");
        assert_eq!(shown(vec![resolved("r2"), requested("r2")]), ["resolved r2", "asked r2"], "resolved: asked again, shown again");
        assert_eq!(shown(vec![requested("r1")]), Vec::<String>::new());
        assert_eq!(shown(vec![result.clone()]), ["result"]);
        assert_eq!(shown(vec![requested("r1"), requested("r1")]), ["asked r1"], "the turn ended: shown again, once");
    }

    /// R-IOS-1: an event seen before does not move the newest event held,
    /// even one newer than it: the place names only what was applied.
    #[test]
    fn r_ios_1_a_duplicate_event_does_not_move_newest() {
        let (mut link, [b1, b2, _]) = three_batches();
        let (mut seen, mut last, mut asked, mut frame, mut applied) = (HashSet::from([id(2)]), Some(id(0)), HashSet::new(), Vec::new(), 0);
        apply(&mut link, &b1, &mut seen, &mut last, &mut asked, &mut frame, &mut applied);
        assert_eq!(last, Some(id(1)));
        apply(&mut link, &b2, &mut seen, &mut last, &mut asked, &mut frame, &mut applied);
        assert_eq!((last, applied, frame.len()), (Some(id(1)), 2, 1), "batch 2 applied, its seen event neither shown nor newest");
        let mut last = Some(id(0));
        assert!(fresh_only(vec![logged(2)], &mut seen, &mut last).is_empty());
        assert_eq!(last, Some(id(0)), "nor in a catch-up");
    }

    /// R-OFF-2: batches 1 and 3 held for a welcome whose catch-up went out
    /// from an older event: the gap shows once, and the welcome's catch-up
    /// covers it, so no second one is asked.
    #[test]
    fn r_off_2_a_gap_among_held_batches_shows_and_is_covered_by_the_welcomes_catch_up() {
        let (mut link, [b1, _, b3]) = three_batches();
        let mut c = CatchUps::default();
        // The harness's `send_welcomed`: the catch-up from the newest event the chunks held.
        assert_eq!(c.welcomed(Some(id(0))), Some(id(0)));
        let (mut seen, mut last, mut asked, mut frame, mut applied) = (HashSet::new(), Some(id(0)), HashSet::new(), Vec::new(), 0);
        let first = replay(&mut link, vec![b1, b3], &mut seen, &mut last, &mut asked, &mut frame, &mut applied);
        assert_eq!(first, Some(Some(id(1))), "the gap is after batch 1's event");
        assert_eq!(gaps(&frame), 1, "the gap shows once");
        assert_eq!(c.gap(first.unwrap(), true), None, "the welcome's catch-up covers it: none asked now");
        assert_eq!(c.done(), None, "nor once it ends");
        assert_eq!(applied, 3);
    }

    /// R-OFF-2: a gap found live while a catch-up is in flight is not
    /// covered by it (that one may have been answered before the missing
    /// batch was sealed): it is asked once the one in flight ends, from the
    /// oldest gap owed.
    #[test]
    fn r_off_2_a_live_gap_while_a_catch_up_is_in_flight_is_asked_once_it_ends() {
        let mut c = CatchUps::default();
        assert_eq!(c.gap(Some(id(5)), false), Some(Some(id(5))), "nothing in flight: asked now");
        c.asked(Some(id(5)));
        assert!(c.in_flight());
        assert_eq!(c.gap(Some(id(7)), false), None, "in flight: nothing asked now");
        assert_eq!(c.done(), Some(Some(id(7))), "asked once the one in flight ends");
        assert!(!c.in_flight());
        assert_eq!(c.done(), None, "and only once");

        // Two owed: the oldest is kept, whichever came first.
        c.asked(Some(id(1)));
        assert_eq!(c.gap(Some(id(8)), false), None);
        assert_eq!(c.gap(Some(id(6)), false), None);
        assert_eq!(c.gap(Some(id(9)), false), None);
        assert_eq!(c.done(), Some(Some(id(6))));

        // Joined again: nothing in flight, and what was is owed to the
        // next welcome's catch-up, which asks from the oldest.
        c.asked(Some(id(1)));
        c.gap(Some(id(2)), false);
        c.reset();
        assert!(!c.in_flight());
        assert_eq!(c.welcomed(Some(id(9))), Some(id(1)), "the unanswered one reaches back furthest");
        assert!(c.in_flight());
        assert_eq!(c.done(), None, "nothing owed after it");
        // Moved to a direct path (no rejoin): the same.
        c.asked(Some(id(3)));
        assert_eq!(c.welcomed(Some(id(9))), Some(id(3)));
        c.done();
        assert_eq!(c.welcomed(Some(id(9))), Some(id(9)), "with nothing unanswered, from the newest");
    }

    /// The in-flight catch-up covers a held batch's gap only when it reaches
    /// back as far: ids compared as strings, `None` from the start.
    #[test]
    fn r_off_2_a_catch_up_in_flight_covers_a_held_gap_only_from_no_newer_an_event() {
        let covers = |flying: Option<String>, gap: Option<String>| {
            let mut c = CatchUps::default();
            c.asked(flying);
            let now = c.gap(gap.clone(), true);
            assert_eq!(now, None, "never asked while one is in flight");
            match c.done() {
                None => true,
                Some(owed) => {
                    assert_eq!(owed, gap);
                    false
                }
            }
        };
        assert!(covers(None, None), "from the start covers from the start");
        assert!(covers(None, Some(id(3))), "from the start covers everything");
        assert!(!covers(Some(id(3)), None), "a gap from the start is covered only from the start");
        assert!(covers(Some(id(3)), Some(id(3))), "the same event");
        assert!(covers(Some(id(3)), Some(id(4))), "an older event");
        assert!(!covers(Some(id(4)), Some(id(3))), "a newer event does not");
        // Owed gaps keep the oldest, `None` the oldest of all.
        let mut c = CatchUps::default();
        c.asked(Some(id(5)));
        c.gap(Some(id(2)), false);
        c.gap(None, false);
        c.gap(Some(id(1)), false);
        assert_eq!(c.done(), Some(None));
    }

    /// R-NET-2: while the session moves to a direct path, a gap in the
    /// relay's batches shows and is caught up, through the helper
    /// the harness's `leaving_in` (and its live path) use.
    #[test]
    fn r_net_2_a_gap_in_the_relays_batches_while_moving_to_a_direct_path_is_caught_up() {
        let (mut link, [b1, _, b3]) = three_batches();
        let mut c = CatchUps::default();
        let (mut seen, mut last, mut asked, mut frame, mut applied) = (HashSet::new(), None, HashSet::new(), Vec::new(), 0);
        let ask = |link: &mut ViewerLink, b: &[u8], c: &mut CatchUps, seen: &mut _, last: &mut _, asked: &mut _, frame: &mut Vec<Update>, applied: &mut _| {
            apply(link, b, seen, last, asked, frame, applied).and_then(|after| gap_found(c, after, false, frame))
        };
        assert_eq!(ask(&mut link, &b1, &mut c, &mut seen, &mut last, &mut asked, &mut frame, &mut applied), None, "in order");
        assert_eq!(gaps(&frame), 0);
        assert_eq!(ask(&mut link, &b3, &mut c, &mut seen, &mut last, &mut asked, &mut frame, &mut applied), Some(Some(id(1))), "a catch-up from batch 1's event");
        assert_eq!(gaps(&frame), 1);

        // With one in flight already, the gap is still shown, and owed.
        let (mut link, [b1, _, b3]) = three_batches();
        let (mut seen, mut last, mut frame, mut applied) = (HashSet::new(), None, Vec::new(), 0);
        c.reset();
        c.asked(None);
        ask(&mut link, &b1, &mut c, &mut seen, &mut last, &mut asked, &mut frame, &mut applied);
        assert_eq!(ask(&mut link, &b3, &mut c, &mut seen, &mut last, &mut asked, &mut frame, &mut applied), None);
        assert_eq!(gaps(&frame), 1);
        assert_eq!(c.done(), Some(Some(id(1))));
        // Batches held until the direct welcome, a catch-up asked on the
        // relay still unanswered: the welcome's catch-up reaches back to
        // it, and covers the gap among them.
        let (mut link, [b1, _, b3]) = three_batches();
        let (mut seen, mut last, mut frame, mut applied) = (HashSet::new(), Some(id(0)), Vec::new(), 0);
        c.asked(None);
        assert_eq!(c.welcomed(Some(id(0))), None, "from the start, as the relay's was");
        let first = replay(&mut link, vec![b1, b3], &mut seen, &mut last, &mut asked, &mut frame, &mut applied);
        assert_eq!(gaps(&frame), 1);
        assert_eq!(c.gap(first.unwrap(), true), None);
        assert_eq!(c.done(), None);
    }
}
