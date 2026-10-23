//! R-CLIENT-1, R-HAND-4, R-IOS-5: the phone follows a session live through
//! `Viewer`, krowk-client's `ViewerCore` and join handshake behind the FFI,
//! never a viewer written again in Swift. The test plays Swift, the relay
//! and the host, in memory, as krowk-client's tests/viewer_core.rs does:
//! the relay's challenge and `joined` are binary control messages,
//! `relay_link::HostLink` is the host, and the time is whatever `now` the
//! test passes. The session is written at rest by a host on the stand-in
//! registry and opened by a paired phone, as tests/sessions.rs does, so
//! `read_chunks` reads the real chunks.

mod common;

use std::sync::Arc;

use common::*;
use krowk_client::e2e::{self, RELAY_ROLE_VIEWER};
use krowk_client::keystore::Keystore;
use krowk_client::protocol::frame::{HEADER, KIND_ACK, KIND_FRAME};
use krowk_client::protocol::{Command, LogBody, StreamLine};
use krowk_client::relay_link::{HostLink, Inbound};
use krowk_client::sync::join::{control, read_control};
use krowk_client::sync::{Answer, Candidate, ViewerFrame, Welcome, uuid};
use krowk_ffi::{Effect, OpenedSession, Viewer, ViewerUpdate, WsFrame};
use serde_json::{Value, json};

const SESSION: &str = "01a0ec7b-3333-7000-8000-000000000080";
const RELAY: &str = "wss://relay.example";
const NONCE: [u8; 32] = [9; 32];

fn stop() -> Command {
    Command::Interrupt { session_id: SESSION.into() }
}

/// Swift, the relay and the host around one `Viewer`, and what the viewer
/// told Swift to do, sorted for the test to read.
struct Phone {
    world: World,
    hosted: Hosted,
    opened: Arc<OpenedSession>,
    viewer: Arc<Viewer>,
    host: HostLink,
    link: u64,
    /// The connection the last `Dial` named.
    conn: u64,
    now: u64,
    /// Every `Show`'s updates, one after the other.
    shown: Vec<ViewerUpdate>,
    /// The frames the host opened from the viewer, in order.
    frames: Vec<ViewerFrame>,
    /// Hellos the host opened.
    hellos: usize,
    /// Acks to the relay, by seq.
    acks: Vec<u64>,
    /// Every other effect, in order.
    rest: Vec<Effect>,
}

impl Phone {
    /// A host writing the session (its first event, a checkpoint, then the
    /// second), and the phone that opened it and started a viewer on it.
    fn new() -> Phone {
        let world = World::new();
        let hosted = world.host_session(SESSION, "follow me", &[event(SESSION, 1, "first")], &[event(SESSION, 2, "second")]);
        let opened = world.phone.open_session(SESSION.into(), None, Some(RELAY.into())).unwrap();
        let viewer = Viewer::open(opened.clone(), 0).unwrap();
        let host = HostLink::new(hosted.keys.current(), uuid(SESSION));
        Phone { world, hosted, opened, viewer, host, link: 0, conn: 0, now: 0, shown: Vec::new(), frames: Vec::new(), hellos: 0, acks: Vec::new(), rest: Vec::new() }
    }

    fn take(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Show { updates } => self.shown.extend(updates),
                Effect::Send { bytes } if bytes.len() >= HEADER && bytes[1] == KIND_ACK => self.acks.push(u64::from_be_bytes(bytes[20..28].try_into().unwrap())),
                Effect::Send { bytes } if bytes.len() >= HEADER && bytes[1] == KIND_FRAME => match self.host.open(self.link, &bytes).expect("the host opens the viewer's frame") {
                    Inbound::Hello { .. } => self.hellos += 1,
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

    fn start(&mut self) {
        let e = self.viewer.start(self.now);
        self.take(e);
    }

    /// The ticket handed in: the connection its `Dial` names is the
    /// current one from here.
    fn ticket(&mut self, t: &str) {
        let e = self.viewer.ticket(t.into(), self.now);
        for e in &e {
            if let Effect::Dial { connection, .. } = e {
                self.conn = *connection;
            }
        }
        self.take(e);
    }

    fn closed(&mut self) {
        let e = self.viewer.closed(self.conn, self.now);
        self.take(e);
    }

    fn receive(&mut self, bytes: &[u8]) {
        let e = self.viewer.received(self.conn, WsFrame::Binary { bytes: bytes.to_vec() }, self.now);
        self.take(e);
    }

    fn relay(&mut self, v: Value) {
        self.receive(&control(&v));
    }

    /// The challenge: the join it is answered with, read back.
    fn challenge(&mut self) -> Value {
        self.relay(json!({"type": "challenge", "version": 1, "nonce": e2e::hex(&NONCE), "relay": RELAY, "session": SESSION}));
        match self.rest.pop() {
            Some(Effect::Send { bytes }) => read_control(&bytes).expect("the join is a control message"),
            other => panic!("the challenge is answered with a join: {other:?}"),
        }
    }

    /// The relay's `joined`, on a new link.
    fn joined(&mut self, host: bool, seq: u64) {
        self.link += 1;
        self.relay(json!({"type": "joined", "link": self.link, "host": host, "seq": seq}));
    }

    /// From a `NeedTicket` to `joined`: ticket, dial, challenge, join.
    fn join(&mut self, host: bool, seq: u64) -> Value {
        assert!(matches!(self.rest.pop(), Some(Effect::NeedTicket { .. })), "{:?}", self.rest);
        self.ticket("t");
        assert!(matches!(self.rest.pop(), Some(Effect::Dial { .. })));
        let join = self.challenge();
        self.joined(host, seq);
        join
    }

    fn welcome_with(&mut self, w: Welcome) {
        let routed = self.host.welcome(self.link, &serde_json::to_vec(&w).unwrap()).unwrap();
        self.receive(&routed[HEADER..]);
    }

    /// The welcome, then the chunks read as Swift reads them, off the lock.
    fn welcome(&mut self, w: Welcome) {
        self.welcome_with(w);
        assert!(matches!(self.rest.as_slice(), [.., Effect::ReadChunks]), "a welcome asks for the chunks: {:?}", self.rest);
        self.rest.clear();
        let read = self.viewer.read_chunks();
        let e = self.viewer.chunks_read(read, self.now);
        self.take(e);
    }

    /// A batch of `events` from the host, sealed, not yet sent.
    fn batch(&mut self, events: &[Value]) -> Vec<u8> {
        self.host.batch(&serde_json::to_vec(&json!({"lines": events, "head": null})).unwrap(), false).unwrap()
    }

    fn answer(&mut self, a: &Answer) {
        let routed = self.host.to_viewer(self.link, &serde_json::to_vec(a).unwrap(), false).unwrap();
        self.receive(&routed[HEADER..]);
    }

    /// A display frame later: what was shown, taken.
    fn frame(&mut self) -> Vec<ViewerUpdate> {
        self.now += 16;
        let e = self.viewer.tick(self.now);
        self.take(e);
        std::mem::take(&mut self.shown)
    }

    fn command(&mut self, c: Command) {
        let e = self.viewer.command(c, self.now);
        self.take(e);
    }

    fn take_rest(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.rest)
    }

    /// Started, joined with a host there, welcomed and the chunks read.
    fn live() -> Phone {
        let mut p = Phone::new();
        p.start();
        p.shown.clear();
        p.join(true, 0);
        p.welcome(Welcome::default());
        p.frame();
        p.frames.clear();
        p.acks.clear();
        p
    }
}

/// The runs of every event a list of updates carries in its lines.
fn line_runs(u: &[ViewerUpdate]) -> Vec<String> {
    u.iter()
        .filter_map(|u| match u {
            ViewerUpdate::Line { line: StreamLine::Log(e) } => match &e.body {
                LogBody::RunOpened { run, .. } => Some(run.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn catch_ups(frames: &[ViewerFrame]) -> Vec<Option<String>> {
    frames.iter().filter_map(|f| if let ViewerFrame::CatchUp { after } = f { Some(after.clone()) } else { None }).collect()
}

fn commands(frames: &[ViewerFrame]) -> Vec<(String, Command)> {
    frames.iter().filter_map(|f| if let ViewerFrame::Command(r) = f { Some((r.id.clone(), r.command.clone())) } else { None }).collect()
}

/// R-CLIENT-1: start shows the session as read at rest and asks for a
/// ticket; the ticket dials with itself kept out of the URL; the relay's
/// challenge is answered with a join signed by this phone's key over the
/// relay dialed, verified here; `joined` with a host there says hello.
#[test]
fn r_client_1_the_viewer_joins_the_relay_through_the_handshake() {
    let mut p = Phone::new();
    p.start();
    match p.shown.as_slice() {
        [ViewerUpdate::Attached { events, head }] => {
            assert_eq!(events.len(), 2, "the checkpoint's event and the tail's: {events:?}");
            assert_eq!(head, &p.opened.head());
        }
        other => panic!("the session at rest first: {other:?}"),
    }
    assert!(matches!(p.rest.as_slice(), [Effect::NeedTicket { at_ms: 0 }]), "{:?}", p.rest);
    assert!(p.viewer.start(0).is_empty(), "start's effects are given once");

    p.take_rest();
    p.ticket("secret-ticket");
    match p.take_rest().as_slice() {
        [Effect::Dial { url, ticket, .. }] => {
            assert_eq!(url, &format!("{RELAY}/v1/relay/{SESSION}?env=development"));
            assert!(!url.contains("secret-ticket"), "the ticket rides the header, never the URL");
            assert_eq!(ticket, "secret-ticket");
        }
        other => panic!("a dial: {other:?}"),
    }
    assert!(p.viewer.ticket("again".into(), 0).is_empty(), "no second dial for one join");

    let join = p.challenge();
    assert_eq!(join["type"], "join");
    assert_eq!(join["role"], "viewer");
    assert_eq!(join["env"], "development");
    let device = e2e::DeviceId::parse(join["device"].as_str().unwrap()).unwrap();
    assert_eq!(device.to_string(), p.world.phone.device_id());
    let signing = Keystore::with(Arc::new(Bridge(p.world.storage.clone()))).signing_key().unwrap();
    let sig = e2e::unhex(join["signature"].as_str().unwrap()).unwrap();
    signing.public().verify_relay_join(&sig, RELAY_ROLE_VIEWER, &uuid(SESSION), &NONCE, &device, RELAY).expect("the join verifies as this phone's, for this relay");

    p.joined(true, 0);
    assert_eq!(p.hellos, 1, "hello said to the host: {:?}", p.rest);
    assert!(p.rest.is_empty(), "{:?}", p.rest);
}

/// R-CLIENT-1: the host's welcome shows the host there; batch 1 shows
/// its line; batch 3 after 1 (the relay dropped 2) shows a gap and asks the
/// host to catch up from batch 1's event.
#[test]
fn r_client_1_welcome_line_and_a_gap_that_asks_for_a_catch_up() {
    let mut p = Phone::new();
    p.start();
    p.shown.clear();
    p.join(true, 0);
    p.welcome(Welcome::default());
    let shown = p.frame();
    assert!(shown.iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })), "{shown:?}");
    assert!(shown.iter().any(|u| matches!(u, ViewerUpdate::Path { path, via: None } if path == "relay")), "{shown:?}");
    assert_eq!(catch_ups(&p.frames), [Some(event(SESSION, 2, "")["id"].as_str().unwrap().to_owned())], "the welcome's catch-up, from the newest event held");
    p.answer(&Answer::CatchUp { events: vec![], more: false });
    p.frames.clear();

    let (b1, _b2, b3) = (p.batch(&[event(SESSION, 3, "three")]), p.batch(&[event(SESSION, 4, "four")]), p.batch(&[event(SESSION, 5, "five")]));
    p.receive(&b1);
    assert_eq!(line_runs(&p.frame()), ["three"]);
    assert!(p.frames.is_empty());
    p.receive(&b3);
    let shown = p.frame();
    assert_eq!(line_runs(&shown), ["five"]);
    assert_eq!(shown.iter().filter(|u| matches!(u, ViewerUpdate::Gap)).count(), 1, "{shown:?}");
    assert_eq!(catch_ups(&p.frames), [Some(event(SESSION, 3, "")["id"].as_str().unwrap().to_owned())], "caught up from batch 1's event");
}

/// R-HAND-4, R-IOS-5: with no host, a command shows at once as queued and
/// nothing is sent; at the host's welcome it goes, and the host's ack
/// shows. Its id is `<device>-<run id>-<counter>`, and one sent and not
/// acked before the connection went is sent again under the same id, so
/// the host runs it once.
#[test]
fn r_hand_4_r_ios_5_a_command_with_no_host_queues_and_goes_at_the_welcome() {
    let mut p = Phone::new();
    p.start();
    p.shown.clear();
    p.join(false, 0);
    assert_eq!(p.hellos, 0, "no host to say hello to");
    p.command(stop());
    let shown = p.frame();
    let id = match shown.as_slice() {
        [ViewerUpdate::Host { present: false }, ViewerUpdate::Sent { id, queued: true }] => id.clone(),
        other => panic!("read-only, the command queued: {other:?}"),
    };
    assert!(p.frames.is_empty() && p.rest.is_empty(), "nothing sent: {:?} {:?}", p.frames, p.rest);
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(parts.len(), 3, "{id}");
    assert_eq!(parts[0], p.world.phone.device_id());
    assert!(parts[1].len() == 16 && parts[1].chars().all(|c| c.is_ascii_hexdigit()), "a random run id: {id}");
    assert_eq!(parts[2], "1");

    // A host arrives: the relay says so, the viewer rejoins and is welcomed.
    p.relay(json!({"type": "host", "present": true}));
    assert!(matches!(p.rest.as_slice(), [Effect::Close, Effect::NeedTicket { .. }]), "the connection dropped, a new one asked for: {:?}", p.rest);
    p.join(true, 0);
    p.welcome(Welcome::default());
    assert_eq!(commands(&p.frames), [(id.clone(), stop())], "sent at the welcome");

    // The connection goes before the ack: sent again, same id.
    p.frames.clear();
    p.closed();
    p.join(true, 0);
    p.welcome(Welcome::default());
    assert_eq!(commands(&p.frames), [(id.clone(), stop())], "the same id again: the host dedups on it");

    p.answer(&Answer::Ack { id: id.clone(), error: None });
    let shown = p.frame();
    assert!(shown.iter().any(|u| matches!(u, ViewerUpdate::Acked { id: a, error: None } if *a == id)), "{shown:?}");

    // Now live: the next command goes at once.
    p.frames.clear();
    p.command(stop());
    assert!(matches!(p.frame().as_slice(), [ViewerUpdate::Sent { queued: false, .. }]));
    assert!(commands(&p.frames)[0].0.ends_with("-2"), "{:?}", p.frames);
}

/// R-CLIENT-1: a connection that closes asks for a ticket again, a second
/// later (the core's backoff), with no `Close` for a connection already
/// gone; one that closes before the relay's `joined` is a failed join,
/// said once; one whose join the relay refuses is dropped (`Close`) and
/// asked for again; a ticket that could not be had is said and asked for
/// again too.
#[test]
fn r_client_1_a_closed_connection_or_a_failed_join_asks_for_a_ticket_again() {
    let mut p = Phone::live();
    assert!(p.viewer.ticket_failed("unasked".into(), p.now).is_empty(), "no join waits for a ticket");
    p.at(5_000);
    p.closed();
    match p.take_rest().as_slice() {
        [Effect::NeedTicket { at_ms }] => assert!(*at_ms > 5_000, "later, not at once: {at_ms}"),
        other => panic!("a ticket again, no Close: {other:?}"),
    }
    p.ticket("t");
    assert!(matches!(p.take_rest().as_slice(), [Effect::Dial { .. }]));
    p.closed();
    assert!(matches!(p.take_rest().as_slice(), [Effect::NeedTicket { .. }]));
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Note { text } if text.contains(RELAY))), "why it is not on the relay");

    // The relay refuses the join: that connection dropped, a ticket again.
    p.ticket("t");
    p.take_rest();
    p.challenge();
    p.relay(json!({"type": "error", "code": "unauthorized", "message": "the ticket has expired"}));
    assert!(matches!(p.take_rest().as_slice(), [Effect::Close, Effect::NeedTicket { .. }]));
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Note { text } if text.contains("the ticket has expired"))));

    let e = p.viewer.ticket_failed("the registry is down".into(), p.now);
    p.take(e);
    assert!(matches!(p.rest.as_slice(), [Effect::NeedTicket { .. }]));
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Note { text } if text.contains("the registry is down"))));

    // Rejoined from the cursor and welcomed again: live.
    p.join(true, 0);
    p.welcome(Welcome::default());
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })));
}

/// R-CLIENT-1: what the host wrote at rest since the phone opened the
/// session comes, at the welcome, from `read_chunks` and `chunks_read` as
/// `CaughtUp`.
#[test]
fn r_client_1_read_chunks_brings_what_was_written_since() {
    let mut p = Phone::new();
    p.start();
    p.shown.clear();
    p.join(true, 0);
    p.hosted.write(&[event(SESSION, 3, "since"), event(SESSION, 4, "and since")]);
    let head = p.world.host.list_chunks(SESSION, None, 100).unwrap().chunks.len();
    assert_eq!(head, 4, "chunks 0 to 3");

    p.welcome_with(Welcome::default());
    assert!(matches!(p.take_rest().as_slice(), [Effect::ReadChunks]));
    let read = p.viewer.read_chunks();
    let e = p.viewer.chunks_read(read, p.now);
    p.take(e);
    let shown = p.frame();
    let caught: Vec<String> = shown
        .iter()
        .filter_map(|u| if let ViewerUpdate::CaughtUp { events } = u { Some(events) } else { None })
        .flatten()
        .map(|e| match &e.body {
            LogBody::RunOpened { run, .. } => run.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(caught, ["since", "and since"], "{shown:?}");
    assert!(shown.iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })), "the welcome acted on after the read");
}

/// R-CLIENT-1: the phone is relay only. A welcome naming direct paths
/// makes the core ask for a race, which the viewer loses at once: no
/// effect for Swift, and the next wake is in the future, not a busy loop.
#[test]
fn r_client_1_a_race_for_a_direct_path_is_lost_at_once_on_the_phone() {
    let mut p = Phone::new();
    p.start();
    p.join(true, 0);
    p.welcome(Welcome { candidates: vec![Candidate { url: "ws://100.64.0.1:7791".into(), via: "tailscale".into() }], ..Welcome::default() });
    p.take_rest();
    assert!(p.viewer.next_wake_ms().unwrap() < 10_000, "a race is due");
    for at in [10_000, 30_000, 70_000] {
        p.at(at);
        // The relay's pong: the link lives.
        assert!(p.viewer.received(p.conn, WsFrame::Text { text: "pong".into() }, at).is_empty());
        let e = p.viewer.tick(at);
        p.take(e);
        assert!(p.take_rest().iter().all(|e| matches!(e, Effect::Ping)), "nothing Swift must race");
        let next = p.viewer.next_wake_ms().unwrap();
        assert!(next > at, "the next race backed off, not due at once: {next} at {at}");
    }
}

/// R-CLIENT-1: a join the relay leaves unanswered — the connection open,
/// no challenge — fails after ten seconds, said, and is asked for again;
/// `next_wake_ms` says when, so Swift needs no timer of its own for it.
#[test]
fn r_client_1_a_join_the_relay_never_answers_fails_and_is_tried_again() {
    let mut p = Phone::new();
    p.start();
    p.take_rest();
    p.ticket("t");
    assert!(matches!(p.take_rest().as_slice(), [Effect::Dial { .. }]));
    assert_eq!(p.viewer.next_wake_ms(), Some(10_000), "the join's wait");
    p.at(9_999);
    let e = p.viewer.tick(p.now);
    p.take(e);
    assert!(p.take_rest().is_empty());
    p.at(10_000);
    let e = p.viewer.tick(p.now);
    p.take(e);
    // Tried again on the supervisor's backoff: its first wait, 250 ms cut
    // by up to half.
    assert!(matches!(p.take_rest().as_slice(), [Effect::Close, Effect::NeedTicket { at_ms: 10_125..=10_250 }]), "dropped, and tried again a moment later");
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Note { text } if text.contains("did not answer the join"))));

    // Each message of the join gives the relay ten seconds more.
    p.at(11_000);
    p.ticket("t");
    p.take_rest();
    p.at(15_000);
    p.challenge();
    let e = p.viewer.tick(20_000);
    p.take(e);
    assert!(p.take_rest().is_empty(), "not yet");
    assert_eq!(p.viewer.next_wake_ms(), Some(25_000));
}

/// R-CLIENT-1: a live connection silent for longer than the heartbeat
/// allows is dropped (`Close`) and joined again.
#[test]
fn r_client_1_a_silent_connection_is_dropped_and_joined_again() {
    let mut p = Phone::live();
    for at in [10_016, 20_032] {
        let e = p.viewer.tick(at);
        p.take(e);
    }
    let rest = p.take_rest();
    assert!(rest.iter().all(|e| matches!(e, Effect::Ping)), "{rest:?}");
    let e = p.viewer.tick(40_064);
    p.take(e);
    assert!(matches!(p.take_rest().as_slice(), [Effect::Close, Effect::NeedTicket { .. }]), "silent for over 30 s: dropped");
}

/// R-CLIENT-1: what the old connection still says after `Close` — a
/// message or its close, queued in Swift — is ignored: the new join goes
/// through, and the session after it is undisturbed.
#[test]
fn r_client_1_inputs_from_a_dropped_connection_are_ignored() {
    let mut p = Phone::live();
    let old = p.conn;
    p.relay(json!({"type": "host", "present": true}));
    assert!(matches!(p.take_rest().as_slice(), [Effect::Close, Effect::NeedTicket { .. }]));
    p.rest.push(Effect::NeedTicket { at_ms: 0 });
    p.ticket("t");
    assert!(p.conn > old, "a new connection");
    p.take_rest();
    let stale = control(&json!({"type": "host", "present": false}));
    assert!(p.viewer.received(old, WsFrame::Binary { bytes: stale }, p.now).is_empty(), "the old one's message: ignored");
    assert!(p.viewer.closed(old, p.now).is_empty(), "the old one's close: ignored");
    p.challenge();
    p.joined(true, 0);
    p.welcome(Welcome::default());
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })));
    assert!(p.viewer.closed(old, p.now).is_empty(), "still ignored once live");
    assert!(p.take_rest().is_empty());
}

/// R-CLIENT-1: a `ChunksRead` is answered only by the viewer that read
/// it: another viewer's, at the same `ReadChunks` of its own, handed in by
/// mistake, is dropped and this viewer's own read still awaited.
#[test]
fn r_client_1_a_chunks_read_from_another_viewer_is_dropped() {
    let welcomed = || {
        let mut p = Phone::new();
        p.start();
        p.shown.clear();
        p.join(true, 0);
        p.welcome_with(Welcome::default());
        assert!(matches!(p.take_rest().as_slice(), [Effect::ReadChunks]));
        p
    };
    let (mut p, other) = (welcomed(), welcomed());
    let theirs = other.viewer.read_chunks();
    assert!(p.viewer.chunks_read(theirs, p.now).is_empty(), "another viewer's read");
    assert!(!p.frame().iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })), "still waiting for its own");
    let mine = p.viewer.read_chunks();
    let e = p.viewer.chunks_read(mine, p.now);
    p.take(e);
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })));
}

/// R-PROTO-2: an event at rest this build cannot decode — a newer host's —
/// is left out of `CaughtUp` and said in a `Failed` with no `End`: the
/// events around it still show, and the viewer goes on.
#[test]
fn r_proto_2_an_event_that_does_not_decode_is_left_out_and_the_viewer_goes_on() {
    let mut p = Phone::new();
    p.start();
    p.shown.clear();
    p.join(true, 0);
    let newer = json!({"id": "01a0ec7b-2222-7000-8000-0000000000aa", "sessionId": SESSION, "timeMs": 9, "type": "turn.teleported"});
    p.hosted.write(&[newer, event(SESSION, 10, "known")]);
    p.welcome(Welcome::default());
    let shown = p.frame();
    let at = shown.iter().position(|u| matches!(u, ViewerUpdate::CaughtUp { events } if events.len() == 1)).expect("the event that decodes");
    assert!(matches!(&shown[at + 1], ViewerUpdate::Failed { detail } if detail.contains("0000000000aa")), "{shown:?}");
    assert!(shown.iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })), "and goes on");
    assert!(!p.rest.iter().any(|e| matches!(e, Effect::End)));
}

/// R-CLIENT-1: a relay address a join cannot sign over is refused as the
/// viewer opens, not found a ticket at a time.
#[test]
fn r_client_1_a_viewer_on_a_relay_with_no_origin_is_invalid_input() {
    let w = World::new();
    let _host = w.host_session(SESSION, "t", &[event(SESSION, 1, "before")], &[]);
    let opened = w.phone.open_session(SESSION.into(), None, Some("relay.example".into())).unwrap();
    assert!(matches!(Viewer::open(opened, 0), Err(krowk_ffi::KrowkError::InvalidInput { .. })));
}

// ------------------------------------------------- the supervisor (ticket 86)

impl Phone {
    fn wake(&mut self) {
        let e = self.viewer.wake(self.now);
        self.take(e);
    }

    fn background(&mut self) {
        let e = self.viewer.background(self.now);
        self.take(e);
    }

    fn foreground(&mut self) {
        let e = self.viewer.foreground(self.now);
        self.take(e);
    }

    fn tick(&mut self) {
        let e = self.viewer.tick(self.now);
        self.take(e);
    }

    fn pong(&mut self) {
        assert!(self.viewer.received(self.conn, WsFrame::Text { text: "pong".into() }, self.now).is_empty());
    }
}

/// R-OFF-2: a lost connection is joined again on the supervisor's backoff,
/// 250 ms then doubling, each cut by up to half, never the core's fixed
/// second; the note about it no longer says "every second". A wake (a
/// usable network path, the scene active, pull-to-refresh) joins at once.
#[test]
fn r_off_2_a_lost_connection_backs_off_and_a_wake_joins_it_at_once() {
    let mut p = Phone::live();
    p.at(5_000);
    p.closed();
    match p.take_rest().as_slice() {
        [Effect::NeedTicket { at_ms }] => assert!((5_125..=5_250).contains(at_ms), "{at_ms}"),
        other => panic!("{other:?}"),
    }
    // Its join fails, and the next: 500 ms, then 1 s, each cut.
    for (at, full) in [(6_000u64, 500u64), (8_000, 1_000)] {
        p.at(at);
        p.ticket("t");
        p.take_rest();
        p.closed();
        match p.take_rest().as_slice() {
            [Effect::NeedTicket { at_ms }] => assert!((at + full / 2..=at + full).contains(at_ms), "{full}: {at_ms}"),
            other => panic!("{other:?}"),
        }
    }
    let notes: Vec<String> = p.frame().into_iter().filter_map(|u| if let ViewerUpdate::Note { text } = u { Some(text) } else { None }).collect();
    assert!(!notes.is_empty() && notes.iter().all(|n| n.contains(RELAY) && !n.contains("every second")), "{notes:?}");

    // A wake: its ticket now, no backoff.
    p.at(8_100);
    p.wake();
    assert!(matches!(p.rest.as_slice(), [Effect::NeedTicket { at_ms: 8_100 }]), "{:?}", p.rest);
    // Asked for already: a second wake waits for it.
    p.wake();
    assert_eq!(p.rest.len(), 1, "{:?}", p.rest);
    p.join(true, 0);
    p.welcome(Welcome::default());
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })));
}

/// R-LAG-9: a wake on a live connection sends `ping`; a `pong` within 5 s
/// keeps it; with none, the next tick past 5 s replaces it, joining again
/// at once with no backoff, at the core's cursor. `next_wake_ms` says when,
/// so Swift needs no timer of its own.
#[test]
fn r_lag_9_a_wake_probes_a_live_connection_and_replaces_one_that_never_answers() {
    let mut p = Phone::live();
    let b = p.batch(&[event(SESSION, 3, "three")]);
    p.receive(&b);
    assert_eq!(line_runs(&p.frame()), ["three"]);

    p.at(2_000);
    p.wake();
    assert!(matches!(p.take_rest().as_slice(), [Effect::Ping]));
    assert!(p.viewer.next_wake_ms().unwrap() <= 7_000, "the probe's deadline");
    p.at(3_000);
    p.pong();
    p.at(7_100);
    p.tick();
    assert!(p.take_rest().iter().all(|e| matches!(e, Effect::Ping)), "answered: kept");

    p.wake();
    assert!(matches!(p.take_rest().as_slice(), [Effect::Ping]));
    p.at(12_099);
    p.tick();
    assert!(p.take_rest().iter().all(|e| matches!(e, Effect::Ping)), "not yet");
    p.at(12_100);
    p.tick();
    assert!(matches!(p.rest.as_slice(), [.., Effect::Close, Effect::NeedTicket { at_ms: 12_100 }]), "unanswered: replaced at once: {:?}", p.rest);
    // At the cursor: the stream and the batch it applied.
    let join = p.join(true, 1);
    assert_eq!(join["afterSeq"], 1, "{join}");
    assert!(join["stream"].is_string(), "{join}");
}

/// R-IOS-2: in the background a live connection is kept and never probed,
/// a lost one is not tried again; at the foreground it is joined at once.
#[test]
fn r_ios_2_a_connection_lost_in_the_background_waits_for_the_foreground() {
    let mut p = Phone::live();
    p.at(1_000);
    p.background();
    p.wake();
    assert!(p.take_rest().is_empty(), "no probe in the background");
    p.at(4_000);
    p.closed();
    assert!(p.take_rest().is_empty(), "lost in the background: not tried again");
    p.at(60_000);
    p.tick();
    assert!(p.take_rest().iter().all(|e| !matches!(e, Effect::NeedTicket { .. })), "nor later");
    p.at(61_000);
    p.foreground();
    assert!(matches!(p.rest.as_slice(), [Effect::NeedTicket { at_ms: 61_000 }]), "{:?}", p.rest);
    p.join(true, 0);
    p.welcome(Welcome::default());
    assert!(p.frame().iter().any(|u| matches!(u, ViewerUpdate::Host { present: true })));
}

/// R-IOS-2: back after 10 s or more away, a connection still up is
/// replaced at once rather than probed (iOS has likely dropped it); after
/// less, it is probed.
#[test]
fn r_ios_2_ten_seconds_away_replaces_the_connection_on_the_foreground() {
    let mut p = Phone::live();
    p.at(1_000);
    p.background();
    p.at(11_000);
    p.foreground();
    assert!(matches!(p.take_rest().as_slice(), [Effect::Close, Effect::NeedTicket { at_ms: 11_000 }]));

    let mut p = Phone::live();
    p.at(1_000);
    p.background();
    p.at(10_999);
    p.foreground();
    assert!(matches!(p.take_rest().as_slice(), [Effect::Ping]));
}
