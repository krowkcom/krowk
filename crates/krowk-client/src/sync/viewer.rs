//! What a viewer of a synced session shows, decided without IO
//! (R-CLIENT-1; engineering/harness.md → Sync): the updates a screen draws
//! from, a stream batch applied to them once, the logged events deduped,
//! the ack to the relay, the direct-path timings, and the catch-ups a gap
//! asks for. The harness's viewer runs them on its tokio loop; the phone's
//! viewer will run the same. The clock is the caller's: nothing here reads
//! it.

use super::store::Head;
use super::Batch;
use crate::protocol::frame::KIND_ACK;
use crate::protocol::{LiveEvent, StreamLine};
use crate::relay_link::ViewerLink;
use std::collections::HashSet;
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
}

/// The last-applied-batch ack a viewer sends the relay (relay.md → Flow
/// control): kind 3, in the clear, `seq` the last batch it applied.
pub fn ack(session: &[u8; 16], seq: u64) -> Vec<u8> {
    let mut b = vec![crate::protocol::frame::V, KIND_ACK, 0, 0];
    b.extend_from_slice(session);
    b.extend_from_slice(&seq.to_be_bytes());
    b
}

/// A viewer acks at least every this many batches, well inside the relay's
/// window of 16, and at the next display frame after any it applied.
pub const ACK_EVERY: u64 = 4;

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
            newest(last, i);
            seen.insert(i.to_string())
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

/// One stream batch: its lines, the log's events among them once each.
/// Answers, when it skipped past a batch this viewer never opened, the
/// newest event held *before* it: what the catch-up asks from, since this
/// batch's own events are newer than the ones the gap swallowed.
pub fn apply(l: &mut ViewerLink, b: &[u8], seen: &mut HashSet<String>, last: &mut Option<String>, asked: &mut HashSet<String>, frame: &mut Vec<Update>, applied: &mut u64) -> Option<Option<String>> {
    let before = last.clone();
    let Ok((body, end, skipped)) = l.open_batch_gap(b) else { return None };
    let seq = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
    // Against the last batch opened, or — the first since joining — the
    // relay's count at the join: either way the batch must be the next.
    let gap = (skipped || seq != *applied + 1).then_some(before);
    *applied = seq;
    let Ok(batch) = serde_json::from_slice::<Batch>(&body) else { return gap };
    for line in batch.lines {
        match &line {
            StreamLine::Log(e) => {
                newest(last, &e.id);
                if !seen.insert(e.id.clone()) {
                    continue;
                }
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
    gap
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
        assert!(ACK_EVERY < 16, "acked well inside the relay's window of 16");
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

    /// R-OFF-2: batches 1 and 3 held for a welcome whose catch-up went out
    /// from an older event: the gap shows once, and the welcome's catch-up
    /// covers it, so no second one is asked.
    #[test]
    fn r_off_2_a_gap_among_held_batches_shows_and_is_covered_by_the_welcomes_catch_up() {
        let (mut link, [b1, _, b3]) = three_batches();
        let mut c = CatchUps::default();
        // `send_welcomed`: the catch-up from the newest event the chunks held.
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
    /// `leaving_in` (and the live path) use.
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
