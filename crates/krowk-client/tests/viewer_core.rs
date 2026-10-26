//! `krowk_client::sync::viewer::ViewerCore`, driven with no network and no
//! registry (R-CLIENT-3; engineering/harness.md → Sync): the test is the
//! relay, `relay_link::HostLink` the host, and the time is whatever `now`
//! the test passes. A viewer's `Send` reaches the host as `host.open(link,
//! bytes)`; the host's welcome and answers reach the viewer as the routed
//! envelope's payload, and its batches whole.

use krowk_client::e2e::{DeviceId, SessionKey, SessionKeys};
use krowk_client::protocol::frame::{HEADER, KIND_ACK, KIND_FRAME};
use krowk_client::protocol::{ApprovalRequest, Command, LiveEvent, StreamLine};
use krowk_client::relay_link::{HostLink, Inbound};
use krowk_client::sync::join::control;
use krowk_client::sync::store::{Attached, ReadError};
use krowk_client::sync::viewer::{Cursor, Effect, Update, ViewerCore};
use krowk_client::sync::{uuid, Answer, Candidate, ViewerFrame, Welcome};
use serde_json::{json, Value};

const SESSION: &str = "01a0f731-f1c0-7000-92d7-64b247e51781";
const RELAY: &str = "wss://relay.example";

/// A logged event, its id the UUIDv7-shaped `01900000-…-n`.
fn logged(n: u8) -> Value {
    json!({"id": id(n), "sessionId": "s", "timeMs": 0, "type": "session.started", "cwd": "/", "krowkVersion": "0", "protocolVersion": 1})
}

fn id(n: u8) -> String {
    format!("01900000-0000-7000-8000-0000000000{n:02}")
}

fn approval(r: &str) -> Value {
    json!({"type": "approval.requested", "sessionId": "s", "turnId": "t", "requestId": r, "tool": "Bash", "input": {}, "summary": "ls", "reason": "asked"})
}

fn stop(n: usize) -> Command {
    Command::Interrupt { session_id: n.to_string() }
}

/// The relay between one `ViewerCore` and its `HostLink`, and what the
/// viewer was told to do, kept for the test to read.
struct Relay {
    core: ViewerCore,
    key: SessionKey,
    host: HostLink,
    link: u64,
    now: u64,
    /// Every `Show`, in order, its `Saved` taken out (and one that held
    /// nothing else left out).
    shown: Vec<Vec<Update>>,
    /// Every `Show` as it came.
    whole: Vec<Vec<Update>>,
    /// The places the `Show`s ended with.
    saved: Vec<Cursor>,
    /// The frames the host opened from the viewer, in order.
    frames: Vec<ViewerFrame>,
    /// The acks the viewer sent the relay, by `seq`.
    acks: Vec<u64>,
    /// Every other effect, in order.
    rest: Vec<Effect>,
}

impl Relay {
    /// A viewer attached at rest with `events`, at `now`: its first `Show`
    /// and its first `Join` taken.
    fn new(events: Vec<Value>, now: u64) -> Relay {
        let key = SessionKey::generate();
        let host = HostLink::new(&key, uuid(SESSION));
        let at_rest = Attached { events, ..Attached::default() };
        let (core, first) = ViewerCore::new(SessionKeys::from(key.clone()), SESSION, DeviceId([1; 16]), RELAY, at_rest, now);
        let mut r = Relay { core, key, host, link: 0, now, shown: Vec::new(), whole: Vec::new(), saved: Vec::new(), frames: Vec::new(), acks: Vec::new(), rest: Vec::new() };
        r.take(first);
        assert!(matches!(r.shown.as_slice(), [u] if matches!(u.as_slice(), [Update::Attached { .. }])), "{:?}", r.shown);
        assert!(matches!(r.rest.as_slice(), [Effect::Join { at_ms, extra }] if *at_ms == now && *extra == json!({})), "{:?}", r.rest);
        r.rest.clear();
        r.shown.clear();
        r
    }

    /// Sorts what the viewer was told: frames to the host opened by it.
    fn take(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Show(u) => {
                    self.whole.push(u.clone());
                    let mut rest = Vec::new();
                    for u in u {
                        match u {
                            Update::Saved(c) => self.saved.push(c),
                            u => rest.push(u),
                        }
                    }
                    if !rest.is_empty() {
                        self.shown.push(rest);
                    }
                }
                Effect::Send(b) if b[1] == KIND_ACK => self.acks.push(u64::from_be_bytes(b[20..28].try_into().unwrap())),
                Effect::Send(b) if b[1] == KIND_FRAME => match self.host.open(self.link, &b).expect("the host opens the viewer's frame") {
                    Inbound::Hello { .. } => {}
                    Inbound::Frame { body, .. } => self.frames.push(serde_json::from_slice(&body).unwrap()),
                },
                e => self.rest.push(e),
            }
        }
    }

    fn at(&mut self, now: u64) -> &mut Self {
        self.now = now;
        self
    }

    fn joined(&mut self, host: bool, seq: u64) {
        self.link += 1;
        let e = self.core.joined(&json!({"type": "joined", "link": self.link, "host": host, "seq": seq}), self.now);
        self.take(e);
    }

    fn welcome_with(&mut self, w: Welcome) {
        let routed = self.host.welcome(self.link, &serde_json::to_vec(&w).unwrap()).unwrap();
        self.receive(&routed[HEADER..]);
    }

    /// Joined with a host there, welcomed, and the chunks read: live.
    fn live(&mut self) {
        self.joined(true, 0);
        self.welcome_with(Welcome::default());
        assert!(self.take_rest().iter().any(|e| matches!(e, Effect::ReadChunks { want: None })));
        self.read(Ok((vec![], Attached::default())));
    }

    fn receive(&mut self, bytes: &[u8]) {
        let e = self.core.received(bytes, self.now);
        self.take(e);
    }

    fn read(&mut self, r: Result<(Vec<Value>, Attached), ReadError>) {
        let e = self.core.chunks_read(r, self.now);
        self.take(e);
    }

    fn tick(&mut self) {
        let e = self.core.tick(self.now);
        self.take(e);
    }

    fn command(&mut self, c: Command) {
        let e = self.core.command(c, self.now);
        self.take(e);
    }

    fn control(&mut self, v: Value) {
        self.receive(&control(&v));
    }

    /// The host's batch of `lines`, `last` ending the stream.
    fn batch(&mut self, lines: Vec<Value>, last: bool) -> Vec<u8> {
        self.host.batch(&serde_json::to_vec(&json!({"lines": lines, "head": null})).unwrap(), last).unwrap()
    }

    fn answer(&mut self, a: &Answer) {
        let routed = self.host.to_viewer(self.link, &serde_json::to_vec(a).unwrap(), false).unwrap();
        self.receive(&routed[HEADER..]);
    }

    /// Everything shown so far, one after the other.
    fn updates(&mut self) -> Vec<Update> {
        std::mem::take(&mut self.shown).into_iter().flatten().collect()
    }

    fn take_rest(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.rest)
    }
}

fn catch_ups(frames: &[ViewerFrame]) -> Vec<Option<String>> {
    frames.iter().filter_map(|f| if let ViewerFrame::CatchUp { after } = f { Some(after.clone()) } else { None }).collect()
}

fn commands(frames: &[ViewerFrame]) -> Vec<Command> {
    frames.iter().filter_map(|f| if let ViewerFrame::Command(r) = f { Some(r.command.clone()) } else { None }).collect()
}

fn hosts(u: &[Update]) -> Vec<bool> {
    u.iter().filter_map(|u| if let Update::Host(h) = u { Some(*h) } else { None }).collect()
}

/// R-HAND-4: prompts typed with no host show as queued and go at the
/// welcome — after the chunks are read up to its head, behind the catch-up,
/// in the order typed — and one typed while the read is out shows at once,
/// queued, and goes after them. Nothing is sent between the welcome and the
/// read.
#[test]
fn r_hand_4_queued_prompts_go_at_the_welcome() {
    let mut r = Relay::new(vec![logged(1)], 0);
    r.command(stop(1));
    r.command(stop(2));
    r.at(16).tick();
    let shown = r.updates();
    assert!(shown.iter().all(|u| matches!(u, Update::Sent { queued: true, .. })) && shown.len() == 2, "{shown:?}");

    r.joined(true, 0);
    r.welcome_with(Welcome::default());
    assert!(matches!(r.take_rest().as_slice(), [Effect::ReadChunks { want: None }]));
    r.command(stop(3));
    assert!(r.frames.is_empty(), "nothing goes before the chunks are read: {:?}", r.frames);
    r.at(32).tick();
    assert!(matches!(r.updates().as_slice(), [Update::Path { .. }, Update::Sent { queued: true, .. }]), "shown at once, queued");

    r.read(Ok((vec![logged(1), logged(2)], Attached::default())));
    assert_eq!(catch_ups(&r.frames), [Some(id(2))], "the catch-up from the newest event the chunks held");
    assert!(matches!(r.frames[0], ViewerFrame::CatchUp { .. }), "the catch-up first");
    assert_eq!(commands(&r.frames), [stop(1), stop(2), stop(3)]);
    assert_eq!(r.acks, [0], "acked at the cursor");
    r.at(48).tick();
    let shown = r.updates();
    assert!(matches!(shown.as_slice(), [Update::CaughtUp(e), Update::Host(true)] if e.len() == 1 && e[0]["id"] == id(2)), "{shown:?}");
}

/// R-OFF-2: batches 1 and 3 — the relay dropped 2 — show a gap and ask the
/// host to catch up from batch 1's event; one found while a catch-up is in
/// flight is asked once that one ends.
#[test]
fn r_off_2_a_gap_asks_the_host_to_catch_up() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    r.frames.clear();
    let (b1, _b2, b3) = (r.batch(vec![logged(1)], false), r.batch(vec![logged(2)], false), r.batch(vec![logged(3)], false));
    r.receive(&b1);
    assert!(r.frames.is_empty());
    r.receive(&b3);
    assert_eq!(catch_ups(&r.frames), [Some(id(1))]);
    r.at(16).tick();
    assert_eq!(r.updates().iter().filter(|u| matches!(u, Update::Gap)).count(), 1);

    // Batch 5 after 3, with that catch-up still out: owed, then asked.
    let (_b4, b5) = (r.batch(vec![logged(4)], false), r.batch(vec![logged(5)], false));
    r.receive(&b5);
    assert_eq!(catch_ups(&r.frames), [Some(id(1))], "not while one is in flight");
    r.answer(&Answer::CatchUp { events: vec![], more: true });
    assert_eq!(catch_ups(&r.frames).len(), 1, "nor on a page with more to come");
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    assert_eq!(catch_ups(&r.frames), [Some(id(1)), Some(id(3))]);
}

/// R-OFF-2: batches that come between the welcome and the chunks' read are
/// held, then applied once the read is in, after the catch-up; a gap among
/// them is covered by that catch-up, and the cursor is acked.
#[test]
fn r_off_2_batches_between_the_welcome_and_the_read_are_held_then_applied() {
    let mut r = Relay::new(vec![], 0);
    r.joined(true, 0);
    r.welcome_with(Welcome::default());
    let (b1, _b2, b3) = (r.batch(vec![logged(1)], false), r.batch(vec![logged(2)], false), r.batch(vec![logged(3)], false));
    r.receive(&b1);
    r.receive(&b3);
    r.at(16).tick();
    assert!(r.updates().iter().all(|u| !matches!(u, Update::Line(_))), "nothing applied before the read");
    assert!(r.frames.is_empty() && r.acks.is_empty());

    r.read(Ok((vec![], Attached::default())));
    assert_eq!(catch_ups(&r.frames), [None], "the welcome's catch-up covers the gap: no second one");
    assert_eq!(r.acks, [0], "the cursor: the batches are not shown yet");
    r.at(32).tick();
    assert_eq!(r.acks, [0, 3], "shown, then acked");
    let shown = r.updates();
    let lines = shown.iter().filter(|u| matches!(u, Update::Line(_))).count();
    assert_eq!((lines, shown.iter().filter(|u| matches!(u, Update::Gap)).count()), (2, 1), "{shown:?}");
}

/// R-LAG-4, R-IOS-1: the relay ack never names a batch whose updates have
/// not been shown. Four batches in one display frame are acked only as the
/// `Show` holding their lines goes out, and then with the fourth; the next
/// frame's at the next tick.
#[test]
fn r_lag_4_the_ack_names_only_batches_shown() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.acks.clear();
    for n in 1..=4 {
        let b = r.batch(vec![logged(n)], false);
        r.receive(&b);
    }
    assert!(r.acks.is_empty(), "applied, not shown: no ack yet");
    assert_eq!(r.core.next_wake(), Some(16), "due at the next display frame");
    let effects = r.core.tick(16);
    let show = effects.iter().position(|e| matches!(e, Effect::Show(_))).expect("shown");
    let ack = effects.iter().position(|e| matches!(e, Effect::Send(b) if b[1] == KIND_ACK)).expect("acked");
    assert!(show < ack, "the Show before the ack: {effects:?}");
    r.take(effects);
    assert_eq!(r.acks, [4], "the fourth");
    assert_eq!(r.shown[0].iter().filter(|u| matches!(u, Update::Line(_))).count(), 4);
    let b = r.batch(vec![logged(5)], false);
    r.receive(&b);
    assert_eq!(r.acks, [4], "not yet");
    r.at(32).tick();
    assert_eq!(r.acks, [4, 5]);
    r.at(56).tick();
    assert_eq!(r.acks, [4, 5], "once");

    // A batch with no lines leaves nothing to show: the ack alone is due,
    // at the next display frame.
    let b = r.batch(vec![], false);
    r.at(60).receive(&b);
    assert_eq!(r.core.next_wake(), Some(48), "a frame after the last show, at 32: past, so at once");
    r.at(60).tick();
    assert_eq!(r.acks, [4, 5, 6]);
    assert_eq!(r.core.next_wake(), Some(10_000), "the heartbeat next");
}

/// R-LAG-7: two ticks 5 ms apart hand the screen one `Show`; the next
/// waits for a whole frame after it.
#[test]
fn r_lag_7_one_show_per_display_frame() {
    let mut r = Relay::new(vec![], 1000);
    r.live();
    r.at(1016).tick();
    r.shown.clear();
    let b = r.batch(vec![logged(1)], false);
    r.receive(&b);
    r.at(1100).tick();
    let b = r.batch(vec![logged(2)], false);
    r.receive(&b);
    r.at(1105).tick();
    assert_eq!(r.shown.len(), 1, "{:?}", r.shown);
    assert_eq!(r.core.next_wake(), Some(1116));
    r.at(1115).tick();
    assert_eq!(r.shown.len(), 1);
    r.at(1116).tick();
    assert_eq!(r.shown.len(), 2);
    assert!(matches!(r.shown[1].as_slice(), [Update::Line(_)]));
}

/// R-OFF-2: a `resync` for another stream joins again at once, carrying the
/// cursor as the harness does. The relay, on another stream, answers with a
/// `resync` `stream` again, and that join carries none: the viewer never
/// loops on the old stream's cursor.
#[test]
fn r_off_2_a_stream_resync_joins_again() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    let b = r.batch(vec![logged(1)], false);
    r.receive(&b);
    r.at(500).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    let stream = krowk_client::e2e::hex(&r.host.stream());
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 500, extra }] if *extra == json!({"stream": stream, "afterSeq": 1})));
    r.link += 1;
    let e = r.core.joined(&json!({"type": "joined", "link": r.link, "host": true, "stream": "22".repeat(16), "seq": 4}), 600);
    r.take(e);
    r.at(600).control(json!({"type": "resync", "reason": "stream", "stream": "22".repeat(16), "seq": 4}));
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 600, extra }] if *extra == json!({})), "no cursor on another stream");
}

/// R-HAND-4: with no host, a `resync` reads the chunks from where the
/// viewer stopped, and shows what they held that it had not seen.
#[test]
fn r_hand_4_a_resync_with_no_host_reads_the_chunks() {
    let mut r = Relay::new(vec![logged(1)], 0);
    r.joined(false, 0);
    r.control(json!({"type": "resync", "reason": "behind", "stream": "00", "seq": 0}));
    assert!(matches!(r.take_rest().as_slice(), [Effect::ReadChunks { want: None }]));
    r.read(Ok((vec![logged(1), logged(2)], Attached::default())));
    r.at(16).tick();
    let shown = r.updates();
    assert!(matches!(shown.as_slice(), [Update::Host(false), Update::CaughtUp(e)] if e.len() == 1), "{shown:?}");
    assert!(r.frames.is_empty(), "no host to send to");
}

/// R-HAND-4: the relay saying the host is absent shows no host, once.
#[test]
fn r_hand_4_host_absent_shows_no_host() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.control(json!({"type": "error", "code": "host_absent", "message": "no host"}));
    r.control(json!({"type": "error", "code": "host_absent", "message": "no host"}));
    r.at(16).tick();
    assert_eq!(hosts(&r.updates()), [true, false]);
}

/// R-HAND-4: the stream's final batch shows no host.
#[test]
fn r_hand_4_a_final_batch_shows_no_host() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    let b = r.batch(vec![], true);
    r.receive(&b);
    r.at(16).tick();
    assert_eq!(hosts(&r.updates()), [true, false]);
}

/// R-LAG-8: a command sent shows at once and is settled by the host's
/// ack, under the same id.
#[test]
fn r_lag_8_an_ack_settles_the_command() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.frames.clear();
    r.command(stop(1));
    let Some(ViewerFrame::Command(sent)) = r.frames.first().cloned() else { panic!("{:?}", r.frames) };
    r.answer(&Answer::Ack { id: sent.id.clone(), error: None });
    r.at(16).tick();
    let shown = r.updates();
    assert!(shown.iter().any(|u| matches!(u, Update::Sent { id, queued: false } if *id == sent.id)));
    assert!(shown.iter().any(|u| matches!(u, Update::Acked { id, error: None } if *id == sent.id)), "{shown:?}");
}

/// R-OFF-2: a ping every 10 s, and a connection with no word for 30 s is
/// dropped and the relay joined again from the cursor; the welcome on it
/// sends again what was never acked.
#[test]
fn r_off_2_no_word_for_30_s_joins_again() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.command(stop(1));
    for t in [10_000, 20_000, 30_000] {
        r.at(t).tick();
        assert!(matches!(r.take_rest().as_slice(), [Effect::Ping]), "at {t}");
    }
    assert_eq!(r.core.next_wake(), Some(40_000));
    r.at(40_000).tick();
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 40_000, .. }]));
    r.at(40_000).tick();
    assert!(r.take_rest().is_empty(), "no ping with no connection");
    r.command(stop(2));
    r.at(40_016).tick();
    assert!(r.updates().iter().any(|u| matches!(u, Update::Sent { queued: true, .. })), "no connection: queued");

    r.frames.clear();
    r.at(41_000).live();
    assert_eq!(commands(&r.frames), [stop(1), stop(2)], "the unacked again, then the queued, at the welcome");
}

/// R-CLIENT-3: a `pong`, or any message, keeps the connection alive.
#[test]
fn r_client_3_any_word_keeps_the_connection() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    for t in [10_000, 20_000, 30_000, 40_000, 50_000] {
        r.at(t).tick();
        assert!(matches!(r.take_rest().as_slice(), [Effect::Ping]), "at {t}");
        if t == 20_000 {
            r.core.alive(25_000);
        }
    }
    r.at(60_000).tick();
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { .. }]));
}

/// R-CLIENT-3: a welcome whose chunks cannot be read up to its head, for
/// good — the session gone, its chunks not opening under this device's
/// keys, or a rollback — shows `Failed` and ends the viewer: nothing after
/// does anything.
#[test]
fn r_client_3_a_failed_read_shows_failed_and_ends() {
    let mut r = Relay::new(vec![], 0);
    r.joined(true, 0);
    r.welcome_with(Welcome::default());
    r.take_rest();
    r.read(Err(ReadError::permanent("the log is shorter than the head")));
    assert!(matches!(r.take_rest().as_slice(), [Effect::End]));
    let shown = r.updates();
    assert!(matches!(shown.last(), Some(Update::Failed(e)) if e.contains("shorter")), "{shown:?}");
    assert_eq!(r.core.next_wake(), None);
    let b = r.batch(vec![logged(1)], false);
    r.receive(&b);
    r.command(stop(1));
    r.at(100_000).tick();
    assert!(r.shown.is_empty() && r.rest.is_empty() && r.frames.is_empty());
}

/// R-PERM-2: the approvals a welcome names show once, and the stream
/// carrying the same request again, or the next welcome naming it again,
/// shows nothing more.
#[test]
fn r_perm_2_a_welcomes_approvals_show_once() {
    let mut r = Relay::new(vec![], 0);
    r.joined(true, 0);
    let asked: ApprovalRequest = serde_json::from_value(approval("r1")).unwrap();
    r.welcome_with(Welcome { approvals: vec![asked.clone()], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.at(16).tick();
    let first = r.updates();
    assert!(first.iter().any(|u| matches!(u, Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(a))) if a.request_id == "r1")), "the welcome's approval shows: {first:?}");
    let b = r.batch(vec![approval("r1")], false);
    r.receive(&b);
    r.at(5_000).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    r.joined(true, 0);
    r.welcome_with(Welcome { approvals: vec![asked], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.at(6_000).tick();
    let shown = r.updates();
    let n = shown.iter().filter(|u| matches!(u, Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(a))) if a.request_id == "r1")).count();
    assert_eq!(n, 0, "not again: {shown:?}");
}

/// R-PERM-2 (ticket 145): requests shown, then the viewer away, and a
/// rejoin whose welcome names only one of them: the others were answered
/// elsewhere meanwhile, and are said gone, once, before the requests the
/// welcome names. The next welcome says nothing more.
#[test]
fn r_perm_2_a_request_resolved_while_away_is_gone_at_the_rejoin() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    let b = r.batch(vec![approval("r1"), approval("r2"), approval("r3")], false);
    r.receive(&b);
    r.at(16).tick();
    r.updates();
    r.at(5_000).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    r.joined(true, 0);
    let waiting: ApprovalRequest = serde_json::from_value(approval("r2")).unwrap();
    r.welcome_with(Welcome { approvals: vec![waiting.clone()], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.at(5_016).tick();
    let shown = r.updates();
    let gone: Vec<&str> = shown.iter().filter_map(|u| if let Update::ApprovalGone(id) = u { Some(id.as_str()) } else { None }).collect();
    assert_eq!(gone, ["r1", "r3"], "{shown:?}");
    assert!(!shown.iter().any(|u| matches!(u, Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(_))))), "r2 was shown already: {shown:?}");

    // Gone, it is asked no more: the stream bringing r1 again shows it.
    let b = r.batch(vec![approval("r1")], false);
    r.receive(&b);
    r.at(5_032).tick();
    assert!(r.updates().iter().any(|u| matches!(u, Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(a))) if a.request_id == "r1")));
    r.at(9_000).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    r.joined(true, 0);
    let again: ApprovalRequest = serde_json::from_value(approval("r1")).unwrap();
    r.welcome_with(Welcome { approvals: vec![waiting, again], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.at(9_016).tick();
    let shown = r.updates();
    assert!(!shown.iter().any(|u| matches!(u, Update::ApprovalGone(_))), "all still waiting: {shown:?}");
}

/// R-OFF-2 (ticket 145): every catch-up the viewer asks for ends with a
/// `CaughtUp`, empty when the host had nothing the viewer had not seen
/// (the batch the relay dropped held only live lines): a screen showing
/// "catching up" since the `Gap` hears that it is over. A page with more
/// to come, or a last page with a catch-up owed after it, says nothing.
#[test]
fn r_off_2_an_empty_catch_up_still_says_caught_up() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    r.at(16).tick();
    let shown = r.updates();
    assert!(shown.iter().any(|u| matches!(u, Update::CaughtUp(e) if e.is_empty())), "the welcome's catch-up ended: {shown:?}");

    let (b1, _b2, b3) = (r.batch(vec![logged(1)], false), r.batch(vec![], false), r.batch(vec![logged(3)], false));
    r.receive(&b1);
    r.receive(&b3);
    let (_b4, b5) = (r.batch(vec![], false), r.batch(vec![logged(5)], false));
    r.receive(&b5);
    r.at(32).tick();
    let shown = r.updates();
    assert_eq!(shown.iter().filter(|u| matches!(u, Update::Gap)).count(), 2, "{shown:?}");
    assert!(!shown.iter().any(|u| matches!(u, Update::CaughtUp(_))), "{shown:?}");

    r.answer(&Answer::CatchUp { events: vec![], more: true });
    r.answer(&Answer::CatchUp { events: vec![serde_json::from_value(logged(3)).unwrap()], more: false });
    r.at(48).tick();
    let shown = r.updates();
    assert!(!shown.iter().any(|u| matches!(u, Update::CaughtUp(_))), "the second gap's catch-up is still out: {shown:?}");
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    r.at(64).tick();
    assert!(matches!(r.updates().as_slice(), [Update::CaughtUp(e)] if e.is_empty()), "the last one, with nothing new");
}

/// R-OFF-2: a host arriving while the viewer waits for one joins again; the
/// relay closing joins again a second later; a refused join is said once
/// and tried again every second.
#[test]
fn r_off_2_presence_a_close_and_a_refusal_join_again() {
    let mut r = Relay::new(vec![], 0);
    r.joined(false, 0);
    r.at(100).control(json!({"type": "host", "present": true}));
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 100, .. }]));
    r.joined(true, 0);
    let e = r.core.closed(200);
    assert!(matches!(e.as_slice(), [Effect::Join { at_ms: 1200, .. }]));
    for t in [1200, 2200] {
        let e = r.core.join_failed("the relay could not be reached".into(), t);
        assert!(matches!(e.as_slice(), [Effect::Join { at_ms, .. }] if *at_ms == t + 1000));
    }
    r.core.join_failed("the relay refused: forbidden — no".into(), 3200);
    r.at(4200).tick();
    let notes: Vec<String> = r.updates().into_iter().filter_map(|u| if let Update::Note(n) = u { Some(n) } else { None }).collect();
    assert_eq!(notes.len(), 2, "once each error, as it changes: {notes:?}");
    assert!(notes[0].starts_with("not on the relay wss://relay.example: the relay could not be reached; trying again every second"), "{notes:?}");

    // Welcomed on the relay: a host arriving is one the relay let go and
    // took back, so the viewer joins again; one leaving shows no host.
    r.at(5000).live();
    r.at(5100).control(json!({"type": "host", "present": false}));
    assert!(r.take_rest().is_empty());
    r.at(5200).tick();
    assert_eq!(hosts(&r.updates()), [true, false]);
    r.at(5300).control(json!({"type": "host", "present": true}));
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 5300, .. }]));
}

/// R-OFF-2: what the connection brings while the welcome's read is out is
/// acted on once the read is in: a close then joins again a second after
/// the read is answered, when the viewer first hears of it. A read
/// answered with none out does nothing.
#[test]
fn r_off_2_what_comes_during_the_read_is_acted_on_after_it() {
    let mut r = Relay::new(vec![], 0);
    r.joined(true, 0);
    r.welcome_with(Welcome::default());
    r.take_rest();
    let e = r.core.closed(50);
    assert!(e.is_empty(), "not yet: {e:?}");
    r.at(80).read(Ok((vec![], Attached::default())));
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 1080, .. }]));
    assert_eq!(catch_ups(&r.frames), [None], "the welcome acted on first");
    r.at(90).read(Ok((vec![logged(1)], Attached::default())));
    assert!(r.take_rest().is_empty());
    r.at(200).tick();
    assert!(!r.updates().iter().any(|u| matches!(u, Update::CaughtUp(_))), "a stale read shows nothing");
}

/// R-HAND-1, R-HAND-4: a handoff asked with no host goes at the welcome,
/// after the commands.
#[test]
fn r_hand_4_a_handoff_asked_with_no_host_goes_after_the_commands() {
    let mut r = Relay::new(vec![], 0);
    r.command(stop(1));
    let e = r.core.ask_handoff(1);
    r.take(e);
    r.at(2).live();
    assert!(matches!(r.frames.as_slice(), [ViewerFrame::CatchUp { .. }, ViewerFrame::Command(_), ViewerFrame::Handoff { .. }]), "{:?}", r.frames);
}

/// R-HAND-4: with no host, a resync read that fails for good — the
/// session's key rotated since it was opened, say — ends the viewer; one
/// that failed for now is let go, with no join.
#[test]
fn r_hand_4_a_resync_read_ends_only_on_a_permanent_failure() {
    let mut r = Relay::new(vec![], 0);
    r.joined(false, 0);
    let resync = json!({"type": "resync", "reason": "beyond_buffer", "stream": "00", "seq": 0});
    r.control(resync.clone());
    r.read(Err(ReadError::transient("network_unreachable")));
    assert!(r.take_rest().iter().all(|e| !matches!(e, Effect::End | Effect::Join { .. })));
    r.control(resync);
    assert!(matches!(r.take_rest().as_slice(), [Effect::ReadChunks { want: None }]));
    r.read(Err(ReadError::permanent(format!("chunk 3 is sealed under a newer epoch — {}; open it again", krowk_client::e2e::ROTATED_SINCE_OPENED))));
    assert!(matches!(r.take_rest().as_slice(), [Effect::End]));
    assert!(matches!(r.updates().last(), Some(Update::Failed(_))));
}

/// R-NET-2: a race won moves the session to the direct path: hello on the
/// new connection, the relay's batches meanwhile held, and the direct
/// welcome drops the relay's connection; the host's presence gone there
/// goes back to the relay.
#[test]
fn r_net_2_a_move_to_a_direct_path_and_back() {
    let mut r = Relay::new(vec![], 0);
    r.joined(true, 0);
    let c = Candidate { url: "ws://100.64.0.1:7000".into(), via: "tailscale".into() };
    r.welcome_with(Welcome { candidates: vec![c.clone()], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.take_rest();
    assert!(r.core.may_probe());
    r.at(16).tick();
    assert!(r.take_rest().iter().any(|e| matches!(e, Effect::Probe)));
    assert!(!r.core.may_probe(), "one race at a time");

    r.link += 1;
    let e = r.core.move_to(c.clone(), &json!({"type": "joined", "link": r.link, "host": true, "seq": 0}), 100);
    r.take(e);
    assert!(r.rest.is_empty());
    // The relay's batch, on the leaving connection, waits for the direct welcome.
    let b = r.batch(vec![logged(1)], false);
    let e = r.core.leaving(&b, 110);
    assert!(e.is_empty());
    r.at(120).welcome_with(Welcome::default());
    assert!(matches!(r.take_rest().as_slice(), [Effect::Leave, Effect::ReadChunks { want: None }]));
    r.read(Ok((vec![], Attached::default())));
    r.at(200).tick();
    let shown = r.updates();
    assert!(shown.iter().any(|u| matches!(u, Update::Path { path, via: Some(v) } if path == "direct over Tailscale" && *v == c.url)), "{shown:?}");
    assert!(shown.iter().any(|u| matches!(u, Update::Line(_))), "the held batch applied");

    r.at(300).control(json!({"type": "host", "present": false}));
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 300, .. }]));
}

/// A whole session — attach, join, welcome, read, batches, a command and
/// its ack, frames, heartbeats and a rejoin — as the `Effect`s it gives,
/// with each `Join`'s time and each `next_wake` counted from the origin.
fn session(origin: u64) -> Vec<String> {
    let mut r = Relay::new(vec![logged(1)], origin);
    let mut log = vec![format!("wake {:?}", r.core.next_wake().map(|t| t - origin))];
    r.at(origin + 5).joined(true, 0);
    r.welcome_with(Welcome::default());
    r.read(Ok((vec![], Attached::default())));
    r.command(stop(1));
    for n in 2..=7 {
        let b = r.batch(vec![logged(n)], false);
        r.at(origin + 5 + u64::from(n)).receive(&b);
    }
    let sent = match r.frames.iter().find(|f| matches!(f, ViewerFrame::Command(_))) {
        Some(ViewerFrame::Command(s)) => s.id.clone(),
        _ => panic!("sent"),
    };
    r.answer(&Answer::Ack { id: sent, error: None });
    while r.now < origin + 45_000 {
        let at = r.core.next_wake().expect("live");
        log.push(format!("wake {}", at - origin));
        r.at(at).tick();
        for e in r.take_rest() {
            log.push(match e {
                Effect::Join { at_ms, extra } => format!("join {} {}", at_ms - origin, extra["afterSeq"]),
                e => format!("{e:?}"),
            });
        }
        for u in std::mem::take(&mut r.shown) {
            log.push(format!("show {}", u.len()));
        }
    }
    log.push(format!("acks {:?}, frames {}", r.acks, r.frames.len()));
    log
}

/// R-CLIENT-3, R-CLIENT-1: the core reads no clock and opens no socket, so
/// the phone and the terminal run the same rules. A whole session
/// driven through `now_ms` alone, from two origins far apart, does the same
/// things at the same offsets: what it does depends on the times passed in
/// and on nothing else.
#[test]
fn r_client_3_the_core_reads_no_clock_and_opens_no_socket() {
    let a = session(0);
    let b = session(1 << 40);
    assert_eq!(a, b);
    assert!(a.contains(&"join 40000 6".to_string()), "{a:?}");
    assert!(a.iter().filter(|l| l.as_str() == "Ping").count() == 3, "{a:?}");
}

/// Welcomed on the relay with one candidate to race.
fn with_a_candidate(now: u64) -> (Relay, Candidate) {
    let mut r = Relay::new(vec![], now);
    r.joined(true, 0);
    let c = Candidate { url: "ws://192.168.1.5:7000".into(), via: "lan".into() };
    r.welcome_with(Welcome { candidates: vec![c.clone()], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.take_rest();
    (r, c)
}

fn probes(r: &mut Relay) -> usize {
    r.take_rest().iter().filter(|e| matches!(e, Effect::Probe)).count()
}

/// R-NET-2: a race nobody wins is tried again 10 s later, then 20, 40 …
/// up to 5 minutes; a direct welcome puts the wait back to 10 s, and a
/// direct path closing goes back to the relay at once, the next race 10 s
/// on.
#[test]
fn r_net_2_races_back_off_and_a_direct_welcome_resets_the_wait() {
    let (mut r, c) = with_a_candidate(0);
    r.at(0).tick();
    assert_eq!(probes(&mut r), 1, "the first race at once");
    assert_eq!(probes(&mut r), 0);
    let mut t = 1000;
    for wait in [10, 20, 40, 80, 160, 300, 300] {
        r.core.probe_lost(t);
        let due = t + wait * 1000;
        r.core.alive(due - 1);
        assert!(r.core.next_wake().is_some_and(|w| w <= due), "woken by {due}");
        r.at(due - 1).tick();
        assert_eq!(probes(&mut r), 0, "not before {due}");
        r.at(due).tick();
        assert_eq!(probes(&mut r), 1, "at {due}");
        t = due;
    }

    r.link += 1;
    let e = r.core.move_to(c.clone(), &json!({"type": "joined", "link": r.link, "host": true, "seq": 0}), t);
    r.take(e);
    r.at(t + 10).welcome_with(Welcome::default());
    r.read(Ok((vec![], Attached::default())));
    assert!(!r.core.may_probe(), "on a direct path");
    let e = r.core.closed(t + 20);
    assert!(matches!(e.as_slice(), [Effect::Join { at_ms, .. }] if *at_ms == t + 20), "at once: {e:?}");
    r.at(t + 30).joined(true, 0);
    r.welcome_with(Welcome { candidates: vec![c], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.at(t + 20 + 10_000 - 1).tick();
    assert_eq!(probes(&mut r), 0);
    r.at(t + 20 + 10_000).tick();
    assert_eq!(probes(&mut r), 1, "10 s after the direct path went");
}

/// R-NET-2: a direct welcome that has not come 3 s after the hello goes
/// back to the relay; so do more than 1024 batches held waiting for it.
#[test]
fn r_net_2_a_late_direct_welcome_or_too_much_held_goes_back_to_the_relay() {
    let (mut r, c) = with_a_candidate(0);
    let b = r.batch(vec![logged(1)], false);
    r.receive(&b);
    r.at(16).tick();
    assert_eq!(probes(&mut r), 1);
    let joined = |link| json!({"type": "joined", "link": link, "host": true, "seq": 0});
    r.link += 1;
    let e = r.core.move_to(c.clone(), &joined(r.link), 100);
    r.take(e);
    assert_eq!(r.core.next_wake(), Some(3100));
    r.at(3099).tick();
    assert!(r.take_rest().iter().all(|e| !matches!(e, Effect::Join { .. })));
    r.at(3100).tick();
    let stream = krowk_client::e2e::hex(&r.host.stream());
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 3100, extra }] if *extra == json!({"stream": stream, "afterSeq": 1})), "back to the relay from the cursor");

    let (mut r, c) = with_a_candidate(0);
    r.at(16).tick();
    r.take_rest();
    r.link += 1;
    let e = r.core.move_to(c, &joined(r.link), 100);
    r.take(e);
    for n in 0..1024 {
        let b = r.batch(vec![], false);
        assert!(r.core.leaving(&b, 200).is_empty(), "held, {n}");
    }
    let b = r.batch(vec![], false);
    assert!(matches!(r.core.leaving(&b, 200).as_slice(), [Effect::Join { at_ms: 200, .. }]));

    // The same on the direct connection, before its welcome.
    let (mut r, c) = with_a_candidate(0);
    r.at(16).tick();
    r.take_rest();
    r.link += 1;
    let e = r.core.move_to(c, &joined(r.link), 100);
    r.take(e);
    for _ in 0..1024 {
        let b = r.batch(vec![], false);
        r.at(200).receive(&b);
    }
    assert!(r.take_rest().is_empty());
    let b = r.batch(vec![], false);
    r.receive(&b);
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 200, .. }]));
}

/// R-NET-2: the caller giving up on a direct path goes back to the relay at
/// once; on the relay already, it does nothing.
#[test]
fn r_net_2_back_to_relay_rejoins_only_off_the_relay() {
    let (mut r, c) = with_a_candidate(0);
    assert!(r.core.back_to_relay(10).is_empty(), "on the relay");
    r.at(16).tick();
    r.take_rest();
    r.link += 1;
    let e = r.core.move_to(c, &json!({"type": "joined", "link": r.link, "host": true, "seq": 0}), 100);
    r.take(e);
    assert!(matches!(r.core.back_to_relay(500).as_slice(), [Effect::Join { at_ms: 500, .. }]));
    assert!(r.core.back_to_relay(600).is_empty(), "once");
}

/// R-OFF-2: a rejoin resumes from the cursor: the relay is asked for what
/// follows the last batch opened, a batch it sends again is acked rather
/// than opened, and the next one is no gap.
#[test]
fn r_off_2_a_rejoin_resumes_from_the_cursor() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    let b: Vec<Vec<u8>> = (1..=3).map(|n| r.batch(vec![logged(n)], false)).collect();
    for b in &b {
        r.receive(b);
    }
    let e = r.core.closed(100);
    let stream = krowk_client::e2e::hex(&r.host.stream());
    assert!(matches!(e.as_slice(), [Effect::Join { at_ms: 1100, extra }] if *extra == json!({"stream": stream, "afterSeq": 3})), "{e:?}");
    r.at(1100);
    r.link += 1;
    let e = r.core.joined(&json!({"type": "joined", "link": r.link, "host": true, "stream": stream, "seq": 9}), 1100);
    r.take(e);
    r.welcome_with(Welcome::default());
    r.read(Ok((vec![], Attached::default())));
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    r.acks.clear();
    r.frames.clear();
    r.receive(&b[2]);
    assert_eq!(r.acks, [3], "opened already: acked with the cursor");
    let b4 = r.batch(vec![logged(4)], false);
    r.receive(&b4);
    r.at(1200).tick();
    let shown = r.updates();
    assert!(!shown.iter().any(|u| matches!(u, Update::Gap)), "{shown:?}");
    assert!(catch_ups(&r.frames).is_empty());
}

/// R-OFF-2: a `resync` the relay sends a welcomed viewer asks the host to
/// catch up from the newest event held; with one in flight, once it ends.
#[test]
fn r_off_2_a_resync_while_welcomed_asks_the_host() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    let b = r.batch(vec![logged(1)], false);
    r.receive(&b);
    r.frames.clear();
    let resync = json!({"type": "resync", "reason": "behind", "stream": "00", "seq": 0});
    r.control(resync.clone());
    assert_eq!(catch_ups(&r.frames), [Some(id(1))]);
    r.control(resync);
    assert_eq!(catch_ups(&r.frames).len(), 1, "one in flight");
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    assert_eq!(catch_ups(&r.frames), [Some(id(1)), Some(id(1))], "asked once it ended");
    assert!(r.take_rest().is_empty(), "no read, no join");
}

/// R-NET-2: a race won with no host at the candidate is refused, and the
/// next one put off; the caller drops that connection.
#[test]
fn r_net_2_a_candidate_with_no_host_is_refused() {
    let (mut r, c) = with_a_candidate(0);
    r.at(0).tick();
    assert_eq!(probes(&mut r), 1);
    let e = r.core.move_to(c, &json!({"type": "joined", "link": 9, "host": false, "seq": 0}), 50);
    assert!(e.is_empty(), "{e:?}");
    r.at(10_049).tick();
    assert_eq!(probes(&mut r), 0);
    r.at(10_050).tick();
    assert_eq!(probes(&mut r), 1);
}

/// The logged events shown, by id, in the order shown: lines and catch-ups.
fn shown_ids(u: &[Update]) -> Vec<String> {
    let mut ids = Vec::new();
    for u in u {
        match u {
            Update::Line(StreamLine::Log(e)) => ids.push(e.id.clone()),
            Update::CaughtUp(events) => ids.extend(events.iter().map(|e| e["id"].as_str().unwrap().to_string())),
            _ => {}
        }
    }
    ids
}

fn joins(effects: &[Effect]) -> Vec<(u64, Value)> {
    effects.iter().filter_map(|e| if let Effect::Join { at_ms, extra } = e { Some((*at_ms, extra.clone())) } else { None }).collect()
}

/// R-CLIENT-3, R-OFF-2 (ticket 115): a welcome whose chunks could not be
/// read for now — a registry hiccup — does not end the viewer: it says why,
/// drops the connection and joins the relay again a second later, from the
/// cursor, with what was held and what waited on the read gone with the
/// connection. The host welcomes it again, the read goes through, and every
/// event shows once: none lost, none twice. A command typed meanwhile goes
/// once, at the welcome that was acted on.
#[test]
fn r_client_3_a_transient_read_failure_during_a_welcome_rejoins_and_catches_up() {
    let mut r = Relay::new(vec![logged(1)], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    let b1 = r.batch(vec![logged(2)], false);
    r.receive(&b1);
    r.at(16).tick();
    assert_eq!(shown_ids(&r.updates()), [id(2)]);

    // The relay lets the viewer go; joined again, welcomed, and while the
    // welcome's read is out batch 2 is held, the person types, and the
    // connection closes.
    let stream = krowk_client::e2e::hex(&r.host.stream());
    assert!(matches!(joins(&r.core.closed(100)).as_slice(), [(1100, _)]));
    r.at(1100);
    r.link += 1;
    let e = r.core.joined(&json!({"type": "joined", "link": r.link, "host": true, "stream": stream, "seq": 1}), 1100);
    r.take(e);
    r.welcome_with(Welcome::default());
    assert!(matches!(r.take_rest().as_slice(), [Effect::ReadChunks { want: None }]));
    let b2 = r.batch(vec![logged(3)], false);
    r.receive(&b2);
    r.command(stop(1));
    assert!(r.core.closed(1150).is_empty(), "held behind the read");
    r.frames.clear();

    r.at(1200).read(Err(ReadError::transient("http_503")));
    let rest = r.take_rest();
    assert!(!rest.iter().any(|e| matches!(e, Effect::End)), "{rest:?}");
    assert_eq!(joins(&rest), [(2200, json!({"stream": stream, "afterSeq": 1}))], "once, a second on, from the cursor: {rest:?}");
    assert!(r.frames.is_empty(), "the welcome not acted on: no catch-up, no command");
    r.at(1216).tick();
    let shown = r.updates();
    assert!(shown.iter().any(|u| matches!(u, Update::Note(n) if n.contains("http_503"))), "said why: {shown:?}");
    assert!(shown_ids(&shown).is_empty() && !shown.iter().any(|u| matches!(u, Update::Failed(_) | Update::Host(true))), "{shown:?}");
    assert!(r.core.next_wake().is_some(), "not ended");

    // Joined again at the cursor: the relay sends batch 2 again, the read
    // goes through holding events 2 and 3, and batch 3 comes live.
    r.at(2200);
    r.link += 1;
    let e = r.core.joined(&json!({"type": "joined", "link": r.link, "host": true, "stream": stream, "seq": 2}), 2200);
    r.take(e);
    r.welcome_with(Welcome::default());
    r.receive(&b2);
    r.read(Ok((vec![logged(2), logged(3)], Attached::default())));
    assert_eq!(catch_ups(&r.frames), [Some(id(3))], "from the newest event held");
    assert_eq!(commands(&r.frames), [stop(1)], "the command typed meanwhile, once");
    let b3 = r.batch(vec![logged(4)], false);
    r.receive(&b3);
    r.answer(&Answer::CatchUp { events: vec![serde_json::from_value(logged(4)).unwrap()], more: false });
    r.at(2300).tick();
    let shown = r.updates();
    assert_eq!(shown_ids(&shown), [id(3), id(4)], "each once: {shown:?}");
    assert!(!shown.iter().any(|u| matches!(u, Update::Gap)), "{shown:?}");
    assert_eq!(hosts(&shown), [true]);
}

/// R-CLIENT-3 (ticket 115): a welcome's read that fails for good ends the
/// viewer whatever the cause; one that failed for now never does.
#[test]
fn r_client_3_only_a_permanent_read_failure_during_a_welcome_ends() {
    for (failure, ends) in [
        (ReadError::permanent("the registry no longer holds the session (not_found)"), true),
        (ReadError::permanent(format!("chunk 3 is sealed under session key epoch 2 — {}", krowk_client::e2e::ROTATED_SINCE_OPENED)), true),
        (ReadError::permanent("the registry served the log only to chunk 2, where the session's host has sealed to chunk 4: a prefix, refused"), true),
        (ReadError::transient("network_unreachable"), false),
        (ReadError::transient("checksum_mismatch"), false),
    ] {
        let mut r = Relay::new(vec![], 0);
        r.joined(true, 0);
        r.welcome_with(Welcome::default());
        r.take_rest();
        r.at(10).read(Err(failure.clone()));
        let rest = r.take_rest();
        assert_eq!(rest.iter().any(|e| matches!(e, Effect::End)), ends, "{failure:?}: {rest:?}");
        assert_eq!(joins(&rest).len(), usize::from(!ends), "{failure:?}: {rest:?}");
        assert_eq!(r.core.next_wake().is_none(), ends);
        if ends {
            assert!(matches!(r.updates().last(), Some(Update::Failed(e)) if *e == failure.detail));
        }
    }
}

/// R-CLIENT-3, R-OFF-2 (ticket 115): a welcome's read that keeps failing
/// joins again after 1 s, then 2, 4 … up to 30 s, never sooner, and says
/// why once while the reason holds; a read that goes through starts the
/// wait over.
#[test]
fn r_client_3_repeated_read_failures_back_off_and_never_spin() {
    let mut r = Relay::new(vec![], 0);
    let mut t = 0;
    let welcomed_and_failed = |r: &mut Relay, t: u64, why: &str| -> Vec<(u64, Value)> {
        r.at(t).joined(true, 0);
        r.welcome_with(Welcome::default());
        assert!(matches!(r.take_rest().as_slice(), [Effect::ReadChunks { .. }]));
        r.read(Err(ReadError::transient(why)));
        joins(&r.take_rest())
    };
    for wait in [1, 2, 4, 8, 16, 30, 30, 30] {
        let j = welcomed_and_failed(&mut r, t, "http_502");
        assert_eq!(j.iter().map(|(at, _)| *at).collect::<Vec<_>>(), [t + wait * 1000], "at {t}");
        // Nothing until then but the frame: no join, no read, no ping.
        while let Some(w) = r.core.next_wake().filter(|w| *w < t + wait * 1000) {
            r.at(w.max(t)).tick();
            assert!(r.take_rest().is_empty(), "at {w}");
        }
        t += wait * 1000;
    }
    let notes = r.updates().into_iter().filter(|u| matches!(u, Update::Note(_))).count();
    assert_eq!(notes, 1, "said once while the reason holds");
    let j = welcomed_and_failed(&mut r, t, "network_unreachable");
    assert_eq!(j[0].0, t + 30_000, "still backed off");
    r.at(t + 16).tick();
    assert_eq!(r.updates().into_iter().filter(|u| matches!(u, Update::Note(_))).count(), 1, "a new reason is said");

    // Through: the next failure waits a second again.
    t += 30_000;
    r.at(t).joined(true, 0);
    r.welcome_with(Welcome::default());
    r.take_rest();
    r.read(Ok((vec![], Attached::default())));
    t += 5_000;
    r.at(t).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    r.take_rest();
    let j = welcomed_and_failed(&mut r, t, "http_502");
    assert_eq!(j[0].0, t + 1000);
}

/// Ticket 115: a welcome onto a new host's stream whose read fails for now
/// joins again from where the relay stood at the join, not from the new
/// stream's first batch, so lines the viewer was never sent are not
/// replayed; and a welcome onto a direct path whose read fails puts the
/// next race off rather than racing again as soon as the relay welcomes.
#[test]
fn r_client_3_r_net_2_a_failed_read_on_a_new_stream_or_a_direct_path_rejoins_where_it_should() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    let b = r.batch(vec![logged(1)], false);
    r.receive(&b);
    // A new host: another stream, its relay count at 7 as the viewer joins.
    r.at(100).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    r.take_rest();
    r.host = HostLink::new(&r.key, uuid(SESSION));
    let new = krowk_client::e2e::hex(&r.host.stream());
    // Its first seven batches went out before this viewer joined.
    let batches: Vec<Vec<u8>> = (2..=9).map(|n| r.batch(vec![logged(n)], false)).collect();
    r.at(150).tick();
    r.updates();
    // Twice in a row: the second join, the relay moved on to 9 meanwhile,
    // still resumes from 7, where the viewer was carried from.
    for (t, wait, seq) in [(200, 1000, 7), (1200, 2000, 9)] {
        r.link += 1;
        let e = r.core.joined(&json!({"type": "joined", "link": r.link, "host": true, "stream": new, "seq": seq}), t);
        r.take(e);
        r.at(t).welcome_with(Welcome::default());
        r.take_rest();
        r.read(Err(ReadError::transient("http_500")));
        assert_eq!(joins(&r.take_rest()), [(t + wait, json!({"stream": new, "afterSeq": 7}))], "at {t}");
    }
    // Through at last: the relay's batch 8 is the next, no gap.
    r.link += 1;
    let e = r.core.joined(&json!({"type": "joined", "link": r.link, "host": true, "stream": new, "seq": 7}), 3200);
    r.take(e);
    r.at(3200).welcome_with(Welcome::default());
    r.read(Ok((vec![], Attached::default())));
    r.receive(&batches[7]);
    r.at(3300).tick();
    let shown = r.updates();
    assert_eq!(shown_ids(&shown), [id(9)], "{shown:?}");
    assert!(!shown.iter().any(|u| matches!(u, Update::Gap)), "{shown:?}");

    // On a direct path: the read fails, the relay is joined again, and the
    // next race waits its 10 s.
    let (mut r, c) = with_a_candidate(0);
    r.at(0).tick();
    assert_eq!(probes(&mut r), 1);
    r.link += 1;
    let e = r.core.move_to(c.clone(), &json!({"type": "joined", "link": r.link, "host": true, "seq": 0}), 100);
    r.take(e);
    r.at(120).welcome_with(Welcome::default());
    r.take_rest();
    r.read(Err(ReadError::transient("http_500")));
    assert_eq!(joins(&r.take_rest()).len(), 1);
    r.at(1120).joined(true, 0);
    r.welcome_with(Welcome { candidates: vec![c], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.take_rest();
    r.at(1200).tick();
    assert_eq!(probes(&mut r), 0, "not at once");
    r.at(10_119).tick();
    assert_eq!(probes(&mut r), 0);
    r.at(10_120).tick();
    assert_eq!(probes(&mut r), 1, "10 s after the read failed");
}

/// The places the `Show`s since the last call ended with.
fn places(r: &mut Relay) -> Vec<(u64, Option<String>)> {
    std::mem::take(&mut r.saved).into_iter().map(|c| (c.after_seq, c.newest)).collect()
}

/// R-OFF-2, R-IOS-1: a batch that opens but does not parse is a gap — the
/// stream's counter refuses it if it comes again — so it shows `Gap` and
/// the host is asked for the events after the one before it. The cursor
/// moves past it, so the next batch is no second gap.
#[test]
fn r_off_2_r_ios_1_a_batch_that_does_not_parse_is_a_gap() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    r.frames.clear();
    let b1 = r.batch(vec![logged(1)], false);
    let bad = r.host.batch(b"{\"lines\": [", false).unwrap();
    let b3 = r.batch(vec![logged(3)], false);
    r.receive(&b1);
    r.receive(&bad);
    assert_eq!(catch_ups(&r.frames), [Some(id(1))], "from the event before it");
    r.receive(&b3);
    assert_eq!(catch_ups(&r.frames), [Some(id(1))], "the next batch is no second gap");
    r.at(16).tick();
    let shown = r.updates();
    assert_eq!(shown.iter().filter(|u| matches!(u, Update::Gap)).count(), 1, "{shown:?}");
    assert_eq!(r.acks.last(), Some(&3));
    let stream = krowk_client::e2e::hex(&r.host.stream());
    assert_eq!(r.core.cursor_extra(), json!({"stream": stream, "afterSeq": 3}));
}

/// R-IOS-1: `Saved` ends each `Show` that moved the viewer's place, and
/// names the stream, the last batch shown and the newest event shown. While
/// a catch-up for a gap is out it names the event the catch-up asks from,
/// so a place kept across launches never catches up from past the gap.
#[test]
fn r_ios_1_saved_ends_the_show_and_names_the_newest_event_shown() {
    let mut r = Relay::new(vec![logged(0)], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    r.at(16).tick();
    assert_eq!(places(&mut r), [(0, Some(id(0)))], "welcomed: where the relay stood, the newest held");
    let stream = krowk_client::e2e::hex(&r.host.stream());

    let (b1, b2) = (r.batch(vec![logged(1)], false), r.batch(vec![logged(2)], false));
    r.receive(&b1);
    r.receive(&b2);
    r.whole.clear();
    r.at(32).tick();
    let show = r.whole.last().expect("a Show").clone();
    assert!(matches!(show.last(), Some(Update::Saved(c)) if *c == Cursor { stream: stream.clone(), after_seq: 2, newest: Some(id(2)) }), "{show:?}");
    assert_eq!(show.iter().filter(|u| matches!(u, Update::Saved(_))).count(), 1);
    places(&mut r);

    // A Show that moved nothing says no place.
    r.command(stop(1));
    r.at(48).tick();
    assert!(r.saved.is_empty(), "{:?}", r.saved);

    // Batch 3 dropped: until the catch-up is answered, the place is the
    // event before the gap.
    let (_b3, b4) = (r.batch(vec![logged(3)], false), r.batch(vec![logged(4)], false));
    r.receive(&b4);
    r.at(64).tick();
    assert_eq!(places(&mut r), [(4, Some(id(2)))]);
    r.answer(&Answer::CatchUp { events: vec![serde_json::from_value(logged(3)).unwrap()], more: false });
    r.at(80).tick();
    assert_eq!(places(&mut r), [(4, Some(id(4)))], "caught up: the newest held");

    // A batch whose one line was seen already moves the batch, not the
    // newest event.
    let dup = r.batch(vec![logged(3)], false);
    r.receive(&dup);
    r.at(96).tick();
    assert_eq!(places(&mut r), [(5, Some(id(4)))]);
}

/// R-IOS-1: a logged event this build cannot read (a newer host's event
/// type) is skipped like any line it does not know, and the place never
/// reaches past it: a relaunch on a build that reads it catches it up.
#[test]
fn r_ios_1_the_place_stays_before_a_logged_event_this_build_cannot_read() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    let future = json!({"id": id(2), "sessionId": "s", "timeMs": 0, "type": "session.someday", "what": 1});
    let (b1, b2, b3) = (r.batch(vec![logged(1)], false), r.batch(vec![future, logged(3)], false), r.batch(vec![logged(4)], false));
    r.receive(&b1);
    r.receive(&b2);
    r.receive(&b3);
    r.at(16).tick();
    let shown = r.updates();
    assert_eq!(shown.iter().filter(|u| matches!(u, Update::Line(_))).count(), 3, "the rest shows: {shown:?}");
    assert!(!shown.iter().any(|u| matches!(u, Update::Gap)), "{shown:?}");
    assert_eq!(places(&mut r).last(), Some(&(3, Some(id(1)))), "the event before the one it could not read");
    let b5 = r.batch(vec![logged(5)], false);
    r.receive(&b5);
    r.at(32).tick();
    assert_eq!(places(&mut r), [(4, Some(id(1)))], "nor later");
}

/// R-IOS-1: a catch-up page holding a logged event this build cannot read
/// still applies the rest, and the place holds from where the catch-up
/// asked: never past the events the page brought.
#[test]
fn r_ios_1_a_catch_up_page_with_an_event_this_build_cannot_read_applies_the_rest() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.answer(&Answer::CatchUp { events: vec![], more: false });
    r.frames.clear();
    let (b1, _b2, b3) = (r.batch(vec![logged(1)], false), r.batch(vec![logged(2)], false), r.batch(vec![logged(4)], false));
    r.receive(&b1);
    r.receive(&b3);
    assert_eq!(catch_ups(&r.frames), [Some(id(1))]);
    let future = json!({"id": id(3), "sessionId": "s", "timeMs": 0, "type": "session.someday"});
    let page = serde_json::to_vec(&json!({"type": "catchUp", "events": [logged(2), future], "more": false})).unwrap();
    let routed = r.host.to_viewer(r.link, &page, false).unwrap();
    r.receive(&routed[HEADER..]);
    r.at(16).tick();
    let shown = r.updates();
    assert!(shown.iter().any(|u| matches!(u, Update::CaughtUp(e) if e.iter().any(|e| e["id"] == id(2)))), "the rest applies: {shown:?}");
    assert_eq!(places(&mut r).last(), Some(&(3, Some(id(1)))), "from where the catch-up asked");
    let b4 = r.batch(vec![logged(5)], false);
    r.receive(&b4);
    r.at(32).tick();
    assert_eq!(places(&mut r), [(4, Some(id(1)))]);
}

/// R-IOS-1 (ticket 131): a viewer opened from the place the app read to,
/// its cache holding the session up to an event, asks the host at the
/// welcome only for what came after that event — or after the newest the
/// chunks read since held, when that is newer. With nothing held, from the
/// start.
#[test]
fn r_ios_1_a_resumed_viewers_catch_up_starts_after_the_cached_newest() {
    for (at_rest, newest, after) in [(vec![], Some(id(5)), Some(id(5))), (vec![logged(7)], Some(id(5)), Some(id(7))), (vec![logged(3)], Some(id(5)), Some(id(5))), (vec![], None, None)] {
        let key = SessionKey::generate();
        let host = HostLink::new(&key, uuid(SESSION));
        let at_rest = Attached { events: at_rest, ..Attached::default() };
        let (core, first) = ViewerCore::resumed(SessionKeys::from(key.clone()), SESSION, DeviceId([1; 16]), RELAY, at_rest, newest.clone(), 0);
        let mut r = Relay { core, key, host, link: 0, now: 0, shown: Vec::new(), whole: Vec::new(), saved: Vec::new(), frames: Vec::new(), acks: Vec::new(), rest: Vec::new() };
        r.take(first);
        r.rest.clear();
        r.joined(true, 0);
        r.welcome_with(Welcome::default());
        assert!(r.take_rest().iter().any(|e| matches!(e, Effect::ReadChunks { want: None })));
        r.read(Ok((vec![], Attached::default())));
        assert_eq!(catch_ups(&r.frames), std::slice::from_ref(&after), "held up to {newest:?}");
    }
}
