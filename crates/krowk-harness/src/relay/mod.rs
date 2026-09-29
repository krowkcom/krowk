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
//!   the session, its device id and the relay it dialed, under the
//!   signing key a registry-signed ticket names (`Roster` holds the
//!   registry's ticket keys; `krowk_client::relay_ticket`).
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
    /// Connections whose ticket was verified and which have not joined yet,
    /// in all: a pool of their own, so neither a flood of strangers nor one
    /// of tickets can take the other's room.
    pub unjoined_ticketed: usize,
    /// Connections open on one channel that have not joined: the
    /// contract's own bound, which a per-session object can hold.
    pub unjoined_per_channel: usize,
    /// Connections one device's ticket may hold open on a channel before
    /// they join: a replayed ticket crowds out only the device it names.
    pub unjoined_per_device: usize,
    /// The longest ticket lifetime accepted, the clock skew allowed, and
    /// the margin past both before a channel nobody is on is forgotten —
    /// its fence with it, which is safe only once every ticket issued
    /// before the lease last moved has expired (relay.md → Tickets).
    pub ticket_lifetime: u64,
    pub ticket_skew: u64,
    pub idle_margin: u64,
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
    /// Links are numbered from past this. A host's direct listener starts
    /// far past the relay's own numbers, so the two relays a host is on at
    /// once never name two viewers alike to its one set of chains.
    pub first_link: u64,
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
            unjoined_ticketed: 1024,
            unjoined_per_channel: 64,
            unjoined_per_device: 2,
            ticket_lifetime: krowk_client::relay_ticket::MAX_LIFETIME,
            ticket_skew: krowk_client::relay_ticket::SKEW,
            idle_margin: krowk_client::relay_ticket::IDLE_MARGIN,
            prejoin_bytes: 80 * 1024,
            prejoin_pings: 8,
            upgrade_wait: Duration::from_secs(3),
            join_wait: Duration::from_secs(10),
            heartbeat: crate::daemon::ws::HEARTBEAT,
            first_link: 0,
        }
    }
}

/// Which relay environment a connection is of (relay.md → Joining, `env`):
/// dialed as `?env=` and repeated in the join, "production" when absent.
/// A channel is named by the env and the session, so the two never share a
/// channel, a buffer or a viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Env {
    Production,
    Development,
}

impl Env {
    /// None for anything but the two names; absent is production.
    pub fn parse(s: Option<&str>) -> Option<Env> {
        match s {
            None | Some("production") => Some(Env::Production),
            Some("development") => Some(Env::Development),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Env::Production => "production",
            Env::Development => "development",
        }
    }
}

/// A channel's name: its env and its session.
type Key = (Env, [u8; 16]);

/// What the upgrade request said: the session, the `Host`, and the env.
type Dialed = (Option<[u8; 16]>, String, Option<Env>, Option<String>);

/// One structured line per join and per refusal, and nothing a device sent
/// in it: no session, no device, no payload.
fn record(event: &str, env: Option<Env>, role: Option<u8>, outcome: &str) {
    let role = match role {
        Some(RELAY_ROLE_HOST) => Value::from("host"),
        Some(_) => Value::from("viewer"),
        None => Value::Null,
    };
    eprintln!("{}", json!({"event": event, "env": env.map_or("invalid", Env::as_str), "role": role, "outcome": outcome}));
}

/// The largest whole number a JSON field carries: what a JavaScript number
/// holds exactly (relay.md → Joining).
pub const MAX_JSON_INT: u64 = (1 << 53) - 1;

impl Limits {
    /// How long a channel nobody is on keeps its buffer and its fence:
    /// a ticket's lifetime, the skew and a margin, so no ticket from before
    /// the lease last moved outlives the fence that refuses it.
    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.ticket_lifetime + self.ticket_skew + self.idle_margin)
    }
}

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
    /// Where each channel's admission facts — its workspace, its fence and
    /// that fence's ticket expiry — are kept across restarts, so a restart
    /// never lets a displaced lease holder host again (relay.md → Tickets).
    /// None keeps them in memory, for loopback only (`krowk relay serve`
    /// refuses another address without `--state`).
    pub state: Option<std::path::PathBuf>,
    /// A host's direct listener (R-NET-1): the origins it may be dialed as,
    /// its tailnet address, its MagicDNS name and its LAN address, each the
    /// host published sealed to its viewers. The request's `Host` picks one
    /// and anything else is refused, so one listener answers every name
    /// without trusting a `Host` it did not publish. Empty: `origin` rules.
    pub origins: Vec<String>,
    /// The optional tailnet hardening (R-NET-3): a connection whose far end
    /// tailscaled does not name as this tailnet user is closed before the
    /// upgrade. The ticket and the challenge still decide who joins.
    pub whois: Option<crate::sync::tailscale::SameUser>,
}

/// Runs a relay on `listener` until the process ends.
pub fn run(listener: std::net::TcpListener, config: Config) -> Result<(), String> {
    // The state is read, strictly, before anything is served: a relay that
    // cannot trust its fences does not start.
    let state = match config.state.as_deref() {
        Some(dir) => Some(open_state(dir, &config.limits).map_err(|StateRefused(why)| why)?),
        None => None,
    };
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let listener = TcpListener::from_std(listener).map_err(|e| e.to_string())?;
        serve_with(listener, config, state).await;
        Ok(())
    })
}

/// Runs a relay on every one of `listeners` until `stop` turns true: a
/// host's direct listener (R-NET-1), which goes with the bridge that runs
/// it. Every connection it holds closes as it stops.
pub fn run_until(listeners: Vec<std::net::TcpListener>, config: Config, mut stop: tokio::sync::watch::Receiver<bool>) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let relay = Rc::new(Relay { config, channels: RefCell::new(HashMap::new()), pending: RefCell::new(VecDeque::new()), pending_ids: Cell::new(0), links: Cell::new(0) });
        for l in listeners {
            l.set_nonblocking(true).map_err(|e| e.to_string())?;
            let l = TcpListener::from_std(l).map_err(|e| e.to_string())?;
            let relay = relay.clone();
            tokio::task::spawn_local(async move {
                loop {
                    match l.accept().await {
                        Ok((stream, peer)) => {
                            tokio::task::spawn_local(admitted(stream, peer, relay.clone()));
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                    }
                }
            });
        }
        let _ = stop.wait_for(|s| *s).await;
        Ok(())
    })
}

/// The tailnet check, when there is one, then the connection.
async fn admitted(stream: TcpStream, peer: std::net::SocketAddr, relay: Rc<Relay>) {
    if let Some(same) = relay.config.whois.clone() {
        if !tokio::task::spawn_blocking(move || same.admits(peer)).await.unwrap_or(false) {
            record("join", None, None, "not_same_tailnet_user");
            return;
        }
    }
    connection(stream, peer.ip(), relay).await
}

/// Accepts connections for ever. Inside a `LocalSet`.
pub async fn serve(listener: TcpListener, config: Config) {
    serve_with(listener, config, None).await
}

async fn serve_with(listener: TcpListener, config: Config, state: Option<(std::fs::File, HashMap<Key, Channel>)>) {
    // The lock is held for as long as the relay serves.
    let (_lock, channels) = match state {
        Some((lock, channels)) => (Some(lock), channels),
        None => (None, HashMap::new()),
    };
    let relay = Rc::new(Relay { config, channels: RefCell::new(channels), pending: RefCell::new(VecDeque::new()), pending_ids: Cell::new(0), links: Cell::new(0) });
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::task::spawn_local(admitted(stream, peer, relay.clone()));
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
    channels: RefCell<HashMap<Key, Channel>>,
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
    session: Option<Key>,
    /// The device and role its ticket names, once the upgrade carried a
    /// good one: a device's viewer tickets never take its host's places.
    device: Option<([u8; 16], u8)>,
    kill: Rc<tokio::sync::Notify>,
}

/// The address a pre-request cap counts by: an IPv4 address itself, an
/// IPv6 one by its /64, which one subscriber typically holds whole.
fn peer_bucket(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip.to_canonical() {
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            std::net::IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
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

    /// At the pool's cap with every connection ticketed: one goes from the
    /// channel holding the most pending — a viewer's before a host's, the
    /// oldest first — so no workspace filling the pool with its own
    /// tickets, across any number of its sessions, pushes out a device of a
    /// channel holding fewer (relay.md → Limits). As a Durable Object holds
    /// each channel to its own cap, this holds the pool to the fullest.
    fn evict_from_fullest(&self) {
        let mut counts: HashMap<Key, usize> = HashMap::new();
        for p in self.pending.borrow().iter().filter(|p| p.device.is_some()) {
            if let Some(k) = p.session {
                *counts.entry(k).or_default() += 1;
            }
        }
        let Some((&fullest, _)) = counts.iter().max_by_key(|(_, n)| **n) else { return };
        let viewer = self.pending.borrow().iter().any(|p| p.session == Some(fullest) && p.device.is_some_and(|(_, r)| r == RELAY_ROLE_VIEWER));
        if viewer {
            self.evict(|p| p.session == Some(fullest) && p.device.is_some_and(|(_, r)| r == RELAY_ROLE_VIEWER));
        } else {
            self.evict(|p| p.session == Some(fullest) && p.device.is_some());
        }
    }

    /// A new pending connection, room made for it under the caps.
    ///
    /// Two pools: connections that have shown no ticket yet (here, 1024,
    /// 32 from one address, the oldest let go) and ones whose ticket was
    /// verified (`names`), so no number of strangers holding connections
    /// open, from any number of addresses, pushes out a device answering
    /// its challenge, and no number of tickets crowds out a stranger's
    /// honest upgrade either.
    fn arrive(&self, peer: std::net::IpAddr) -> (u64, Rc<tokio::sync::Notify>) {
        let limits = &self.config.limits;
        let peer = peer_bucket(peer);
        if self.pending.borrow().iter().filter(|p| p.peer == peer && p.device.is_none()).count() >= limits.unjoined_per_peer {
            self.evict(|p| p.peer == peer && p.device.is_none());
        }
        if self.pending.borrow().iter().filter(|p| p.device.is_none()).count() >= limits.unjoined {
            self.evict(|p| p.device.is_none());
        }
        let id = self.pending_ids.get() + 1;
        self.pending_ids.set(id);
        let kill = Rc::new(tokio::sync::Notify::new());
        self.pending.borrow_mut().push_back(Pending { id, peer, session: None, device: None, kill: kill.clone() });
        (id, kill)
    }

    /// Its channel, once the upgrade names it: room made there too.
    /// Its channel and device, once the upgrade carried a ticket the relay
    /// verified: room made there, first among that device's own pending
    /// connections, so a replayed ticket costs only the device it names and
    /// no stranger (who has no ticket) is ever counted on a channel.
    fn names(&self, id: u64, session: Key, device: [u8; 16], role: u8) {
        let limits = &self.config.limits;
        let device = (device, role);
        if self.pending.borrow().iter().filter(|p| p.session == Some(session) && p.device == Some(device)).count() >= limits.unjoined_per_device {
            self.evict(|p| p.session == Some(session) && p.device == Some(device));
        }
        if self.pending.borrow().iter().filter(|p| p.session == Some(session)).count() >= limits.unjoined_per_channel {
            self.evict(|p| p.session == Some(session));
        }
        if self.pending.borrow().iter().filter(|p| p.device.is_some()).count() >= limits.unjoined_ticketed {
            self.evict_from_fullest();
        }
        if let Some(p) = self.pending.borrow_mut().iter_mut().find(|p| p.id == id) {
            p.session = Some(session);
            p.device = Some(device);
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
    /// The workspace of the first join it admitted.
    workspace: Option<String>,
    /// The highest fence a host has been admitted at: a ticket from before
    /// the lease moved on is refused.
    fence: Option<u64>,
    /// The latest expiry of a ticket admitted at that fence, on the
    /// registry's clock: every ticket from before the lease moved on
    /// expires no later, so the channel keeps its fence at least until the
    /// relay's own clock has passed it (relay.md → Tickets).
    fence_exp: u64,
    /// The device a host was admitted as at that fence. One lease at one
    /// fence has one holder, so another device at an equal fence is not
    /// the lease holder, whatever its ticket says (relay.md → Tickets).
    fence_device: Option<[u8; 16]>,
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
    /// The env it joined under, recorded with it.
    env: Env,
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
    let Some((ws, key, device, role, join)) = joined else { return };
    let session = key;
    left.set(usize::MAX);
    let (tx, rx) = mpsc::unbounded_channel();
    let id = relay.links.get().max(limits.first_link) + 1;
    relay.links.set(id);
    let link = Rc::new(Link { id, device, env: key.0, tx, queued: Rc::default(), limit: relay.config.limits.link_queue });
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
async fn handshake(stream: Metered, relay: &Rc<Relay>, pending: u64, kill: &tokio::sync::Notify) -> Option<(Ws, Key, DeviceId, u8, Join)> {
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    let _ = stream.inner.set_nodelay(true);
    let limits = &relay.config.limits;
    let config = WebSocketConfig::default().max_message_size(Some(limits.max_message)).max_frame_size(Some(limits.max_message));
    let seen: Rc<RefCell<Dialed>> = Rc::default();
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
        // The env it is dialed under routes it; one the relay does not know
        // is refused as a bad join once the challenge is out.
        let env = Env::parse(req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("env="))));
        // The ticket rides the upgrade (`X-Krowk-Ticket`), never the URL.
        // Exactly one, or it is no ticket at all: two are refused, not
        // read by which comes first.
        let all: Vec<_> = req.headers().get_all("x-krowk-ticket").iter().collect();
        let ticket = match all.as_slice() {
            [] => None,
            [one] => Some(one.to_str().map(str::to_string).unwrap_or_else(|_| "\u{0}".into())),
            _ => Some("\u{0}duplicate".into()),
        };
        *into.borrow_mut() = (Some(session), host, env, ticket);
        Ok(resp)
    };
    let upgrade = tokio::time::timeout(limits.upgrade_wait, tokio_tungstenite::accept_hdr_async_with_config(stream, route, Some(config)));
    let mut ws = tokio::select! {
        _ = kill.notified() => return None,
        r = upgrade => r.ok()?.ok()?,
    };
    let (session, host, env, ticket) = seen.borrow().clone();
    let session = session?;
    // The ticket is checked before the connection counts anywhere: a
    // connection without a good one for this session and env is refused
    // at once, before any challenge, and takes no channel's room.
    let ticket = match upgrade_ticket(relay, session, env, ticket.as_deref()) {
        Ok(t) => t,
        Err(r) => {
            refused(&mut ws, env, &r).await;
            return None;
        }
    };
    let env = env.expect("checked with the ticket");
    relay.names(pending, (env, session), ticket.1.device, ticket.1.role);
    let result = challenge(&mut ws, relay, session, Some(env), &host, ticket, kill).await;
    let (device, role, join) = result?;
    Some((ws, (env, session), device, role, join))
}

/// The upgrade's ticket: present, the registry's, alive, for this session
/// and env (relay.md → Joining).
fn upgrade_ticket(relay: &Relay, session: [u8; 16], env: Option<Env>, hex: Option<&str>) -> Result<(String, krowk_client::relay_ticket::Ticket), Refusal> {
    let again = "join with the ticket the registry issued for this device, session, role and env";
    let Some(env) = env else {
        return Err(refuse("bad_join", "the URL's env is not \"production\" or \"development\"", "dial ?env=production, ?env=development, or neither for production"));
    };
    let Some(hex) = hex.filter(|h| !h.is_empty()) else {
        return Err(refuse("bad_ticket", "the upgrade carries no ticket", "send the registry's relay ticket in the X-Krowk-Ticket header of the upgrade"));
    };
    let limits = &relay.config.limits;
    let t = krowk_client::relay_ticket::verify_within(hex, &relay.config.roster.keys, krowk_client::relay_ticket::now(), limits.ticket_lifetime, limits.ticket_skew)
        .map_err(|r| refuse(r.code, r.message, if r.code == "ticket_expired" { "ask the registry for a fresh ticket, then join again" } else { again }))?;
    let code = if env == Env::Production { krowk_client::relay_ticket::ENV_PRODUCTION } else { krowk_client::relay_ticket::ENV_DEVELOPMENT };
    if t.session != session {
        return Err(refuse("bad_ticket", "the ticket is for another session", again));
    }
    if t.env != code {
        return Err(refuse("bad_ticket", "the ticket is for another env", again));
    }
    Ok((hex.to_string(), t))
}

/// A refusal before the join, recorded.
async fn refused(ws: &mut Ws, env: Option<Env>, r: &Refusal) {
    record("join", env, None, r.code);
    close(ws, r).await;
}

/// The challenge, and the join that answers it.
async fn challenge(ws: &mut Ws, relay: &Rc<Relay>, session: [u8; 16], env: Option<Env>, host: &str, ticket: (String, krowk_client::relay_ticket::Ticket), kill: &tokio::sync::Notify) -> Option<(DeviceId, u8, Join)> {
    let limits = &relay.config.limits;
    let given = relay.config.origin.clone().unwrap_or_else(|| format!("ws://{host}"));
    let listed = relay.config.origins.iter().filter_map(|o| e2e::canonical_origin(o)).collect::<Vec<_>>();
    if !listed.is_empty() && !e2e::canonical_origin(&given).is_some_and(|o| listed.contains(&o)) {
        refused(ws, env, &refuse("bad_origin", format!("{given:?} is not an address this host published"), "dial one of the candidate addresses the host sent")).await;
        return None;
    }
    let Some(origin) = e2e::canonical_origin(&given) else {
        refused(ws, env, &refuse("bad_origin", format!("{given:?} is no origin a device can sign"), "dial the relay by a host name or address, or start it with --origin")).await;
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
                refused(ws, env, &refuse("join_timeout", "the oldest connection not yet joined made way for a newer one", "send the join as soon as the challenge arrives")).await;
                return None;
            }
            r = tokio::time::timeout_at(deadline, ws.next()) => r,
        };
        match next {
            Err(_) => {
                refused(ws, env, &refuse("join_timeout", "no join arrived in time", "send the join within ten seconds of the challenge")).await;
                return None;
            }
            Ok(Some(Ok(Message::Binary(b)))) => break b,
            Ok(Some(Ok(Message::Text(t)))) if t.as_str() == "ping" => {
                pings += 1;
                if pings > limits.prejoin_pings {
                    refused(ws, env, &refuse("rate_limited", "more heartbeats than a join takes to sign", "join, then beat")).await;
                    return None;
                }
                let _ = ws.send(Message::Text("pong".into())).await;
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(Some(Err(_))) => {
                refused(ws, env, &refuse("too_large", format!("at most {} bytes may come before the join", limits.prejoin_bytes), "send the join, at most 64 KiB, and nothing larger before it")).await;
                return None;
            }
            Ok(Some(Ok(_))) => {
                refused(ws, env, &refuse("not_joined", "the first message after the challenge must be a join", "answer the challenge with a join control message")).await;
                return None;
            }
            Ok(_) => return None,
        }
    };
    let parsed = if join.len() > limits.max_control { Err(refuse("too_large", "a join is at most 64 KiB", "send a join, nothing larger")) } else { read_join(&join) };
    let role = parsed.as_ref().ok().map(|j| j.role);
    let checked = parsed.and_then(|j| {
        // The join names the env it was dialed under, absent meaning
        // production on both sides; anything else is not a join.
        match (env, j.env) {
            (Some(dialed), Some(said)) if dialed == said => Ok(j),
            _ => Err(refuse("bad_join", "the join's env is not \"production\" or \"development\", or not the one its URL was dialed with", "dial ?env= and send env alike: \"production\" or \"development\", or neither for production")),
        }
    });
    match checked.and_then(|j| admit(relay, (env.unwrap_or(Env::Production), session), &nonce, &origin, j, &ticket)) {
        Err(r) => {
            record("join", env, role, r.code);
            close(ws, &r).await;
            None
        }
        Ok(found) => {
            record("join", env, Some(found.1), "joined");
            Some(found)
        }
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
    /// `env`: None for a value that is neither name, refused once the
    /// dialed env is known to compare it with.
    env: Option<Env>,
    /// The session's workspace, once admitted: what its channel holds to.
    workspace: String,
    /// The registry's ticket, hex, when the join repeats the upgrade's.
    ticket: Option<String>,
    /// A host's ticket's fence, once admitted.
    ticket_fence: u64,
    ticket_exp: u64,
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
    let env = match v.get("env") {
        None | Some(Value::Null) => Env::parse(None),
        Some(Value::String(e)) => Env::parse(Some(e)),
        Some(_) => None,
    };
    let ticket = match v.get("ticket") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) if t.len() <= 512 => Some(t.clone()),
        Some(_) => return Err(bad("has a ticket that is not a hex string")),
    };
    Ok(Join { role, device, signature, fence, stream, after, env, workspace: String::new(), ticket, ticket_fence: 0, ticket_exp: 0 })
}

/// Who may join (relay.md → What the relay checks): a device with a
/// ticket the registry signed for this session, env and role, answering the
/// challenge with the signing key the ticket names; and to host, the lease
/// holder's ticket, at a fence no lower than the channel has seen. Nothing
/// is asked of anyone: the ticket carries what the registry vouched for,
/// so a stranger costs a signature check and nothing more.
fn admit(relay: &Relay, key: Key, nonce: &[u8; 32], origin: &str, mut j: Join, ticket: &(String, krowk_client::relay_ticket::Ticket)) -> Result<(DeviceId, u8, Join), Refusal> {
    let session = key.1;
    // The ticket the upgrade carried, verified then; a join may repeat it,
    // and then it must be the same one.
    let (hex, t) = ticket;
    if j.ticket.as_ref().is_some_and(|given| !given.eq_ignore_ascii_case(hex)) {
        return Err(refuse("bad_ticket", "the join names another ticket than the upgrade carried", "send the upgrade's ticket, or none, in the join"));
    }
    if krowk_client::relay_ticket::now() >= t.exp {
        return Err(refuse("ticket_expired", "the ticket expired before the join", "ask the registry for a fresh ticket, then join again"));
    }
    let env = if key.0 == Env::Production { krowk_client::relay_ticket::ENV_PRODUCTION } else { krowk_client::relay_ticket::ENV_DEVELOPMENT };
    let bad = |what: &str| refuse("bad_ticket", format!("the ticket is for {what}"), "join with the ticket the registry issued for this device, session, role and env");
    if t.session != session {
        return Err(bad("another session"));
    }
    if t.env != env {
        return Err(bad("another env"));
    }
    if t.device != j.device.0 {
        return Err(bad("another device"));
    }
    if t.role == RELAY_ROLE_HOST && j.role == RELAY_ROLE_VIEWER {
        return Err(bad("hosting, and this join is a viewer's"));
    }
    if e2e::SigningPublic(t.signing_key).verify_relay_join(&j.signature, j.role, &session, nonce, &j.device, origin).is_err() {
        return Err(refuse("bad_signature", "the join's signature does not verify under the signing key the ticket names", "sign this challenge's nonce with the device's own signing key, for the role, session and relay origin you dialed"));
    }
    // A channel holds to the workspace it was first joined under, so no
    // other workspace's session of the same id ever shares it; the ticket
    // says which workspace the registry issued it for.
    let pinned = relay.channels.borrow().get(&key).and_then(|c| c.workspace.clone());
    if pinned.as_ref().is_some_and(|w| *w != t.workspace) {
        return Err(refuse("unknown_session", "the relay has no such session for this device", "open a session of your own workspace that syncs; its lease names the host"));
    }
    j.workspace = t.workspace.clone();
    let limits = &relay.config.limits;
    let mut channels = relay.channels.borrow_mut();
    let channel = channels.entry(key).or_default();
    if !channel.device_joins.entry(j.device.0).or_default().take(limits.joins_per_device) {
        return Err(refuse("rate_limited", format!("device {} joined this session more than {} times this minute", j.device, limits.joins_per_device), "wait a minute, and reconnect with backoff"));
    }
    if j.role == RELAY_ROLE_HOST {
        // A viewer's ticket, or a lease the channel has since seen move on:
        // the registry said this device held the lease once, and a host
        // with a higher fence has joined since.
        let other_at_fence = channel.fence == Some(t.fence) && channel.fence_device.is_some_and(|d| d != t.device);
        if t.role != RELAY_ROLE_HOST || channel.fence.is_some_and(|f| t.fence < f) || other_at_fence {
            return Err(refuse("not_lease_holder", "only the device holding the session's lease may host it, and the lease has moved on from this ticket's", "take the session's lease (krowk resumes it on this device), or join as a viewer"));
        }
        if j.fence != Some(t.fence) {
            return Err(refuse("stale_lease", format!("the ticket's fence is {}, not {}", t.fence, j.fence.map_or("absent".into(), |f| f.to_string())), "send the fence of the lease the ticket came with"));
        }
        if j.stream.is_none() {
            return Err(refuse("bad_join", "a host's join names no stream", "send the stream epoch the host seals batches under, 32 hex characters"));
        }
        j.ticket_fence = t.fence;
        j.ticket_exp = t.exp;
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
fn enter(relay: &Relay, session: Key, link: &Rc<Link>, role: u8, j: Join) {
    let mut channels = relay.channels.borrow_mut();
    // A channel nobody has been on for a while is forgotten, its buffer
    // with it: a host coming back starts it afresh.
    let limits = &relay.config.limits;
    let now = krowk_client::relay_ticket::now();
    channels.retain(|id, c| *id == session || !forgettable(c, limits, now));
    let ch = channels.entry(session).or_default();
    ch.empty_since = None;
    ch.workspace.get_or_insert_with(|| j.workspace.clone());
    if role == RELAY_ROLE_HOST {
        match ch.fence {
            Some(f) if f > j.ticket_fence => {}
            Some(f) if f == j.ticket_fence => ch.fence_exp = ch.fence_exp.max(j.ticket_exp),
            _ => {
                ch.fence = Some(j.ticket_fence);
                ch.fence_exp = j.ticket_exp;
                ch.fence_device = Some(link.device.0);
            }
        }
        // Kept before the host is told it joined, so a restart after it
        // cannot let a displaced holder in.
        if let Some(dir) = &relay.config.state {
            save_state(dir, session, ch);
        }
    }
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
async fn read(mut incoming: futures_util::stream::SplitStream<Ws>, relay: Rc<Relay>, session: Key, link: Rc<Link>, role: u8) {
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
        record("refusal", Some(link.env), Some(role), r.code);
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

fn from_host(relay: &Relay, key: Key, link: &Rc<Link>, e: Envelope, raw: &[u8], sent: &mut u64) -> Result<(), Refusal> {
    let session = key.1;
    let mut channels = relay.channels.borrow_mut();
    let ch = channels.get_mut(&key).expect("joined");
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

fn from_viewer(relay: &Relay, key: Key, link: &Rc<Link>, e: Envelope, raw: &[u8]) -> Result<(), Refusal> {
    let session = key.1;
    let mut channels = relay.channels.borrow_mut();
    let ch = channels.get_mut(&key).expect("joined");
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

fn leave(relay: &Relay, session: Key, link: &Rc<Link>, role: u8) {
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

/// Whether a channel nobody is on may be forgotten, its fence with it: only
/// once it has been empty for the idle time *and* the relay's own clock has
/// passed its fence's ticket expiry by the margin, so every ticket from
/// before the lease last moved is expired by the relay's own check —
/// whatever the registry's clock reads against the relay's.
fn forgettable(c: &Channel, limits: &Limits, now: u64) -> bool {
    c.empty_since.is_some_and(|t| t.elapsed() >= limits.idle()) && now >= c.fence_exp + limits.idle_margin
}

/// The admission facts of every channel that has any, one JSON line each,
/// the last line for a channel the one that counts.
const STATE_FILE: &str = "relay-channels.jsonl";
const STATE_LOCK: &str = "relay.lock";

fn state_line(key: Key, ch: &Channel) -> String {
    json!({"env": key.0.as_str(), "session": e2e::hex(&key.1), "workspace": ch.workspace, "fence": ch.fence, "fenceExp": ch.fence_exp, "fenceDevice": ch.fence_device.map(|d| e2e::hex(&d))}).to_string() + "\n"
}

fn save_state(dir: &std::path::Path, key: Key, ch: &Channel) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let written = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join(STATE_FILE)).and_then(|mut f| {
        f.write_all(state_line(key, ch).as_bytes())?;
        f.sync_data()
    });
    if let Err(e) = written {
        // Not kept is not safe: the relay stops rather than carry on
        // with a fence a restart would forget.
        eprintln!("relay state {}: {e}; stopping", dir.display());
        std::process::exit(1);
    }
}

/// Why the state under `--state` cannot be trusted: the relay refuses to
/// start rather than run as if it knew no fence (relay.md → Tickets).
#[derive(Debug)]
pub struct StateRefused(pub String);

/// Opens `--state DIR` for this relay alone: the directory made 0700, a
/// lock held on it for as long as the relay runs (two relays on one state
/// would each forget the other's fences), and the admission facts read
/// strictly. Any read error but a missing file, any byte that is not
/// UTF-8, any line that is not a whole record (a last one cut off
/// mid-append included), and a rewrite left unfinished refuse to start,
/// each with what to do, and nothing is rewritten after a refusal. Records past both their idle time and their fence's expiry
/// are pruned, and the rest written back durably: a temporary file,
/// synced, renamed over the old, and the directory synced.
fn open_state(dir: &std::path::Path, limits: &Limits) -> Result<(std::fs::File, HashMap<Key, Channel>), StateRefused> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let fail = |what: String| StateRefused(format!("--state {}: {what}", dir.display()));
    std::fs::create_dir_all(dir).map_err(|e| fail(format!("cannot be made: {e}")))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| fail(format!("cannot be made private (0700): {e}")))?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(STATE_LOCK)).map_err(|e| fail(format!("its lock cannot be opened: {e}")))?;
    // SAFETY: flock on a file descriptor this process owns.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(fail("another relay holds it; give each relay its own state".into()));
    }
    let path = dir.join(STATE_FILE);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    if tmp.exists() {
        return Err(fail(format!("a rewrite of {STATE_FILE} was cut off (its .tmp is there); {STATE_FILE} itself is whole — remove {STATE_FILE}.tmp and start again")));
    }
    let raw = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(fail(format!("{STATE_FILE} cannot be read ({e}); the relay will not start without its fences — restore the file or its permissions (0600, the relay's own)"))),
    };
    let text = String::from_utf8(raw).map_err(|_| fail(format!("{STATE_FILE} is not text; the relay will not start without its fences — restore it from a backup, or remove it only once every ticket it could refuse has expired (seven minutes after the relay last ran)")))?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut channels: HashMap<Key, Channel> = HashMap::new();
    for (n, line) in lines.iter().enumerate() {
        let record = line.strip_suffix('\n').and_then(parse_state_line);
        let Some((key, workspace, fence, fence_exp, fence_device)) = record else {
            let torn = n + 1 == lines.len() && !line.ends_with('\n');
            let fix = if torn {
                "a crash cut the last record off mid-append: remove that last line (the host it was for was never told it joined) and start again".to_string()
            } else {
                "restore the file, or remove it only once every ticket it could refuse has expired (seven minutes after the relay last ran)".to_string()
            };
            return Err(fail(format!("{STATE_FILE} line {} is not a whole record, so the relay will not start without its fences — {fix}", n + 1)));
        };
        let ch = channels.entry(key).or_default();
        ch.workspace = Some(workspace);
        ch.fence = Some(fence);
        ch.fence_exp = fence_exp;
        ch.fence_device = fence_device;
        ch.empty_since = Some(Instant::now());
    }
    // Pruned: a channel whose fence's tickets have all expired past the
    // margin would be forgotten after the idle time anyway, and the idle
    // time starts again at every start.
    let now = krowk_client::relay_ticket::now();
    channels.retain(|_, c| now < c.fence_exp + limits.idle_margin + limits.idle().as_secs());
    let compact: String = channels.iter().map(|(k, c)| state_line(*k, c)).collect();
    let durable = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp).and_then(|mut f| {
        f.write_all(compact.as_bytes())?;
        f.sync_all()
    });
    durable
        .and_then(|_| std::fs::rename(&tmp, &path))
        .and_then(|_| std::fs::File::open(dir)?.sync_all())
        .map_err(|e| fail(format!("cannot be rewritten durably: {e}")))?;
    let _ = std::io::stderr().flush();
    Ok((lock, channels))
}

type StateRecord = (Key, String, u64, u64, Option<[u8; 16]>);

/// One whole record: every field present and of its type.
fn parse_state_line(line: &str) -> Option<StateRecord> {
    let v: Value = serde_json::from_str(line).ok()?;
    let env = Env::parse(Some(v["env"].as_str()?))?;
    let session: [u8; 16] = e2e::unhex(v["session"].as_str()?)?.try_into().ok()?;
    let workspace = v["workspace"].as_str()?.to_string();
    let fence = v["fence"].as_u64()?;
    let fence_exp = v["fenceExp"].as_u64()?;
    let fence_device = match &v["fenceDevice"] {
        Value::Null => None,
        d => Some(e2e::unhex(d.as_str()?)?.try_into().ok()?),
    };
    Some(((env, session), workspace, fence, fence_exp, fence_device))
}

/// A session id's sixteen bytes, from its canonical UUID form.
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if s.len() != 36 || hex.len() != 32 {
        return None;
    }
    e2e::unhex(&hex.to_ascii_lowercase())?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::peer_bucket;

    /// R-RELAY-1: the pre-request cap counts an IPv6 peer by its /64 and an
    /// IPv4 one, v4-mapped included, by itself.
    #[test]
    fn r_relay_1_an_ipv6_peer_is_counted_by_its_64() {
        let b = |s: &str| peer_bucket(s.parse().unwrap());
        assert_eq!(b("2001:db8::1"), b("2001:db8::ffff"));
        assert_eq!(b("2001:db8::1"), b("2001:db8:0:0:abcd:1:2:3"));
        assert_ne!(b("2001:db8::1"), b("2001:db8:0:1::1"));
        assert_eq!(b("::ffff:1.2.3.4"), b("1.2.3.4"));
        assert_ne!(b("1.2.3.4"), b("1.2.3.5"));
    }
}
