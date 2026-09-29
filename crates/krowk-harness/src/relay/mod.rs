//! The reference relay (`krowk relay serve`): the contract in Canon's
//! `engineering/relay.md`, implemented in Rust so the relay is replaceable
//! (R-RELAY-1). It is the hermetic stand-in the tests run against, a
//! self-hosting path, and the implementation ticket 18's Durable Object is
//! held to by the same conformance suite (`tests/relay_conformance.rs`).
//!
//! A relay carries one session per channel between the device holding the
//! session's lease (the host) and the devices watching it (viewers). It
//! holds only ciphertext: every batch and frame it carries must say it is
//! sealed (`enc` = 1), and it reads nothing but the 28-byte header ticket
//! 14 laid out (`daemon::ws::Envelope`, the same type, not a copy). What
//! it adds to that envelope is two kinds: its own JSON control messages
//! (`KIND_RELAY`) and a sealed envelope carried to or from one viewer
//! (`KIND_ROUTED`, the viewer's link in `seq`).
//!
//! - **Joining** is a challenge and a signature: the relay sends a random
//!   nonce, the device signs it with its Ed25519 signing key
//!   (`krowk_client::e2e::SigningKey::sign_relay_join`), binding the role,
//!   the session, its device id and the relay it dialed. Who is trusted
//!   comes from a roster (`Roster`): the registry's device records and
//!   leases in ticket 18, a file here.
//! - **Fan-out**: the host's batches go to every viewer; a viewer's frames
//!   go to the host alone, routed so the host knows whose they are.
//! - **The ring buffer** (R-LAG-6): each channel keeps the current
//!   stream's latest batches, bounded in count and bytes, so a viewer that
//!   reconnects resumes from the last `seq` it applied without the host;
//!   one behind the buffer is told to catch up from the host.
//! - **Heartbeats** are the relay's to answer, never the host's: a
//!   WebSocket ping, or the text message `ping`, is answered at once
//!   however long the host has been silent.
//!
//! Single-threaded, like the daemon: every channel's state is a `RefCell`
//! on one thread, each link's writes a task of their own, so one slow
//! viewer holds nobody else back.

pub mod roster;

use crate::daemon::ws::{ENC_XCHACHA20_POLY1305, Envelope, FLAG_ZSTD, HEADER, KIND_ACK, KIND_BATCH, KIND_FRAME, KIND_RELAY, KIND_ROUTED};
use futures_util::{SinkExt, StreamExt};
pub use krowk_client::e2e::canonical_origin;
pub use roster::Roster;
use krowk_client::e2e::{self, DeviceId, RELAY_ROLE_HOST, RELAY_ROLE_VIEWER};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// The contract's version, in the challenge.
pub const VERSION: u64 = 1;

/// The path a channel is reached at: `/v1/relay/<session uuid>`.
pub const PATH: &str = "/v1/relay/";

/// The contract's ceilings (relay.md → Limits). A relay may be more
/// generous; the conformance suite holds it to refusing past these.
#[derive(Debug, Clone)]
pub struct Limits {
    /// The largest message: the header and a daemon's largest frame.
    pub max_message: usize,
    /// The largest control message, and anything before a join.
    pub max_control: usize,
    /// The ring buffer: batches, and their bytes, whichever binds first.
    pub ring_batches: usize,
    pub ring_bytes: usize,
    /// Viewers on one channel at once.
    pub viewers: usize,
    /// Joins a minute to one channel: per device, and viewers' joins all
    /// told, so viewers reconnecting in a loop cannot use up the host's.
    pub joins_per_device: u32,
    pub joins_per_session: u32,
    /// Messages and bytes a second a link may send: a token bucket that
    /// holds a second's worth.
    pub host_messages: f64,
    pub host_bytes: f64,
    pub viewer_messages: f64,
    pub viewer_bytes: f64,
    /// What may wait to be written to one link before it is let go.
    pub link_queue: usize,
    /// Connections that have not joined yet: in all, and from one address.
    pub unjoined: usize,
    pub unjoined_per_peer: usize,
    /// Connections open on one channel that have not joined: the
    /// contract's own bound, which a per-session object can hold.
    pub unjoined_per_channel: usize,
    /// Bytes a connection may send before it has joined: the upgrade
    /// request, the join and a few heartbeats. Counted as they are read,
    /// so no more than this is buffered for anyone unauthenticated.
    pub prejoin_bytes: usize,
    /// Text heartbeats answered before a join.
    pub prejoin_pings: u32,
    /// How long the WebSocket upgrade may take, and then the join.
    pub upgrade_wait: Duration,
    pub join_wait: Duration,
    /// The relay's own pings, and silence past three of them closes a link.
    pub heartbeat: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_message: HEADER + (4 << 20),
            max_control: 64 * 1024,
            ring_batches: 1024,
            ring_bytes: 8 << 20,
            viewers: 16,
            joins_per_device: 30,
            joins_per_session: 60,
            host_messages: 2000.0,
            host_bytes: 32.0 * (1 << 20) as f64,
            viewer_messages: 100.0,
            viewer_bytes: 4.0 * (1 << 20) as f64 + HEADER as f64,
            link_queue: 16 << 20,
            unjoined: 1024,
            unjoined_per_peer: 32,
            unjoined_per_channel: 16,
            prejoin_bytes: 80 * 1024,
            prejoin_pings: 8,
            upgrade_wait: Duration::from_secs(3),
            join_wait: Duration::from_secs(10),
            heartbeat: crate::daemon::ws::HEARTBEAT,
        }
    }
}

/// The largest whole number a JSON field carries: what a JavaScript number
/// holds exactly (relay.md → Joining).
pub const MAX_JSON_INT: u64 = (1 << 53) - 1;

/// How long a channel nobody is on keeps its ring buffer.
pub const IDLE_CHANNEL: Duration = Duration::from_secs(5 * 60);

/// Batches in flight to a viewer before the relay waits for its ack: the
/// daemon's window (R-LAG-4), per link.
pub const WINDOW: u64 = crate::daemon::ws::WINDOW;

/// How a relay is run.
#[derive(Debug, Clone)]
pub struct Config {
    pub roster: Roster,
    /// The origin devices sign (`ws://127.0.0.1:7790`, `wss://relay.krowk.com`):
    /// what they dialed. Without it, `ws://` and the request's `Host` —
    /// for loopback only, since a relay that trusts the `Host` a client
    /// sends lets a relay in the middle pass its challenge through
    /// (`krowk relay serve` refuses another address without `--origin`).
    pub origin: Option<String>,
    pub limits: Limits,
}

/// Runs a relay on `listener` until the process ends.
pub fn run(listener: std::net::TcpListener, config: Config) -> Result<(), String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let listener = TcpListener::from_std(listener).map_err(|e| e.to_string())?;
        serve(listener, config).await;
        Ok(())
    })
}

/// Accepts connections for ever. Inside a `LocalSet`.
pub async fn serve(listener: TcpListener, config: Config) {
    let relay = Rc::new(Relay { config, channels: RefCell::new(HashMap::new()), pending: RefCell::new(VecDeque::new()), pending_ids: Cell::new(0), links: Cell::new(0) });
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::task::spawn_local(connection(stream, peer.ip(), relay.clone()));
            }
            Err(e) => {
                eprintln!("relay accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

struct Relay {
    config: Config,
    channels: RefCell<HashMap<[u8; 16], Channel>>,
    /// Connections not yet joined, oldest first. At any cap the oldest
    /// is let go to make room, never the newcomer refused: someone
    /// holding connections open must then outpace honest clients, who
    /// join within a round trip of the challenge.
    pending: RefCell<VecDeque<Pending>>,
    pending_ids: Cell<u64>,
    links: Cell<u64>,
}

struct Pending {
    id: u64,
    peer: std::net::IpAddr,
    session: Option<[u8; 16]>,
    kill: Rc<tokio::sync::Notify>,
}

impl Relay {
    /// Lets go the oldest pending connection `which` picks.
    fn evict(&self, which: impl Fn(&Pending) -> bool) {
        let mut p = self.pending.borrow_mut();
        if let Some(i) = p.iter().position(which) {
            let gone = p.remove(i).expect("found");
            gone.kill.notify_one();
        }
    }

    /// A new pending connection, room made for it under the caps.
    fn arrive(&self, peer: std::net::IpAddr) -> (u64, Rc<tokio::sync::Notify>) {
        let limits = &self.config.limits;
        if self.pending.borrow().iter().filter(|p| p.peer == peer).count() >= limits.unjoined_per_peer {
            self.evict(|p| p.peer == peer);
        }
        if self.pending.borrow().len() >= limits.unjoined {
            self.evict(|_| true);
        }
        let id = self.pending_ids.get() + 1;
        self.pending_ids.set(id);
        let kill = Rc::new(tokio::sync::Notify::new());
        self.pending.borrow_mut().push_back(Pending { id, peer, session: None, kill: kill.clone() });
        (id, kill)
    }

    /// Its channel, once the upgrade names it: room made there too.
    fn names(&self, id: u64, session: [u8; 16]) {
        if self.pending.borrow().iter().filter(|p| p.session == Some(session)).count() >= self.config.limits.unjoined_per_channel {
            self.evict(|p| p.session == Some(session));
        }
        if let Some(p) = self.pending.borrow_mut().iter_mut().find(|p| p.id == id) {
            p.session = Some(session);
        }
    }

    fn done(&self, id: u64) {
        self.pending.borrow_mut().retain(|p| p.id != id);
    }
}

/// A count that starts again each minute.
#[derive(Default)]
struct Minute {
    start: Option<Instant>,
    n: u32,
}

impl Minute {
    /// Counts one; false past `max` this minute.
    fn take(&mut self, max: u32) -> bool {
        let now = Instant::now();
        if self.start.is_none_or(|s| now.duration_since(s) >= Duration::from_secs(60)) {
            *self = Minute { start: Some(now), n: 0 };
        }
        self.n += 1;
        self.n <= max
    }
}

/// A token bucket holding a second's worth.
struct Bucket {
    rate: f64,
    level: f64,
    at: Instant,
}

impl Bucket {
    fn new(rate: f64) -> Bucket {
        Bucket { rate, level: rate, at: Instant::now() }
    }

    fn take(&mut self, n: f64) -> bool {
        let now = Instant::now();
        self.level = (self.level + now.duration_since(self.at).as_secs_f64() * self.rate).min(self.rate);
        self.at = now;
        if self.level < n {
            return false;
        }
        self.level -= n;
        true
    }
}

/// One session's channel.
#[derive(Default)]
struct Channel {
    host: Option<Rc<Link>>,
    /// The host's stream: the epoch it seals batches under, which names
    /// what the ring buffer holds. Zero before any host joined.
    stream: [u8; 16],
    /// The current stream's latest batches, whole, in `seq` order with no
    /// gap: a host's `seq` goes up by one each batch.
    ring: VecDeque<(u64, Rc<[u8]>)>,
    ring_bytes: usize,
    /// The last `seq` the channel was sent in this stream; 0 for none.
    last: u64,
    viewers: HashMap<u64, Viewer>,
    /// This minute's joins: each device's, and all viewers'.
    device_joins: HashMap<[u8; 16], Minute>,
    viewer_joins: Minute,
    /// Since when nobody has been on the channel.
    empty_since: Option<Instant>,
}

impl Channel {
    fn stream_json(&self) -> Value {
        if self.stream == [0; 16] { Value::Null } else { e2e::hex(&self.stream).into() }
    }
}

struct Viewer {
    link: Rc<Link>,
    /// The next batch to send it, and the last it acknowledged: never
    /// more than `WINDOW` apart.
    next: u64,
    acked: u64,
}

/// One connection's write side: a queue its own task drains, so a slow
/// socket holds up only itself.
struct Link {
    id: u64,
    device: DeviceId,
    tx: mpsc::UnboundedSender<Message>,
    queued: Rc<Cell<usize>>,
    limit: usize,
}

impl Link {
    /// Queues a message; false when the link has more waiting than it may.
    fn send(&self, m: Message) -> bool {
        let n = m.len();
        if self.queued.get() + n > self.limit {
            return false;
        }
        self.queued.set(self.queued.get() + n);
        self.tx.send(m).is_ok()
    }

    fn control(&self, v: Value) -> bool {
        self.send(Message::Binary(control(&v).into()))
    }

    /// Lets a link go that has more waiting than it may: told, if there is
    /// room to tell it, then closed. Its reader sees the close and leaves.
    fn overflow(&self) {
        let r = refuse("link_overflow", "more waits for this link than the relay holds", "read faster, and join again with your cursor");
        self.queued.set(0);
        let _ = self.tx.send(Message::Binary(control(&r.json()).into()));
        let _ = self.tx.send(Message::Close(Some(CloseFrame { code: CloseCode::Policy, reason: "link_overflow".into() })));
    }
}

/// A connection's reads, counted against what it may send before it has
/// joined: past the budget the read fails and the connection goes, so
/// nothing unauthenticated is buffered past it. Lifted once it joins.
struct Metered {
    inner: TcpStream,
    left: Rc<Cell<usize>>,
}

impl tokio::io::AsyncRead for Metered {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len() - before;
        if n > self.left.get() {
            self.left.set(0);
            return std::task::Poll::Ready(Err(std::io::Error::other("sent past what it may before joining")));
        }
        self.left.set(self.left.get() - n);
        r
    }
}

impl tokio::io::AsyncWrite for Metered {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A control message: kind 4, plain JSON, no session.
pub fn control(v: &Value) -> Vec<u8> {
    Envelope { kind: KIND_RELAY, flags: 0, enc: 0, session: [0; 16], seq: 0, payload: serde_json::to_vec(v).expect("json") }.encode()
}

/// A refusal: the code a client acts on, what happened, and what to do.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
    pub fix: &'static str,
}

fn refuse(code: &'static str, message: impl Into<String>, fix: &'static str) -> Refusal {
    Refusal { code, message: message.into(), fix }
}

impl Refusal {
    fn json(&self) -> Value {
        json!({"type": "error", "code": self.code, "message": self.message, "fix": self.fix})
    }
}

type Ws = tokio_tungstenite::WebSocketStream<Metered>;

async fn connection(stream: TcpStream, peer: std::net::IpAddr, relay: Rc<Relay>) {
    let limits = &relay.config.limits;
    let (pending, kill) = relay.arrive(peer);
    let left = Rc::new(Cell::new(limits.prejoin_bytes));
    let joined = handshake(Metered { inner: stream, left: left.clone() }, &relay, pending, &kill).await;
    relay.done(pending);
    let Some((ws, session, device, role, join)) = joined else { return };
    left.set(usize::MAX);
    let (tx, rx) = mpsc::unbounded_channel();
    let id = relay.links.get() + 1;
    relay.links.set(id);
    let link = Rc::new(Link { id, device, tx, queued: Rc::default(), limit: relay.config.limits.link_queue });
    let (sink, incoming) = ws.split();
    let writer = tokio::task::spawn_local(write(sink, rx, link.queued.clone()));
    enter(&relay, session, &link, role, join);
    read(incoming, relay.clone(), session, link, role).await;
    // What was queued before it went is written, then the socket closes.
    let _ = writer.await;
}

/// Closes with an error message first, the close's reason its code.
async fn close(ws: &mut Ws, r: &Refusal) {
    let _ = ws.send(Message::Binary(control(&r.json()).into())).await;
    let _ = ws.send(Message::Close(Some(CloseFrame { code: CloseCode::Policy, reason: r.code.into() }))).await;
    let _ = ws.close(None).await;
}

/// The WebSocket handshake, the challenge and the join.
async fn handshake(stream: Metered, relay: &Rc<Relay>, pending: u64, kill: &tokio::sync::Notify) -> Option<(Ws, [u8; 16], DeviceId, u8, Join)> {
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    let _ = stream.inner.set_nodelay(true);
    let limits = &relay.config.limits;
    let config = WebSocketConfig::default().max_message_size(Some(limits.max_message)).max_frame_size(Some(limits.max_message));
    let seen: Rc<RefCell<(Option<[u8; 16]>, String)>> = Rc::default();
    let into = seen.clone();
    #[allow(clippy::result_large_err)]
    let route = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let session = req.uri().path().strip_prefix(PATH).and_then(parse_uuid);
        let host = req.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string();
        let Some(session) = session else {
            let mut refused = ErrorResponse::new(Some(format!("a relay channel is {PATH}<session id>")));
            *refused.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::NOT_FOUND;
            return Err(refused);
        };
        *into.borrow_mut() = (Some(session), host);
        Ok(resp)
    };
    let upgrade = tokio::time::timeout(limits.upgrade_wait, tokio_tungstenite::accept_hdr_async_with_config(stream, route, Some(config)));
    let mut ws = tokio::select! {
        _ = kill.notified() => return None,
        r = upgrade => r.ok()?.ok()?,
    };
    let (session, host) = seen.borrow().clone();
    let session = session?;
    relay.names(pending, session);
    let result = challenge(&mut ws, relay, session, &host, kill).await;
    let (device, role, join) = result?;
    Some((ws, session, device, role, join))
}

/// The challenge, and the join that answers it.
async fn challenge(ws: &mut Ws, relay: &Rc<Relay>, session: [u8; 16], host: &str, kill: &tokio::sync::Notify) -> Option<(DeviceId, u8, Join)> {
    let limits = &relay.config.limits;
    let given = relay.config.origin.clone().unwrap_or_else(|| format!("ws://{host}"));
    let Some(origin) = e2e::canonical_origin(&given) else {
        close(ws, &refuse("bad_origin", format!("{given:?} is no origin a device can sign"), "dial the relay by a host name or address, or start it with --origin")).await;
        return None;
    };
    let nonce: [u8; 32] = e2e::random();
    let challenge = json!({"type": "challenge", "version": VERSION, "nonce": e2e::hex(&nonce), "relay": origin, "session": e2e::hex(&session)});
    ws.send(Message::Binary(control(&challenge).into())).await.ok()?;
    let deadline = tokio::time::Instant::now() + limits.join_wait;
    let mut pings = 0;
    let join = loop {
        let next = tokio::select! {
            _ = kill.notified() => {
                close(ws, &refuse("join_timeout", "the oldest connection not yet joined made way for a newer one", "send the join as soon as the challenge arrives")).await;
                return None;
            }
            r = tokio::time::timeout_at(deadline, ws.next()) => r,
        };
        match next {
            Err(_) => {
                close(ws, &refuse("join_timeout", "no join arrived in time", "send the join within ten seconds of the challenge")).await;
                return None;
            }
            Ok(Some(Ok(Message::Binary(b)))) => break b,
            Ok(Some(Ok(Message::Text(t)))) if t.as_str() == "ping" => {
                pings += 1;
                if pings > limits.prejoin_pings {
                    close(ws, &refuse("rate_limited", "more heartbeats than a join takes to sign", "join, then beat")).await;
                    return None;
                }
                let _ = ws.send(Message::Text("pong".into())).await;
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(Some(Err(_))) => {
                close(ws, &refuse("too_large", format!("at most {} bytes may come before the join", limits.prejoin_bytes), "send the join, at most 64 KiB, and nothing larger before it")).await;
                return None;
            }
            Ok(Some(Ok(_))) => {
                close(ws, &refuse("not_joined", "the first message after the challenge must be a join", "answer the challenge with a join control message")).await;
                return None;
            }
            Ok(_) => return None,
        }
    };
    let parsed = if join.len() > limits.max_control { Err(refuse("too_large", "a join is at most 64 KiB", "send a join, nothing larger")) } else { read_join(&join) };
    match parsed.and_then(|j| admit(relay, session, &nonce, &origin, j)) {
        Err(r) => {
            close(ws, &r).await;
            None
        }
        Ok(found) => Some(found),
    }
}

/// A join, as the client sent it.
struct Join {
    role: u8,
    device: DeviceId,
    signature: Vec<u8>,
    fence: Option<u64>,
    stream: Option<[u8; 16]>,
    after: Option<u64>,
}

fn read_join(b: &[u8]) -> Result<Join, Refusal> {
    let bad = |what: &str| refuse("bad_join", format!("the join {what}"), "send {type: \"join\", role, device, signature} as relay.md → Joining lays out");
    let e = Envelope::decode(b).map_err(|e| bad(&format!("is no envelope: {e}")))?;
    if e.kind != KIND_RELAY {
        return Err(refuse("not_joined", format!("an envelope of kind {} came before the join", e.kind), "answer the challenge with a join control message first"));
    }
    if e.flags != 0 {
        return Err(bad("is compressed; a control message is plain"));
    }
    let v: Value = serde_json::from_slice(&e.payload).map_err(|_| bad("is not JSON"))?;
    if v["type"] != "join" {
        return Err(bad("is not of type \"join\""));
    }
    let role = match v["role"].as_str() {
        Some("host") => RELAY_ROLE_HOST,
        Some("viewer") => RELAY_ROLE_VIEWER,
        _ => return Err(bad("has no role \"host\" or \"viewer\"")),
    };
    let device = v["device"].as_str().and_then(DeviceId::parse).ok_or_else(|| bad("names no device id (32 hex characters)"))?;
    let signature = v["signature"].as_str().and_then(e2e::unhex).filter(|s| s.len() == 64).ok_or_else(|| bad("has no signature (128 hex characters)"))?;
    let stream = match v.get("stream").filter(|s| !s.is_null()) {
        None => None,
        Some(s) => Some(s.as_str().and_then(e2e::unhex).and_then(|b| <[u8; 16]>::try_from(b).ok()).filter(|b| *b != [0; 16]).ok_or_else(|| bad("has a stream that is not 32 hex characters, or is zero"))?),
    };
    let after = match v.get("afterSeq").filter(|s| !s.is_null()) {
        None => None,
        Some(s) => Some(s.as_u64().filter(|n| *n <= MAX_JSON_INT).ok_or_else(|| bad("has an afterSeq that is not a whole number up to 2^53 - 1"))?),
    };
    let fence = match v.get("fence").filter(|s| !s.is_null()) {
        None => None,
        Some(s) => Some(s.as_u64().filter(|n| *n <= MAX_JSON_INT).ok_or_else(|| bad("has a fence that is not a whole number up to 2^53 - 1"))?),
    };
    Ok(Join { role, device, signature, fence, stream, after })
}

/// Who may join: a device the roster knows, not revoked, whose signature
/// verifies, of the session's workspace, and — to host — the holder of
/// its lease, naming the lease's current fence. The signature proves the
/// device; the fence says it knows the lease is still its own, so a host
/// displaced by another device's takeover is refused rather than let
/// back in. The lease token stays between the holder and the registry: a
/// relay never needs it, so never holds it. Checked in that order, so a
/// device the relay does not trust learns nothing about the session.
fn admit(relay: &Relay, session: [u8; 16], nonce: &[u8; 32], origin: &str, j: Join) -> Result<(DeviceId, u8, Join), Refusal> {
    let ring = &relay.config.roster;
    let Some(device) = ring.device(&j.device) else {
        return Err(refuse("unknown_device", format!("device {} is not one this relay trusts", j.device), "register the device (krowk sync init, or krowk sync join and approve it), then connect again"));
    };
    if device.revoked {
        return Err(refuse("device_revoked", format!("device {} was removed from its workspace", j.device), "approve this machine again from one that syncs: krowk sync join here, krowk devices approve there"));
    }
    if device.signing.verify_relay_join(&j.signature, j.role, &session, nonce, &j.device, origin).is_err() {
        return Err(refuse("bad_signature", "the join's signature does not verify under the device's signing key", "sign this challenge's nonce with the device's own signing key, for the role, session and relay origin you dialed"));
    }
    // Another workspace's session reads as no session: a device of one
    // workspace learns nothing of another's session ids.
    let Some(entry) = ring.session(&session).filter(|e| e.workspace == device.workspace) else {
        return Err(refuse("unknown_session", "the relay has no such session for this device", "open a session of your own workspace that syncs; its lease names the host"));
    };
    let limits = &relay.config.limits;
    let mut channels = relay.channels.borrow_mut();
    let channel = channels.entry(session).or_default();
    if !channel.device_joins.entry(j.device.0).or_default().take(limits.joins_per_device) {
        return Err(refuse("rate_limited", format!("device {} joined this session more than {} times this minute", j.device, limits.joins_per_device), "wait a minute, and reconnect with backoff"));
    }
    if j.role == RELAY_ROLE_HOST {
        if entry.holder != j.device.0 {
            return Err(refuse("not_lease_holder", "only the device holding the session's lease may host it", "take the session's lease (krowk resumes it on this device), or join as a viewer"));
        }
        if j.fence != Some(entry.fence) {
            return Err(refuse("stale_lease", format!("the lease's fence is {}, not {}", entry.fence, j.fence.map_or("absent".into(), |f| f.to_string())), "read the session's lease from the registry again; if another device took it, join as a viewer"));
        }
        if j.stream.is_none() {
            return Err(refuse("bad_join", "a host's join names no stream", "send the stream epoch the host seals batches under, 32 hex characters"));
        }
    } else {
        // Full is checked before the join is counted, so being refused as
        // full costs the session nothing.
        if channel.viewers.len() >= limits.viewers {
            return Err(refuse("channel_full", format!("the session has {} viewers already", channel.viewers.len()), "close a device watching the session, then join again"));
        }
        if !channel.viewer_joins.take(limits.joins_per_session) {
            return Err(refuse("rate_limited", format!("viewers joined the session more than {} times this minute", limits.joins_per_session), "wait a minute, and reconnect with backoff"));
        }
    }
    Ok((j.device, j.role, j))
}

/// Puts a joined link on its channel, and says so.
fn enter(relay: &Relay, session: [u8; 16], link: &Rc<Link>, role: u8, j: Join) {
    let mut channels = relay.channels.borrow_mut();
    // A channel nobody has been on for a while is forgotten, its buffer
    // with it: a host coming back starts it afresh.
    channels.retain(|id, c| *id == session || c.empty_since.is_none_or(|t| t.elapsed() < IDLE_CHANNEL));
    let ch = channels.entry(session).or_default();
    ch.empty_since = None;
    if role == RELAY_ROLE_HOST {
        let stream = j.stream.expect("admitted with a stream");
        if let Some(old) = ch.host.take() {
            old.control(refuse("replaced", "another connection of the lease holder took the channel", "nothing to do if that was this device reconnecting").json());
            let _ = old.send(Message::Close(Some(CloseFrame { code: CloseCode::Policy, reason: "replaced".into() })));
        }
        let new_stream = stream != ch.stream;
        if new_stream {
            ch.stream = stream;
            ch.ring.clear();
            ch.ring_bytes = 0;
            ch.last = 0;
        }
        ch.host = Some(link.clone());
        link.control(json!({"type": "joined", "role": "host", "link": link.id, "stream": e2e::hex(&stream), "seq": ch.last, "viewers": ch.viewers.len()}));
        for v in ch.viewers.values_mut() {
            v.link.control(json!({"type": "host", "present": true}));
            if new_stream {
                v.next = 0;
                v.acked = 0;
                v.link.control(json!({"type": "resync", "reason": "stream", "stream": e2e::hex(&stream), "seq": 0}));
            }
            link.control(json!({"type": "viewer", "event": "joined", "link": v.link.id, "device": v.link.device.to_string()}));
        }
        return;
    }
    let stream = ch.stream_json();
    let host = ch.host.is_some();
    link.control(json!({"type": "joined", "role": "viewer", "link": link.id, "stream": stream.clone(), "seq": ch.last, "host": host}));
    // Where it starts: after its cursor when the buffer still holds what
    // follows it in this stream, else live, told to catch up from the host.
    let live = ch.last;
    // 0 is "whatever the stream's first batch is", for a stream the relay
    // has seen nothing of yet.
    let now = if live == 0 { 0 } else { live + 1 };
    let (next, why) = match (j.stream, j.after) {
        (None, _) | (_, None) => (now, None),
        (Some(s), Some(_)) if s != ch.stream => (now, Some("stream")),
        (Some(_), Some(after)) if after > live => (now, Some("ahead")),
        (Some(_), Some(after)) if after < live && ch.ring.front().is_none_or(|(first, _)| after + 1 < *first) => (now, Some("beyond_buffer")),
        (Some(_), Some(after)) => (after + 1, None),
    };
    if let Some(reason) = why {
        link.control(json!({"type": "resync", "reason": reason, "stream": stream, "seq": live}));
    }
    ch.viewers.insert(link.id, Viewer { link: link.clone(), next, acked: next.saturating_sub(1) });
    if let Some(h) = &ch.host {
        h.control(json!({"type": "viewer", "event": "joined", "link": link.id, "device": link.device.to_string()}));
    }
    pump(ch, link.id);
}

/// Sends a viewer what the buffer holds for it, up to its window.
fn pump(ch: &mut Channel, id: u64) {
    let Some(first) = ch.ring.front().map(|(s, _)| *s) else { return };
    let stream = ch.stream_json();
    let Some(v) = ch.viewers.get_mut(&id) else { return };
    if v.next == 0 {
        v.next = first;
        v.acked = first - 1;
    }
    while v.next <= ch.last && v.next < v.acked.saturating_add(WINDOW).saturating_add(1) {
        if v.next < first {
            // Fell behind the buffer while its window was full.
            v.link.control(json!({"type": "resync", "reason": "behind", "stream": stream, "seq": ch.last}));
            v.next = ch.last + 1;
            v.acked = ch.last;
            return;
        }
        let (_, bytes) = &ch.ring[(v.next - first) as usize];
        if !v.link.send(Message::Binary(bytes.to_vec().into())) {
            v.link.overflow();
            return;
        }
        v.next += 1;
    }
}

async fn write(mut sink: futures_util::stream::SplitSink<Ws, Message>, mut rx: mpsc::UnboundedReceiver<Message>, queued: Rc<Cell<usize>>) {
    while let Some(m) = rx.recv().await {
        queued.set(queued.get().saturating_sub(m.len()));
        let closing = matches!(m, Message::Close(_));
        if sink.send(m).await.is_err() || closing {
            break;
        }
    }
    let _ = sink.close().await;
}

/// Reads a joined link until it goes, answering heartbeats itself.
async fn read(mut incoming: futures_util::stream::SplitStream<Ws>, relay: Rc<Relay>, session: [u8; 16], link: Rc<Link>, role: u8) {
    let limits = relay.config.limits.clone();
    let (mut messages, mut bytes) = if role == RELAY_ROLE_HOST { (Bucket::new(limits.host_messages), Bucket::new(limits.host_bytes)) } else { (Bucket::new(limits.viewer_messages), Bucket::new(limits.viewer_bytes)) };
    let pinger = {
        let link = link.clone();
        tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + limits.heartbeat, limits.heartbeat);
            loop {
                tick.tick().await;
                if !link.send(Message::Ping(Vec::new().into())) {
                    return;
                }
            }
        })
    };
    let mut sent = 0u64;
    let refusal = loop {
        let msg = match tokio::time::timeout(limits.heartbeat * 3, incoming.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_)))) => break Some(refuse("too_large", format!("a message is at most {} bytes", limits.max_message), "send smaller batches; a daemon's frame is at most 4 MiB")),
            Ok(_) => break None,
            Err(_) => break None,
        };
        let b = match msg {
            Message::Binary(b) => b,
            // The heartbeat a browser or a Worker can send: answered here,
            // never by the host (R-LAG-6).
            Message::Text(t) if t.as_str() == "ping" => {
                link.send(Message::Text("pong".into()));
                continue;
            }
            Message::Text(_) => break Some(refuse("not_binary", "every message but the text heartbeat is binary", "send envelopes as binary messages")),
            Message::Close(_) => break None,
            // tungstenite answers a ping as it reads it, and the read that
            // follows flushes the pong; a pong only says the peer is
            // there, which reading it noted.
            _ => continue,
        };
        if !messages.take(1.0) || !bytes.take(b.len() as f64) {
            break Some(refuse("rate_limited", "the link sent past its ceiling of messages or bytes a second", "send less often: batch, as the daemon does every 16 ms"));
        }
        let e = match Envelope::decode(&b) {
            Ok(e) => e,
            Err(e) => break Some(refuse("bad_envelope", e, "send the 28-byte header of relay.md → Frames, version 1")),
        };
        let outcome = if role == RELAY_ROLE_HOST { from_host(&relay, session, &link, e, &b, &mut sent) } else { from_viewer(&relay, session, &link, e, &b) };
        if let Err(r) = outcome {
            if r.code == "host_absent" {
                link.control(r.json());
                continue;
            }
            break Some(r);
        }
    };
    pinger.abort();
    if let Some(r) = refusal {
        link.control(r.json());
        link.send(Message::Close(Some(CloseFrame { code: CloseCode::Policy, reason: r.code.into() })));
    }
    leave(&relay, session, &link, role);
}

/// Checks a sealed envelope's header: of this session and sealed.
fn sealed(e: &Envelope, session: [u8; 16]) -> Result<(), Refusal> {
    if e.enc != ENC_XCHACHA20_POLY1305 {
        return Err(refuse("plaintext", format!("envelope kind {} with enc {} is not sealed; a relay carries ciphertext only", e.kind, e.enc), "seal every batch and frame under the session key (enc 1), the hello included"));
    }
    if e.session != session {
        return Err(refuse("wrong_session", "an envelope names another session than its channel's", "send each session's envelopes on its own channel"));
    }
    if e.flags & !FLAG_ZSTD != 0 {
        return Err(refuse("bad_envelope", format!("flags {:#04x} set bits the envelope does not define", e.flags), "set only bit 0 (zstd)"));
    }
    Ok(())
}

fn from_host(relay: &Relay, session: [u8; 16], link: &Rc<Link>, e: Envelope, raw: &[u8], sent: &mut u64) -> Result<(), Refusal> {
    let mut channels = relay.channels.borrow_mut();
    let ch = channels.get_mut(&session).expect("joined");
    if !ch.host.as_ref().is_some_and(|h| h.id == link.id) {
        return Ok(());
    }
    match e.kind {
        KIND_BATCH => {
            sealed(&e, session)?;
            let first = ch.last == 0;
            if e.seq == 0 || e.seq > MAX_JSON_INT || (!first && e.seq != ch.last + 1) {
                return Err(refuse("seq_out_of_order", format!("batch seq {} does not follow {}", e.seq, ch.last), "number a stream's batches 1, 2, 3 … on the relay, one more each, from where the joined message says the relay is"));
            }
            let bytes: Rc<[u8]> = raw.into();
            ch.last = e.seq;
            ch.ring_bytes += bytes.len();
            ch.ring.push_back((e.seq, bytes));
            let limits = &relay.config.limits;
            while ch.ring.len() > limits.ring_batches || ch.ring_bytes > limits.ring_bytes {
                let (_, old) = ch.ring.pop_front().expect("not empty");
                ch.ring_bytes -= old.len();
            }
            let ids: Vec<u64> = ch.viewers.keys().copied().collect();
            for id in ids {
                pump(ch, id);
            }
            *sent += 1;
            if (*sent).is_multiple_of(WINDOW / 2) {
                link.send(Message::Binary(Envelope { kind: KIND_ACK, flags: 0, enc: 0, session, seq: ch.last, payload: Vec::new() }.encode().into()));
            }
            Ok(())
        }
        KIND_ROUTED => {
            sealed(&e, session)?;
            let inner = Envelope::decode(&e.payload).map_err(|m| refuse("bad_envelope", format!("a routed envelope carries no envelope: {m}"), "put a whole sealed envelope in a routed one's payload"))?;
            if inner.kind != KIND_BATCH {
                return Err(refuse("wrong_kind", "a host routes batches to a viewer", "route kind 1"));
            }
            sealed(&inner, session)?;
            if let Some(v) = ch.viewers.get(&e.seq)
                && !v.link.send(Message::Binary(e.payload.into()))
            {
                v.link.overflow();
            }
            Ok(())
        }
        KIND_ACK => Ok(()),
        k => Err(refuse("wrong_kind", format!("a host may not send kind {k}"), "a host sends batches (1), routed batches (5) and acks (3)")),
    }
}

fn from_viewer(relay: &Relay, session: [u8; 16], link: &Rc<Link>, e: Envelope, raw: &[u8]) -> Result<(), Refusal> {
    let mut channels = relay.channels.borrow_mut();
    let ch = channels.get_mut(&session).expect("joined");
    match e.kind {
        KIND_FRAME => {
            sealed(&e, session)?;
            let Some(host) = &ch.host else {
                return Err(refuse("host_absent", "the session's host is not connected; the frame was dropped", "send it again once a host message says the host is present"));
            };
            let routed = Envelope { kind: KIND_ROUTED, flags: 0, enc: ENC_XCHACHA20_POLY1305, session, seq: link.id, payload: raw.to_vec() };
            if !host.send(Message::Binary(routed.encode().into())) {
                host.overflow();
            }
            Ok(())
        }
        KIND_ACK => {
            if let Some(v) = ch.viewers.get_mut(&link.id) {
                // At most what was sent, so no value overflows the window.
                v.acked = v.acked.max(e.seq.min(v.next.saturating_sub(1)));
            }
            pump(ch, link.id);
            Ok(())
        }
        k => Err(refuse("wrong_kind", format!("a viewer may not send kind {k}"), "a viewer sends frames (2) and acks (3)")),
    }
}

fn leave(relay: &Relay, session: [u8; 16], link: &Rc<Link>, role: u8) {
    let mut channels = relay.channels.borrow_mut();
    let Some(ch) = channels.get_mut(&session) else { return };
    if role == RELAY_ROLE_HOST {
        if ch.host.as_ref().is_some_and(|h| h.id == link.id) {
            ch.host = None;
            for v in ch.viewers.values() {
                v.link.control(json!({"type": "host", "present": false}));
            }
        }
    } else if ch.viewers.remove(&link.id).is_some()
        && let Some(h) = &ch.host
    {
        h.control(json!({"type": "viewer", "event": "left", "link": link.id, "device": link.device.to_string()}));
    }
    if ch.host.is_none() && ch.viewers.is_empty() {
        ch.empty_since = Some(Instant::now());
    }
}

/// A session id's sixteen bytes, from its canonical UUID form.
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if s.len() != 36 || hex.len() != 32 {
        return None;
    }
    e2e::unhex(&hex.to_ascii_lowercase())?.try_into().ok()
}
