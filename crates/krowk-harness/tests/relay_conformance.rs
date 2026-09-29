//! The relay conformance suite (R-RELAY-1, R-LAG-6): Canon's
//! `engineering/relay.md`, checked black-box over WebSocket. Nothing here
//! reaches into a relay's code: every test dials a channel, answers the
//! challenge and watches what comes back, so the same suite holds any
//! implementation of the contract to it — the reference relay
//! (`krowk relay serve`, `krowk_harness::relay`) by default, ticket 18's
//! Durable Object under `wrangler dev` with
//!
//! ```sh
//! KROWK_RELAY_URL=ws://127.0.0.1:8787 cargo test -p krowk-harness --test relay_conformance
//! ```
//!
//! once that relay trusts `tests/fixtures/relay/roster.json` — the
//! devices' signing keys, their workspaces and the sessions' leases. The
//! fixture's seeds are test keys, published on purpose; nothing real is
//! ever signed with them. Each test has its own devices and session, so the
//! tests run at once and the per-device join ceiling never binds.
//!
//! What the relay carries is opaque to it, so payloads here are random
//! bytes marked sealed (`enc` = 1): a relay that tried to open them would
//! fail, which is the point.
#![cfg(unix)]

use futures_util::{SinkExt, StreamExt};
use krowk_client::e2e::{self, DeviceId, SigningKey, RELAY_ROLE_HOST, RELAY_ROLE_VIEWER};
use krowk_harness::daemon::ws::{Envelope, ENC_NONE, ENC_XCHACHA20_POLY1305, KIND_ACK, KIND_BATCH, KIND_FRAME, KIND_RELAY, KIND_ROUTED};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const FIXTURE: &str = include_str!("fixtures/relay/roster.json");
const FIXTURE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/relay/roster.json");

/// Every test, its devices and its session: `<test>-host` holds the lease
/// of session `<test>`, `<test>-viewer` and `<test>-viewer2` watch it, all
/// in workspace A.
const TESTS: &[&str] = &["auth", "fanout", "plaintext", "resume", "beyond", "behind", "absent", "heartbeat", "window", "order", "replace", "stream", "rate", "lockout", "size", "prejoin", "crowd", "idle"];

/// How long anything the relay should answer may take, and how long to
/// wait to be sure nothing comes.
const ANSWER: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(300);

fn seed(name: &str) -> [u8; 32] {
    Sha256::new().chain_update(b"krowk/relay-conformance/v1/").chain_update(name.as_bytes()).finalize().into()
}

fn device_id(name: &str) -> DeviceId {
    DeviceId(seed(&format!("id/{name}"))[..16].try_into().unwrap())
}

fn session_id(test: &str) -> String {
    let h = seed(&format!("session/{test}"));
    let mut b: [u8; 16] = h[..16].try_into().unwrap();
    // A UUIDv7's version and variant bits, so it reads as the ids krowk mints.
    b[6] = 0x70 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let x = e2e::hex(&b);
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..])
}

/// The lease's fence: any number, so a relay cannot assume they start low.
fn fence(test: &str) -> u64 {
    u64::from(seed(&format!("fence/{test}"))[0]) + 3
}

fn device(name: &str, workspace: &str, revoked: bool) -> Value {
    let key = SigningKey::from_secret(&seed(name)).unwrap();
    json!({"name": name, "id": device_id(name).to_string(), "seed": e2e::hex(&seed(name)), "signingKey": e2e::hex(&key.public().0), "workspace": workspace, "revoked": revoked})
}

/// The fixture, generated: what `roster.json` must hold.
fn fixture() -> Value {
    let mut devices = Vec::new();
    let mut sessions = Vec::new();
    for t in TESTS {
        for role in ["host", "viewer", "viewer2"] {
            devices.push(device(&format!("{t}-{role}"), "ws_conformance_a", false));
        }
        sessions.push(json!({"name": t, "id": session_id(t), "workspace": "ws_conformance_a", "holder": device_id(&format!("{t}-host")).to_string(), "fence": fence(t)}));
    }
    devices.push(device("outsider", "ws_conformance_b", false));
    devices.push(device("revoked", "ws_conformance_a", true));
    let stranger = device("stranger", "ws_conformance_a", false);
    json!({
        "about": "The relay conformance suite's test devices (crates/krowk-harness/tests/relay_conformance.rs). The seeds are published test keys: never trust this roster outside a test.",
        "devices": devices,
        "sessions": sessions,
        "unregistered": [stranger],
    })
}

/// The checked-in fixture is the generated one; `KROWK_FIXTURE_UPDATE=1`
/// writes it. Another relay loads the file, so it must not drift.
#[test]
fn the_checked_in_roster_is_the_generated_one() {
    let want = serde_json::to_string_pretty(&fixture()).unwrap() + "\n";
    if std::env::var_os("KROWK_FIXTURE_UPDATE").is_some() {
        std::fs::create_dir_all(std::path::Path::new(FIXTURE_PATH).parent().unwrap()).unwrap();
        std::fs::write(FIXTURE_PATH, &want).unwrap();
        return;
    }
    assert_eq!(FIXTURE, want, "run KROWK_FIXTURE_UPDATE=1 cargo test -p krowk-harness --test relay_conformance");
}

/// The relay under test: `KROWK_RELAY_URL`, or the reference relay started
/// in this process on a loopback port with the fixture — the same
/// `relay::run` that `krowk relay serve --roster` runs.
fn relay_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        if let Ok(url) = std::env::var("KROWK_RELAY_URL") {
            return url.trim_end_matches('/').to_string();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let roster = krowk_harness::relay::Roster::parse(FIXTURE).unwrap();
        std::thread::spawn(move || krowk_harness::relay::run(listener, krowk_harness::relay::Config { roster, origin: None, limits: Default::default() }));
        url
    })
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Conn {
    ws: Ws,
    session: [u8; 16],
}

/// What one message from the relay was.
#[derive(Debug)]
enum In {
    Control(Value),
    Env(Envelope),
    Text(String),
    Pong,
    Closed(Option<String>),
}

impl Conn {
    async fn recv(&mut self, wait: Duration) -> Option<In> {
        loop {
            let m = match tokio::time::timeout(wait, self.ws.next()).await {
                Err(_) => return None,
                Ok(None) | Ok(Some(Err(_))) => return Some(In::Closed(None)),
                Ok(Some(Ok(m))) => m,
            };
            return Some(match m {
                Message::Binary(b) => {
                    let e = Envelope::decode(&b).expect("the relay sends envelopes");
                    if e.kind == KIND_RELAY {
                        In::Control(serde_json::from_slice(&e.payload).expect("control is JSON"))
                    } else {
                        In::Env(e)
                    }
                }
                Message::Text(t) => In::Text(t.to_string()),
                Message::Pong(_) => In::Pong,
                Message::Close(f) => In::Closed(f.map(|f| f.reason.to_string())),
                // The relay's own heartbeat: answered by tungstenite.
                Message::Ping(_) | Message::Frame(_) => continue,
            });
        }
    }

    /// The next message that is not a relay notice about who is present.
    async fn next(&mut self) -> In {
        loop {
            match self.recv(ANSWER).await.expect("the relay answers in time") {
                In::Control(v) if v["type"] == "viewer" || v["type"] == "host" => continue,
                other => return other,
            }
        }
    }

    async fn control(&mut self) -> Value {
        match self.next().await {
            In::Control(v) => v,
            other => panic!("expected a control message, got {other:?}"),
        }
    }

    /// The next control message of `kind`, skipping the others.
    async fn expect(&mut self, kind: &str) -> Value {
        loop {
            match self.recv(ANSWER).await.unwrap_or_else(|| panic!("no {kind} arrived")) {
                In::Control(v) if v["type"] == kind => return v,
                In::Closed(r) => panic!("closed ({r:?}) before a {kind}"),
                _ => continue,
            }
        }
    }

    /// An error of `code`, then the connection closes with it as the reason.
    async fn refused(&mut self, code: &str) {
        let e = self.expect("error").await;
        assert_eq!(e["code"], code, "{e}");
        assert!(e["fix"].as_str().is_some_and(|f| !f.is_empty()), "every refusal says what to do: {e}");
        loop {
            match self.recv(ANSWER).await {
                Some(In::Closed(reason)) => {
                    if let Some(r) = reason.filter(|r| !r.is_empty()) {
                        assert_eq!(r, code);
                    }
                    return;
                }
                None => panic!("still open after refusing {code}"),
                _ => continue,
            }
        }
    }

    /// Nothing but heartbeats and presence notices for a while.
    async fn quiet(&mut self) {
        let until = Instant::now() + QUIET;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.recv(left).await {
                None => return,
                Some(In::Control(v)) if v["type"] == "viewer" || v["type"] == "host" => {}
                Some(other) => panic!("expected nothing, got {other:?}"),
            }
        }
    }

    async fn send(&mut self, e: Envelope) {
        self.ws.send(Message::Binary(e.encode().into())).await.expect("sent");
    }

    async fn send_control(&mut self, v: Value) {
        self.ws.send(Message::Binary(control(&v).into())).await.expect("sent");
    }

    fn sealed(&self, kind: u8, seq: u64, payload: &[u8]) -> Envelope {
        Envelope { kind, flags: 0, enc: ENC_XCHACHA20_POLY1305, session: self.session, seq, payload: payload.to_vec() }
    }

    async fn batch(&mut self, seq: u64) {
        let p = payload(seq);
        let e = self.sealed(KIND_BATCH, seq, &p);
        self.send(e).await;
    }

    /// Waits for the relay's ack of the host's batches up to `seq` (the
    /// relay acknowledges every eighth).
    async fn acked(&mut self, seq: u64) {
        loop {
            match self.next().await {
                In::Env(e) if e.kind == KIND_ACK && e.seq >= seq => return,
                In::Env(e) if e.kind == KIND_ACK => continue,
                other => panic!("expected the relay's ack of {seq}, got {other:?}"),
            }
        }
    }

    async fn ack(&mut self, seq: u64) {
        let e = Envelope { kind: KIND_ACK, flags: 0, enc: ENC_NONE, session: self.session, seq, payload: Vec::new() };
        self.send(e).await;
    }

    /// The next batch, which must be `seq` with its payload whole.
    async fn batch_in(&mut self, seq: u64) {
        match self.next().await {
            In::Env(e) => {
                assert_eq!((e.kind, e.seq, e.enc), (KIND_BATCH, seq, ENC_XCHACHA20_POLY1305));
                assert_eq!(e.payload, payload(seq), "batch {seq} arrived as it was sent");
            }
            other => panic!("expected batch {seq}, got {other:?}"),
        }
    }
}

fn control(v: &Value) -> Vec<u8> {
    Envelope { kind: KIND_RELAY, flags: 0, enc: ENC_NONE, session: [0; 16], seq: 0, payload: serde_json::to_vec(v).unwrap() }.encode()
}

/// A batch's stand-in ciphertext: distinct per seq, so a batch served in
/// another's place shows.
fn payload(seq: u64) -> Vec<u8> {
    let mut p = seq.to_be_bytes().to_vec();
    p.extend_from_slice(&seed(&format!("payload/{seq}")));
    p
}

fn origin() -> String {
    let url = relay_url();
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    format!("{}://{authority}", url.split("://").next().unwrap_or("ws"))
}

/// Dials `test`'s channel and reads the challenge.
async fn dial(test: &str) -> (Conn, Value) {
    let sid = session_id(test);
    let (ws, _) = tokio_tungstenite::connect_async(format!("{}/v1/relay/{sid}", relay_url())).await.expect("the relay accepts a WebSocket");
    let session = krowk_harness::daemon::ws::uuid(&sid);
    let mut c = Conn { ws, session };
    let challenge = c.control().await;
    assert_eq!(challenge["type"], "challenge", "{challenge}");
    assert_eq!(challenge["version"], 1);
    // `challenge.relay` is informational (relay.md): a client never signs
    // it, so the suite holds a relay to nothing about it.
    (c, challenge)
}

struct As<'a> {
    name: &'a str,
    role: u8,
    fence: Option<u64>,
    stream: Option<[u8; 16]>,
    after: Option<u64>,
    /// Sign with another device's key, or for another origin.
    key_of: Option<&'a str>,
    origin: Option<&'a str>,
}

fn host(test: &str) -> As<'static> {
    let name: &'static str = Box::leak(format!("{test}-host").into_boxed_str());
    As { name, role: RELAY_ROLE_HOST, fence: Some(fence(test)), stream: Some(stream_id(test, 1)), after: None, key_of: None, origin: None }
}

fn viewer(test: &str, which: &str) -> As<'static> {
    let name: &'static str = Box::leak(format!("{test}-{which}").into_boxed_str());
    As { name, role: RELAY_ROLE_VIEWER, fence: None, stream: None, after: None, key_of: None, origin: None }
}

fn stream_id(test: &str, n: u8) -> [u8; 16] {
    seed(&format!("stream/{test}/{n}"))[..16].try_into().unwrap()
}

async fn join_as(test: &str, a: &As<'_>) -> (Conn, Value) {
    let (mut c, challenge) = dial(test).await;
    let nonce: [u8; 32] = e2e::unhex(challenge["nonce"].as_str().unwrap()).unwrap().try_into().unwrap();
    let key = SigningKey::from_secret(&seed(a.key_of.unwrap_or(a.name))).unwrap();
    let dialed = origin();
    let sig = key.sign_relay_join(a.role, &c.session, &nonce, &device_id(a.name), a.origin.unwrap_or(&dialed)).unwrap();
    let mut join = json!({"type": "join", "role": if a.role == RELAY_ROLE_HOST { "host" } else { "viewer" }, "device": device_id(a.name).to_string(), "signature": e2e::hex(&sig)});
    if let Some(f) = a.fence {
        join["fence"] = f.into();
    }
    if let Some(s) = a.stream {
        join["stream"] = e2e::hex(&s).into();
    }
    if let Some(n) = a.after {
        join["afterSeq"] = n.into();
    }
    c.send_control(join).await;
    let answer = c.control().await;
    (c, answer)
}

async fn joined(test: &str, a: &As<'_>) -> (Conn, Value) {
    let (c, answer) = join_as(test, a).await;
    assert_eq!(answer["type"], "joined", "{} joins {test}: {answer}", a.name);
    (c, answer)
}

async fn refused_join(test: &str, a: &As<'_>, code: &str) {
    let (mut c, answer) = join_as(test, a).await;
    assert_eq!(answer["type"], "error", "{} must not join {test}: {answer}", a.name);
    assert_eq!(answer["code"], code, "{answer}");
    assert!(answer["fix"].as_str().is_some_and(|f| !f.is_empty()), "{answer}");
    match c.recv(ANSWER).await {
        Some(In::Closed(_)) => {}
        other => panic!("a refused join is closed, got {other:?}"),
    }
}

/// R-RELAY-1: a device the relay does not trust is refused; so is a
/// signature by another key, or for another relay's origin, or another
/// role's; a viewer cannot pose as the host, nor the holder of a lease
/// since taken over (an older fence); a device of another workspace, or a revoked one, cannot
/// join at all. None of them is told anything about the session.
#[tokio::test]
async fn r_relay_1_only_a_trusted_device_with_its_own_signature_joins_and_only_the_lease_holder_hosts() {
    let t = "auth";
    refused_join(t, &As { name: "stranger", ..viewer(t, "viewer") }, "unknown_device").await;
    refused_join(t, &As { key_of: Some("auth-viewer2"), ..viewer(t, "viewer") }, "bad_signature").await;
    refused_join(t, &As { origin: Some("wss://another-relay.example"), ..viewer(t, "viewer") }, "bad_signature").await;
    // A viewer's signature presented as a host's does not verify either:
    // the role is signed.
    {
        let (mut c, challenge) = dial(t).await;
        let nonce: [u8; 32] = e2e::unhex(challenge["nonce"].as_str().unwrap()).unwrap().try_into().unwrap();
        let key = SigningKey::from_secret(&seed("auth-host")).unwrap();
        let sig = key.sign_relay_join(RELAY_ROLE_VIEWER, &c.session, &nonce, &device_id("auth-host"), &origin()).unwrap();
        c.send_control(json!({"type": "join", "role": "host", "device": device_id("auth-host").to_string(), "signature": e2e::hex(&sig), "fence": fence(t), "stream": e2e::hex(&stream_id(t, 1))})).await;
        c.refused("bad_signature").await;
    }
    // A viewer asking to host, knowing the lease's fence (it is public).
    refused_join(t, &As { role: RELAY_ROLE_HOST, fence: Some(fence(t)), stream: Some(stream_id(t, 1)), ..viewer(t, "viewer") }, "not_lease_holder").await;
    // The holder of an older lease — another device took it since — or
    // one naming no fence.
    refused_join(t, &As { fence: Some(fence(t) - 1), ..host(t) }, "stale_lease").await;
    refused_join(t, &As { fence: None, ..host(t) }, "stale_lease").await;
    // Another workspace's session reads as none at all.
    refused_join(t, &As { name: "outsider", ..viewer(t, "viewer") }, "unknown_session").await;
    // A session the relay does not know, asked by a device it does.
    refused_join("nowhere", &viewer(t, "viewer"), "unknown_session").await;
    // Anything but a join first, and a join missing what it needs.
    {
        let (mut c, _) = dial(t).await;
        let e = c.sealed(KIND_FRAME, 0, b"sealed");
        c.send(e).await;
        c.refused("not_joined").await;
        let (mut c, _) = dial(t).await;
        c.send_control(json!({"type": "join"})).await;
        c.refused("bad_join").await;
        let (mut c, _) = dial(t).await;
        c.ws.send(Message::Text("hello".into())).await.unwrap();
        c.refused("not_joined").await;
    }
    refused_join(t, &As { name: "revoked", ..viewer(t, "viewer") }, "device_revoked").await;
    // A signature answers one challenge: the nonce is fresh each time.
    let (_a, one) = dial(t).await;
    let (_b, two) = dial(t).await;
    assert_ne!(one["nonce"], two["nonce"]);
    // And the right ones join.
    let (_h, j) = joined(t, &host(t)).await;
    assert_eq!(j["role"], "host");
    let (_v, j) = joined(t, &viewer(t, "viewer")).await;
    assert_eq!((j["role"].as_str(), j["host"].as_bool()), (Some("viewer"), Some(true)));
}

/// R-RELAY-1: the host's batches reach every viewer, whole and in order; a
/// viewer's frames reach the host alone, routed with the viewer's link;
/// what the host routes to one viewer reaches that viewer alone.
#[tokio::test]
async fn r_relay_1_host_batches_fan_out_and_viewer_frames_reach_only_the_host() {
    let t = "fanout";
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v1, j1) = joined(t, &viewer(t, "viewer")).await;
    let (mut v2, j2) = joined(t, &viewer(t, "viewer2")).await;
    assert_ne!(j1["link"], j2["link"]);
    let notice = h.expect("viewer").await;
    assert_eq!((notice["event"].as_str(), notice["link"].clone()), (Some("joined"), j1["link"].clone()));
    assert_eq!(notice["device"], device_id("fanout-viewer").to_string());
    for seq in 1..=5 {
        h.batch(seq).await;
    }
    for seq in 1..=5 {
        v1.batch_in(seq).await;
        v2.batch_in(seq).await;
    }
    // A viewer's frame: to the host, routed, and to nobody else.
    let frame = v1.sealed(KIND_FRAME, 0, b"sealed prompt");
    v1.send(frame.clone()).await;
    let routed = loop {
        match h.next().await {
            In::Env(e) if e.kind == KIND_ROUTED => break e,
            In::Env(e) if e.kind == KIND_ACK => continue,
            other => panic!("expected the routed frame, got {other:?}"),
        }
    };
    assert_eq!(Some(routed.seq), j1["link"].as_u64(), "routed with the sender's link");
    assert_eq!(routed.session, h.session);
    assert_eq!(Envelope::decode(&routed.payload).unwrap(), frame, "the viewer's envelope, byte for byte");
    v2.quiet().await;
    // The host's answer to that viewer alone.
    let reply = h.sealed(KIND_BATCH, 0, b"sealed welcome");
    let wrapped = Envelope { kind: KIND_ROUTED, flags: 0, enc: ENC_XCHACHA20_POLY1305, session: h.session, seq: j1["link"].as_u64().unwrap(), payload: reply.encode() };
    h.send(wrapped).await;
    match v1.next().await {
        In::Env(e) => assert_eq!(e, reply),
        other => panic!("expected the routed reply, got {other:?}"),
    }
    v2.quiet().await;
    // A viewer leaving is said to the host.
    drop(v2);
    let left = h.expect("viewer").await;
    assert_eq!((left["event"].as_str(), left["link"].clone()), (Some("left"), j2["link"].clone()));
}

/// R-RELAY-1: the relay carries ciphertext only. A plaintext batch from the
/// host (`enc` 0) is refused and the host's connection closed, and it is
/// never stored: a viewer resuming from before it is served nothing. A
/// plaintext frame from a viewer never reaches the host, nor does a routed
/// envelope with a plaintext one inside.
#[tokio::test]
async fn r_relay_1_the_relay_refuses_and_never_holds_a_plaintext_frame() {
    let t = "plaintext";
    let stream = stream_id(t, 1);
    let (mut h, _) = joined(t, &host(t)).await;
    h.send(Envelope { kind: KIND_BATCH, flags: 0, enc: ENC_NONE, session: h.session, seq: 1, payload: b"{\"type\":\"line\"}\n".to_vec() }).await;
    h.refused("plaintext").await;
    let (mut v, j) = joined(t, &As { stream: Some(stream), after: Some(0), ..viewer(t, "viewer") }).await;
    assert_eq!(j["seq"], 0, "nothing was stored: {j}");
    v.quiet().await;
    // A viewer's plaintext frame: refused, and the host sees nothing of it.
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v2, _) = joined(t, &viewer(t, "viewer2")).await;
    h.expect("viewer").await;
    v2.send(Envelope { kind: KIND_FRAME, flags: 0, enc: ENC_NONE, session: v2.session, seq: 0, payload: b"{\"type\":\"hello\"}".to_vec() }).await;
    v2.refused("plaintext").await;
    loop {
        match h.recv(QUIET).await {
            None => break,
            Some(In::Control(c)) if c["type"] == "viewer" => continue,
            Some(other) => panic!("the host must see nothing of a plaintext frame, got {other:?}"),
        }
    }
    // Nor a plaintext envelope routed inside a sealed one.
    let inner = Envelope { kind: KIND_BATCH, flags: 0, enc: ENC_NONE, session: h.session, seq: 0, payload: b"{}".to_vec() };
    let link = j["link"].as_u64().unwrap();
    h.send(Envelope { kind: KIND_ROUTED, flags: 0, enc: ENC_XCHACHA20_POLY1305, session: h.session, seq: link, payload: inner.encode() }).await;
    h.refused("plaintext").await;
    v.quiet().await;
}

/// R-LAG-6: a viewer that goes away and comes back with the stream and the
/// last `seq` it applied is served every batch after it from the ring
/// buffer — none lost, none twice — with the host silent throughout.
#[tokio::test]
async fn r_lag_6_a_viewer_resumes_from_a_seq_without_losing_a_frame_in_the_ring_buffer() {
    let t = "resume";
    let stream = stream_id(t, 1);
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v, j) = joined(t, &viewer(t, "viewer")).await;
    assert_eq!(j["stream"], e2e::hex(&stream), "the viewer is told the host's stream");
    for seq in 1..=10 {
        h.batch(seq).await;
    }
    for seq in 1..=10 {
        v.batch_in(seq).await;
    }
    v.ack(7).await;
    drop(v);
    // Sent while it was away; the relay holds them all once it has
    // acknowledged the last — over a network nothing else orders the
    // host's batches before the viewer's join.
    for seq in 11..=48 {
        h.batch(seq).await;
    }
    h.acked(48).await;
    let (mut v, j) = joined(t, &As { stream: Some(stream), after: Some(7), ..viewer(t, "viewer") }).await;
    assert_eq!(j["seq"], 48, "{j}");
    for seq in 8..=23 {
        v.batch_in(seq).await;
    }
    // The window: sixteen in flight until it acknowledges.
    v.quiet().await;
    v.ack(23).await;
    for seq in 24..=39 {
        v.batch_in(seq).await;
    }
    v.ack(39).await;
    for seq in 40..=48 {
        v.batch_in(seq).await;
    }
    v.ack(48).await;
    // Then live.
    for seq in 49..=56 {
        h.batch(seq).await;
    }
    h.acked(56).await;
    for seq in 49..=56 {
        v.batch_in(seq).await;
    }
    // A viewer already up to date is sent nothing again.
    let (mut v2, _) = joined(t, &As { stream: Some(stream), after: Some(56), ..viewer(t, "viewer2") }).await;
    v2.quiet().await;
}

/// R-LAG-6: a cursor the ring buffer no longer covers — older than its
/// oldest batch, of another stream, or ahead of the relay — is told to
/// catch up from the host, and then follows live.
#[tokio::test]
async fn r_lag_6_a_viewer_the_ring_buffer_cannot_serve_is_told_to_resync() {
    let t = "beyond";
    let stream = stream_id(t, 1);
    let (mut h, _) = joined(t, &host(t)).await;
    // Past the contract's 1024 batches.
    for seq in 1..=1100 {
        h.batch(seq).await;
    }
    h.acked(1096).await;
    let (mut v, _) = joined(t, &As { stream: Some(stream), after: Some(3), ..viewer(t, "viewer") }).await;
    let r = v.control().await;
    assert_eq!((r["type"].as_str(), r["reason"].as_str()), (Some("resync"), Some("beyond_buffer")), "{r}");
    assert_eq!(r["seq"], 1100);
    h.batch(1101).await;
    v.batch_in(1101).await;
    // Inside the buffer, the same cursor space serves.
    let (mut v2, _) = joined(t, &As { stream: Some(stream), after: Some(1090), ..viewer(t, "viewer2") }).await;
    for seq in 1091..=1101 {
        v2.batch_in(seq).await;
    }
    drop(v2);
    let (mut v2, _) = joined(t, &As { stream: Some(stream_id(t, 9)), after: Some(1090), ..viewer(t, "viewer2") }).await;
    assert_eq!(v2.control().await["reason"], "stream");
    drop(v2);
    let (mut v2, _) = joined(t, &As { stream: Some(stream), after: Some(5000), ..viewer(t, "viewer2") }).await;
    assert_eq!(v2.control().await["reason"], "ahead");
}

/// R-LAG-6: the relay answers heartbeats itself — a WebSocket ping and the
/// text `ping` a browser or a Worker can send — at once, whether the host
/// is connected and silent or not connected at all.
#[tokio::test]
async fn r_lag_6_heartbeats_are_answered_while_the_host_is_silent() {
    let t = "heartbeat";
    let (mut v, j) = joined(t, &viewer(t, "viewer")).await;
    assert_eq!(j["host"], false);
    let (_h, _) = joined(t, &host(t)).await;
    for _ in 0..3 {
        for text in [false, true] {
            let sent = Instant::now();
            if text {
                v.ws.send(Message::Text("ping".into())).await.unwrap();
            } else {
                v.ws.send(Message::Ping(b"beat".to_vec().into())).await.unwrap();
            }
            loop {
                match v.recv(ANSWER).await.expect("a heartbeat is answered") {
                    In::Text(s) if text && s == "pong" => break,
                    In::Pong if !text => break,
                    In::Control(c) if c["type"] == "host" || c["type"] == "resync" => continue,
                    other => panic!("expected the pong, got {other:?}"),
                }
            }
            assert!(sent.elapsed() < Duration::from_millis(500), "answered in {:?}", sent.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Before a join too: a client may beat while it signs.
    let (mut c, _) = dial(t).await;
    c.ws.send(Message::Text("ping".into())).await.unwrap();
    assert!(matches!(c.recv(ANSWER).await, Some(In::Text(s)) if s == "pong"));
}

/// R-RELAY-1: at most sixteen batches go to a viewer unacknowledged; an
/// ack past what was sent counts as what was sent.
#[tokio::test]
async fn r_relay_1_a_viewer_is_sent_at_most_the_window_until_it_acks() {
    let t = "window";
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v, _) = joined(t, &viewer(t, "viewer")).await;
    for seq in 1..=40 {
        h.batch(seq).await;
    }
    for seq in 1..=16 {
        v.batch_in(seq).await;
    }
    v.quiet().await;
    v.ack(u64::MAX).await;
    for seq in 17..=32 {
        v.batch_in(seq).await;
    }
    v.quiet().await;
}

/// R-RELAY-1: a host's batches go up by one each; any other `seq` is
/// refused, since the ring buffer's resume depends on it.
#[tokio::test]
async fn r_relay_1_a_batch_out_of_seq_order_is_refused() {
    let t = "order";
    let (mut h, j) = joined(t, &host(t)).await;
    assert_eq!(j["seq"], 0);
    h.batch(1).await;
    h.batch(2).await;
    h.batch(4).await;
    h.refused("seq_out_of_order").await;
    // The host reconnecting to its stream carries on from where the relay is.
    let (mut h, j) = joined(t, &host(t)).await;
    assert_eq!(j["seq"], 2, "{j}");
    h.batch(2).await;
    h.refused("seq_out_of_order").await;
    let (mut h, _) = joined(t, &host(t)).await;
    h.batch(0).await;
    h.refused("seq_out_of_order").await;
}

/// R-RELAY-1: one host a session: the lease holder connecting again takes
/// the channel, and the older connection is told and closed.
#[tokio::test]
async fn r_relay_1_a_second_host_connection_replaces_the_first() {
    let t = "replace";
    let (mut old, _) = joined(t, &host(t)).await;
    let (mut new, _) = joined(t, &host(t)).await;
    old.refused("replaced").await;
    let (mut v, _) = joined(t, &viewer(t, "viewer")).await;
    new.batch(1).await;
    v.batch_in(1).await;
}

/// R-RELAY-1, R-LAG-6: a host joining with a new stream empties the ring
/// buffer, and every viewer is told to catch up; batches of the new stream
/// then follow.
#[tokio::test]
async fn r_relay_1_a_new_stream_empties_the_ring_buffer() {
    let t = "stream";
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v, _) = joined(t, &viewer(t, "viewer")).await;
    for seq in 1..=3 {
        h.batch(seq).await;
        v.batch_in(seq).await;
    }
    drop(h);
    let (mut h, j) = joined(t, &As { stream: Some(stream_id(t, 2)), ..host(t) }).await;
    assert_eq!(j["seq"], 0, "a new stream starts empty: {j}");
    let r = v.expect("resync").await;
    assert_eq!((r["reason"].as_str(), r["stream"].as_str()), (Some("stream"), Some(e2e::hex(&stream_id(t, 2)).as_str())));
    h.batch(1).await;
    v.batch_in(1).await;
    let (mut v2, _) = joined(t, &As { stream: Some(stream_id(t, 1)), after: Some(2), ..viewer(t, "viewer2") }).await;
    assert_eq!(v2.control().await["reason"], "stream");
}

/// R-RELAY-1: a viewer sending faster than its ceiling (100 messages a
/// second, relay.md → Limits) is refused and let go.
#[tokio::test]
async fn r_relay_1_a_viewer_past_its_rate_ceiling_is_refused() {
    let t = "rate";
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v, _) = joined(t, &viewer(t, "viewer")).await;
    for _ in 0..300 {
        let f = v.sealed(KIND_FRAME, 0, b"x");
        if v.ws.send(Message::Binary(f.encode().into())).await.is_err() {
            break;
        }
    }
    v.refused("rate_limited").await;
    // The host was sent what came under the ceiling, and is still there.
    let mut routed = 0;
    while let Some(m) = h.recv(QUIET).await {
        match m {
            In::Env(e) if e.kind == KIND_ROUTED => routed += 1,
            In::Closed(r) => panic!("the host was let go ({r:?}) for a viewer's flood"),
            _ => {}
        }
    }
    assert!((1..=150).contains(&routed), "{routed} frames reached the host");
    h.batch(1).await;
}

/// R-RELAY-1: a message past the contract's size cap is refused, and so is
/// a join past 64 KiB.
#[tokio::test]
async fn r_relay_1_a_message_past_the_size_cap_is_refused() {
    let t = "size";
    let (mut h, _) = joined(t, &host(t)).await;
    let big = vec![7u8; (4 << 20) + 1];
    let e = h.sealed(KIND_BATCH, 1, &big);
    let _ = h.ws.send(Message::Binary(e.encode().into())).await;
    h.refused("too_large").await;
    let (mut c, _) = dial(t).await;
    c.send_control(json!({"type": "join", "pad": "x".repeat(70 * 1024)})).await;
    c.refused("too_large").await;
}

/// R-LAG-6: a viewer that stops acknowledging while the host sends past
/// the ring buffer is told, once it acknowledges again, that the relay
/// cannot fill its gap — never served a batch after the gap as though it
/// followed — and then follows live.
#[tokio::test]
async fn r_lag_6_a_viewer_left_behind_by_the_ring_buffer_is_told_to_resync() {
    let t = "behind";
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v, _) = joined(t, &viewer(t, "viewer")).await;
    for seq in 1..=1104 {
        h.batch(seq).await;
    }
    h.acked(1104).await;
    for seq in 1..=16 {
        v.batch_in(seq).await;
    }
    v.ack(16).await;
    let r = v.control().await;
    assert_eq!((r["type"].as_str(), r["reason"].as_str()), (Some("resync"), Some("behind")), "{r}");
    h.batch(1105).await;
    v.batch_in(1105).await;
}

/// R-RELAY-1: a viewer's frame with no host connected is refused
/// `host_absent`, the link staying open; once the host is there, frames
/// reach it.
#[tokio::test]
async fn r_relay_1_a_frame_with_no_host_is_refused_host_absent_and_the_link_stays() {
    let t = "absent";
    let (mut v, j) = joined(t, &viewer(t, "viewer")).await;
    assert_eq!((j["host"].as_bool(), j["stream"].is_null()), (Some(false), true), "no stream yet reads as null: {j}");
    let f = v.sealed(KIND_FRAME, 0, b"sealed");
    v.send(f.clone()).await;
    let e = v.expect("error").await;
    assert_eq!(e["code"], "host_absent", "{e}");
    v.ws.send(Message::Text("ping".into())).await.unwrap();
    assert!(matches!(v.next().await, In::Text(s) if s == "pong"), "still open");
    let (mut h, _) = joined(t, &host(t)).await;
    v.expect("host").await;
    v.send(f).await;
    loop {
        match h.next().await {
            In::Env(e) if e.kind == KIND_ROUTED => break,
            In::Env(_) => continue,
            other => panic!("expected the routed frame, got {other:?}"),
        }
    }
}

/// R-RELAY-1: viewers reconnecting in a loop use up their own join
/// ceiling, never the host's: the lease holder still joins its channel.
#[tokio::test]
async fn r_relay_1_viewers_joining_in_a_loop_cannot_lock_the_host_out() {
    let t = "lockout";
    for which in ["viewer", "viewer2"] {
        for _ in 0..30 {
            let (c, _) = joined(t, &viewer(t, which)).await;
            drop(c);
        }
    }
    let (_h, j) = joined(t, &host(t)).await;
    assert_eq!(j["role"], "host");
}

/// Binds `sock` to `ip`, a loopback address other than 127.0.0.1, so the
/// relay sees another address. Linux answers for all of 127/8; macOS only
/// for the addresses lo0 is given (`sudo ifconfig lo0 alias 127.0.0.2 up`,
/// as CI does). False, with a line saying so, on a machine without it —
/// except under CI, which must run the check.
fn other_loopback(sock: &tokio::net::TcpSocket, ip: &str) -> bool {
    match sock.bind(format!("{ip}:0").parse().unwrap()) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable && std::env::var_os("CI").is_none() => {
            eprintln!("skipped: {ip} is not a loopback address here (`sudo ifconfig lo0 alias {ip} up` makes it one)");
            false
        }
        Err(e) => panic!("binding {ip}: {e}"),
    }
}

/// The reference relay's own defence before a join, beyond the contract:
/// connections that never finish the upgrade, from one address, cannot
/// crowd out another's, and a connection sending past the pre-join budget
/// is let go. Skipped against another relay unless KROWK_RELAY_REFERENCE
/// says it is one.
#[tokio::test]
async fn r_relay_1_the_reference_relay_bounds_what_comes_before_a_join() {
    if std::env::var_os("KROWK_RELAY_URL").is_some() && std::env::var_os("KROWK_RELAY_REFERENCE").is_none() {
        return;
    }
    let t = "prejoin";
    let authority = relay_url().split("://").nth(1).unwrap().to_string();
    let port: u16 = authority.rsplit(':').next().unwrap().parse().unwrap();
    // Seventy idle connections from 127.0.0.2, saying nothing.
    let mut idle = Vec::new();
    for _ in 0..70 {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        if !other_loopback(&sock, "127.0.0.2") {
            return;
        }
        if let Ok(s) = sock.connect(format!("127.0.0.1:{port}").parse().unwrap()).await {
            idle.push(s);
        }
    }
    let (_v, j) = joined(t, &viewer(t, "viewer")).await;
    assert_eq!(j["role"], "viewer", "another address still joins");
    // Past the pre-join budget: closed, with too_large when it can say so.
    let (mut c, _) = dial(t).await;
    let _ = c.ws.send(Message::Binary(vec![0u8; 200 * 1024].into())).await;
    loop {
        match c.recv(ANSWER).await {
            Some(In::Closed(_)) => break,
            Some(In::Control(e)) if e["type"] == "error" => assert_eq!(e["code"], "too_large", "{e}"),
            None => panic!("still open past the pre-join budget"),
            _ => {}
        }
    }
    drop(idle);
}

/// R-RELAY-1: connections that upgrade to a channel and never join cannot
/// keep its devices off it: at the pre-join cap the oldest is let go, not
/// the newcomer refused (relay.md → Limits).
#[tokio::test]
async fn r_relay_1_idle_connections_to_a_channel_cannot_lock_its_devices_out() {
    let t = "crowd";
    let url = format!("{}/v1/relay/{}", relay_url(), session_id(t));
    let authority = relay_url().split("://").nth(1).unwrap().to_string();
    let target: std::net::SocketAddr = tokio::net::lookup_host(&authority).await.unwrap().next().unwrap();
    // More than the channel's 16, from another address, each upgraded and
    // holding its challenge unanswered.
    let mut idle = Vec::new();
    for _ in 0..24 {
        let sock = if target.is_ipv4() { tokio::net::TcpSocket::new_v4() } else { tokio::net::TcpSocket::new_v6() }.unwrap();
        if target.ip().is_loopback() && target.is_ipv4() && !other_loopback(&sock, "127.0.0.9") {
            return;
        }
        let stream = sock.connect(target).await.unwrap();
        if let Ok((ws, _)) = tokio_tungstenite::client_async(url.as_str(), stream).await {
            idle.push(ws);
        }
    }
    let (_h, j) = joined(t, &host(t)).await;
    assert_eq!(j["role"], "host");
    let (_v, j) = joined(t, &viewer(t, "viewer")).await;
    assert_eq!(j["role"], "viewer");
    drop(idle);
}

/// R-LAG-6, slow and optional (`KROWK_RELAY_SLOW=1`): a channel idle for
/// 15 seconds — long enough for a Durable Object to be evicted from memory
/// — still serves a viewer's resume from its ring buffer. Ticket 18 runs
/// it against `wrangler dev` and the deployed relay.
#[tokio::test]
async fn r_lag_6_the_ring_buffer_survives_an_idle_channel() {
    if std::env::var_os("KROWK_RELAY_SLOW").is_none() {
        return;
    }
    let t = "idle";
    let stream = stream_id(t, 1);
    let (mut h, _) = joined(t, &host(t)).await;
    let (mut v, _) = joined(t, &viewer(t, "viewer")).await;
    for seq in 1..=16 {
        h.batch(seq).await;
    }
    h.acked(16).await;
    for seq in 1..=16 {
        v.batch_in(seq).await;
    }
    drop(v);
    tokio::time::sleep(Duration::from_secs(15)).await;
    let (mut v, j) = joined(t, &As { stream: Some(stream), after: Some(8), ..viewer(t, "viewer") }).await;
    assert_eq!(j["seq"], 16, "{j}");
    for seq in 9..=16 {
        v.batch_in(seq).await;
    }
    h.batch(17).await;
    v.batch_in(17).await;
}
