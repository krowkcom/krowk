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
use krowk_client::sync::store::Attached;
use krowk_client::sync::viewer::{Effect, Update, ViewerCore};
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
    host: HostLink,
    link: u64,
    now: u64,
    /// Every `Show`, in order.
    shown: Vec<Vec<Update>>,
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
        let (core, first) = ViewerCore::new(SessionKeys::from(key), SESSION, DeviceId([1; 16]), RELAY, at_rest, now);
        let mut r = Relay { core, host, link: 0, now, shown: Vec::new(), frames: Vec::new(), acks: Vec::new(), rest: Vec::new() };
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
                Effect::Show(u) => self.shown.push(u),
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

    fn read(&mut self, r: Result<(Vec<Value>, Attached), String>) {
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
/// in the order typed — and one typed while the read is out goes after
/// them. Nothing is sent between the welcome and the read.
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

    r.read(Ok((vec![logged(1), logged(2)], Attached::default())));
    assert_eq!(catch_ups(&r.frames), [Some(id(2))], "the catch-up from the newest event the chunks held");
    assert!(matches!(r.frames[0], ViewerFrame::CatchUp { .. }), "the catch-up first");
    assert_eq!(commands(&r.frames), [stop(1), stop(2), stop(3)]);
    assert_eq!(r.acks, [0], "acked at the cursor");
    r.at(32).tick();
    let shown = r.updates();
    assert!(matches!(shown.as_slice(), [Update::Path { .. }, Update::CaughtUp(e), Update::Host(true), Update::Sent { queued: false, .. }] if e.len() == 1 && e[0]["id"] == id(2)), "{shown:?}");
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
    assert_eq!(r.acks, [3]);
    r.at(32).tick();
    let shown = r.updates();
    let lines = shown.iter().filter(|u| matches!(u, Update::Line(_))).count();
    assert_eq!((lines, shown.iter().filter(|u| matches!(u, Update::Gap)).count()), (2, 1), "{shown:?}");
}

/// R-LAG-4: the viewer acks the relay once four batches are applied, and
/// the rest at the next tick.
#[test]
fn r_lag_4_an_ack_after_four_batches_and_on_the_next_tick() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.acks.clear();
    for n in 1..=4 {
        let b = r.batch(vec![logged(n)], false);
        r.receive(&b);
    }
    assert_eq!(r.acks, [4]);
    let b = r.batch(vec![logged(5)], false);
    r.receive(&b);
    assert_eq!(r.acks, [4], "not yet");
    assert_eq!(r.core.next_wake(), Some(16), "due at the next display frame");
    r.at(16).tick();
    assert_eq!(r.acks, [4, 5]);
    r.at(40).tick();
    assert_eq!(r.acks, [4, 5], "once");
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
/// cursor, so the relay's buffer fills the gap.
#[test]
fn r_off_2_a_stream_resync_joins_again() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    let b = r.batch(vec![logged(1)], false);
    r.receive(&b);
    r.at(500).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    let stream = krowk_client::e2e::hex(&r.host.stream());
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { at_ms: 500, extra }] if *extra == json!({"stream": stream, "afterSeq": 1})));
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

    r.frames.clear();
    r.at(41_000).live();
    assert_eq!(commands(&r.frames), [stop(1)], "sent again at the welcome");
}

/// R-CLIENT-3: a `pong`, or any message, keeps the connection alive.
#[test]
fn r_client_3_any_word_keeps_the_connection() {
    let mut r = Relay::new(vec![], 0);
    r.live();
    r.core.alive(25_000);
    for t in [10_000, 20_000, 30_000, 40_000, 50_000] {
        r.at(t).tick();
        assert!(matches!(r.take_rest().as_slice(), [Effect::Ping]), "at {t}");
    }
    r.at(60_000).tick();
    assert!(matches!(r.take_rest().as_slice(), [Effect::Join { .. }]));
}

/// R-CLIENT-3: a welcome whose chunks cannot be read up to its head shows
/// `Failed` and ends the viewer: nothing after does anything.
#[test]
fn r_client_3_a_failed_read_shows_failed_and_ends() {
    let mut r = Relay::new(vec![], 0);
    r.joined(true, 0);
    r.welcome_with(Welcome::default());
    r.take_rest();
    r.read(Err("the log is shorter than the head".into()));
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
    let b = r.batch(vec![approval("r1")], false);
    r.receive(&b);
    r.at(5_000).control(json!({"type": "resync", "reason": "stream", "stream": "00", "seq": 0}));
    r.joined(true, 0);
    r.welcome_with(Welcome { approvals: vec![asked], ..Welcome::default() });
    r.read(Ok((vec![], Attached::default())));
    r.at(6_000).tick();
    let shown = r.updates();
    let n = shown.iter().filter(|u| matches!(u, Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(a))) if a.request_id == "r1")).count();
    assert_eq!(n, 1, "{shown:?}");
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
    r.at(3000).tick();
    let notes = r.updates().into_iter().filter(|u| matches!(u, Update::Note(n) if n.starts_with("not on the relay wss://relay.example: the relay could not be reached"))).count();
    assert_eq!(notes, 1);
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
