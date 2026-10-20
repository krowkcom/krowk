//! Another device's side of a synced session: listing what is synced from
//! the sealed indexes, attaching from the latest checkpoint and the tail,
//! and then following the host live over the relay, sending it commands.
//!
//! **Read-only without a host.** A viewer never writes the log and never
//! becomes the host (R-SYNC-2, R-HAND-4). With no host on the channel it
//! shows what the chunks hold, and a prompt typed then is queued here and
//! sent when a host arrives and welcomes it.
//!
//! **Optimistic, reconciled** (R-LAG-8). A command is shown at once as
//! `Update::Sent` and settled by the host's ack (`Update::Acked`), by the
//! viewer's own id. **One batch a frame** (R-LAG-7): whatever arrives in one
//! display frame is handed on as one `Vec<Update>`, never a message a batch.
//!
//! The viewer itself is `krowk_client::sync::viewer::ViewerCore`, the one
//! the phone links too (R-CLIENT-1): this file is its tokio driver. The
//! sockets, the registry calls, the clock and the race for a direct path
//! (with mDNS) are the harness's; what is shown, sent and when is the core's.

use super::store::{self, Head};
use super::direct::Candidate;
use super::Join;
use crate::protocol::Command;
use futures_util::StreamExt;
use krowk_api::Client;
use krowk_client::e2e::{self, DeviceId, SessionKeys, SigningKey};
use krowk_client::device_chain::Chain;
use krowk_client::session_record::Signer;
use krowk_client::user_key::UserKeys;
use krowk_client::sync::viewer::{Effect, ViewerCore};
pub use krowk_client::sync::viewer::{Update, HELD_MAX, PROBE, REPROBE, REPROBE_MAX};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// A synced session as `krowk sessions` lists it, from its sealed index.
#[derive(Debug, Clone)]
pub struct Listed {
    pub id: String,
    pub index: store::Index,
    pub holder: Option<String>,
    /// The device whose signature the session's record carries.
    pub signer: String,
}

/// Every synced session this device can open, most recently written first.
/// One whose record no device on the verified list signed, or whose key or
/// index does not open under the user keys this device holds — one sealed under a generation newer than it has, say — is left
/// out and counted.
pub fn list(api: &Client, keys: &UserKeys, chain: &Chain) -> Result<(Vec<Listed>, usize), String> {
    let mut out = Vec::new();
    let mut unreadable = 0;
    let mut before = String::new();
    loop {
        let page = api.list_sync_sessions(&before, 100).map_err(|e| e.to_string())?;
        for s in &page.sessions {
            // A listing leaves the index out; `show` has it.
            let full = api.show_sync_session(&s.id).map_err(|e| e.to_string())?;
            match store::open_session_key(&full, &s.id, keys, chain, Signer::EverHeld).and_then(|k| store::open_index(&k, &s.id, &full.sealed_index)) {
                Ok(index) => out.push(Listed { id: s.id.clone(), index, holder: full.lease.map(|l| l.device), signer: full.signer.clone() }),
                Err(_) => unreadable += 1,
            }
        }
        if page.next.is_empty() {
            return Ok((out, unreadable));
        }
        before = page.next;
    }
}

pub struct Options {
    pub relay: String,
    pub env: String,
    /// Signed by this device (`Client::signed_by` with an
    /// `e2e::DeviceSigner` of `device` and `signing`): its lease, chunk,
    /// index and relay-ticket calls act as the device, and a registry
    /// refuses them unsigned.
    pub api: Arc<Client>,
    pub device: DeviceId,
    pub signing: SigningKey,
    /// The person's user key, by the generations this device holds.
    pub keys: UserKeys,
    /// The device list as this device verified it: whose signatures a
    /// session record is checked against.
    pub chain: Chain,
    pub session: String,
    /// The highest head this device has seen of the session before: a log
    /// served shorter is refused.
    pub known: Option<Head>,
}

/// A running viewer: `send` a command, read `updates` a frame at a time.
pub struct Viewer {
    pub commands: mpsc::UnboundedSender<Command>,
    /// Asks the host to hand the session to this device, once welcomed;
    /// `Update::Handoff` answers.
    pub handoff: mpsc::UnboundedSender<()>,
    pub updates: mpsc::Receiver<Vec<Update>>,
    /// How long reading the checkpoint and the tail took.
    pub attach_time: Duration,
    /// When each `Vec` of updates was handed on (R-LAG-7's measure).
    pub handed: Arc<std::sync::Mutex<Vec<Instant>>>,
    /// The session's title, from its sealed index; empty when it has none.
    pub title: String,
    /// The name the verified device list gives the device that signed the
    /// session record — the one that hosts it — when the list holds it.
    pub host: Option<String>,
}

/// Attaches: the sealed index, the checkpoint and the tail (off the
/// thread), then the relay. Answers once the session as stored is read.
pub async fn attach(o: Options) -> Result<Viewer, String> {
    let started = Instant::now();
    let o = Arc::new(o);
    let (key, attached, host) = {
        let o = o.clone();
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            let s = o.api.show_sync_session(&o.session).map_err(|e| e.to_string())?;
            let key = store::open_session_key(&s, &o.session, &o.keys, &o.chain, Signer::EverHeld)?;
            let index = store::open_index(&key, &o.session, &s.sealed_index)?;
            let a = store::attach(&o.api, &key, &o.session, index, o.known)?;
            // `open_session_key` checked the signer against this list already.
            let host = e2e::DeviceId::parse(&s.signer).and_then(|id| o.chain.devices().iter().find(|d| d.id() == id).map(|d| d.name.clone()));
            Ok((key, a, host))
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let attach_time = started.elapsed();
    let (commands, rx) = mpsc::unbounded_channel();
    let (handoff, handoff_rx) = mpsc::unbounded_channel();
    let (tx, updates) = mpsc::channel(256);
    let handed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let title = attached.index.title.clone();
    let start = tokio::time::Instant::now();
    let (core, first) = ViewerCore::new(key.clone(), &o.session, o.device, &o.relay, attached, 0);
    let (won_tx, won) = mpsc::unbounded_channel();
    let mut d = Driver { o, key, core, start, ws: None, leaving: None, pending: None, out: tx, handed: handed.clone(), won_tx };
    // The session as read at rest goes out before `attach` answers, and
    // the first join is set for now.
    if d.carry_out(first).await {
        tokio::spawn(d.run(rx, handoff_rx, won));
    }
    Ok(Viewer { commands, handoff, updates, attach_time, handed, title, host })
}

/// A race's end: the first candidate that joined, its connection and
/// `joined`, or None when none did.
type Won = Option<(Candidate, super::Ws, serde_json::Value)>;

/// What came on a connection, as `ViewerCore` takes it.
enum Msg {
    Binary(Vec<u8>),
    /// Anything else (a `pong`): the connection lives.
    Other,
    Closed,
}

/// The next message on `ws`; with none, never.
async fn next(ws: &mut Option<super::Ws>) -> Msg {
    let Some(w) = ws.as_mut() else { return std::future::pending().await };
    match w.next().await {
        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => Msg::Closed,
        Some(Ok(Message::Binary(b))) => Msg::Binary(b.to_vec()),
        Some(Ok(_)) => Msg::Other,
    }
}

/// The tokio side of `ViewerCore` (R-CLIENT-1): the sockets, the
/// registry calls off the runtime (R-LAG-9), the clock, and the race for
/// a direct path (R-NET-2). What the viewer shows, sends and when is the
/// core's.
struct Driver {
    o: Arc<Options>,
    key: SessionKeys,
    core: ViewerCore,
    /// The origin of `now_ms`.
    start: tokio::time::Instant,
    /// The connection the session is read from: the relay's, or a direct
    /// path's.
    ws: Option<super::Ws>,
    /// The relay's, while the session moves off it to a direct path.
    leaving: Option<super::Ws>,
    /// The join the core asked for and not yet run: its `extra` and when.
    pending: Option<(serde_json::Value, u64)>,
    out: mpsc::Sender<Vec<Update>>,
    handed: Arc<std::sync::Mutex<Vec<Instant>>>,
    won_tx: mpsc::UnboundedSender<Won>,
}

impl Driver {
    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// When `at_ms` is, on the runtime's clock; with none, never soon.
    fn at(&self, at_ms: Option<u64>) -> tokio::time::Instant {
        match at_ms {
            Some(ms) => self.start + Duration::from_millis(ms),
            None => self.start + Duration::from_secs(86_400 * 365),
        }
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>, mut handoff: mpsc::UnboundedReceiver<()>, mut won: mpsc::UnboundedReceiver<Won>) {
        loop {
            let wake = self.core.next_wake();
            let (woken, joining) = (self.at(wake), self.at(self.pending.as_ref().map(|p| p.1)));
            let effects = tokio::select! {
                c = commands.recv() => {
                    let Some(c) = c else { break };
                    self.core.command(c, self.now())
                }
                Some(()) = handoff.recv() => self.core.ask_handoff(self.now()),
                _ = tokio::time::sleep_until(woken), if wake.is_some() => self.core.tick(self.now()),
                _ = tokio::time::sleep_until(joining), if self.pending.is_some() => self.join().await,
                Some(w) = won.recv() => self.won(w),
                m = next(&mut self.leaving) => match m {
                    Msg::Binary(b) => self.core.leaving(&b, self.now()),
                    Msg::Other => Vec::new(),
                    Msg::Closed => { self.leaving = None; Vec::new() }
                },
                m = next(&mut self.ws) => match m {
                    Msg::Binary(b) => self.core.received(&b, self.now()),
                    Msg::Other => { self.core.alive(self.now()); Vec::new() }
                    Msg::Closed => { self.ws = None; self.core.closed(self.now()) }
                },
            };
            if !self.carry_out(effects).await {
                break;
            }
        }
    }

    /// The join the core asked for, now: a fresh ticket (off the runtime),
    /// then the relay. A ticket not had is asked again in a second.
    async fn join(&mut self) -> Vec<Effect> {
        let Some((extra, _)) = self.pending.take() else { return Vec::new() };
        let o = self.o.clone();
        let api = o.api.clone();
        let (id, device, env) = (o.session.clone(), o.device.to_string(), o.env.clone());
        let ticket = match tokio::task::spawn_blocking(move || api.relay_ticket(&id, &device, &env)).await {
            Ok(Ok(t)) => t.relay_ticket,
            _ => {
                self.pending = Some((extra, self.now() + 1000));
                return Vec::new();
            }
        };
        let j = Join { relay: &o.relay, session: &o.session, env: &o.env, ticket: &ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_VIEWER, extra };
        match super::join(j).await {
            Ok((w, joined)) => {
                self.ws = Some(w);
                self.core.joined(&joined, self.now())
            }
            Err(e) => self.core.join_failed(e, self.now()),
        }
    }

    /// A race's end. A candidate the core moves to (its hello the `Send`
    /// back) is the current connection from here, the relay's the leaving
    /// one; any other is dropped.
    fn won(&mut self, w: Won) -> Vec<Effect> {
        let Some((c, dw, joined)) = w else {
            self.core.probe_lost(self.now());
            return Vec::new();
        };
        let effects = self.core.move_to(c, &joined, self.now());
        if effects.iter().any(|e| matches!(e, Effect::Send(_))) {
            self.leaving = self.ws.take();
            self.ws = Some(dw);
        }
        effects
    }

    /// Carries out the core's effects, in order, and those they lead to.
    /// False once the viewer ends: the core said so, or nobody takes what
    /// it shows.
    async fn carry_out(&mut self, effects: Vec<Effect>) -> bool {
        let mut todo = VecDeque::from(effects);
        while let Some(e) = todo.pop_front() {
            match e {
                Effect::Send(b) => {
                    if let Some(w) = self.ws.as_mut() && !super::send(w, b).await {
                        self.ws = None;
                        todo.extend(self.core.closed(self.now()));
                    }
                }
                Effect::Ping => {
                    if let Some(w) = self.ws.as_mut() && !super::ping(w).await {
                        self.ws = None;
                        todo.extend(self.core.closed(self.now()));
                    }
                }
                Effect::Show(us) => {
                    // R-LAG-7's measure: when each frame was handed on.
                    self.handed.lock().unwrap_or_else(|e| e.into_inner()).push(Instant::now());
                    if self.out.send(us).await.is_err() {
                        return false;
                    }
                }
                // Read inline, as nothing else is acted on until it is in:
                // the core holds what comes meanwhile anyway.
                Effect::ReadChunks { want } => {
                    let (api, key, id) = (self.o.api.clone(), self.key.clone(), self.o.session.clone());
                    let mut a = self.core.attached().clone();
                    let unread = a.clone();
                    let read = tokio::task::spawn_blocking(move || store::catch_up(&api, &key, &id, &mut a, want).map(|f| (f, a))).await;
                    // A read that did not run read nothing new.
                    let read = read.unwrap_or_else(|_| Ok((Vec::new(), unread)));
                    todo.extend(self.core.chunks_read(read, self.now()));
                }
                Effect::Join { extra, at_ms } => {
                    (self.ws, self.leaving) = (None, None);
                    self.pending = Some((extra, at_ms));
                }
                Effect::Probe => {
                    tokio::spawn(race(self.o.clone(), self.core.candidates().to_vec(), self.core.cursor_extra(), self.won_tx.clone()));
                }
                Effect::Leave => self.leaving = None,
                Effect::End => return false,
            }
        }
        true
    }
}

/// Races every candidate: joins each, at the viewer's cursor, with a fresh
/// ticket, and hands on the first that joins, or None when none does. The
/// rest are dropped as it returns.
async fn race(o: Arc<Options>, candidates: Vec<Candidate>, extra: serde_json::Value, won: mpsc::UnboundedSender<Won>) {
    let api = o.api.clone();
    let (id, device, env) = (o.session.clone(), o.device.to_string(), o.env.clone());
    let Ok(Ok(ticket)) = tokio::task::spawn_blocking(move || api.relay_ticket(&id, &device, &env)).await else {
        let _ = won.send(None);
        return;
    };
    let ticket = ticket.relay_ticket;
    let mut set = tokio::task::JoinSet::new();
    let try_one = |set: &mut tokio::task::JoinSet<_>, c: Candidate| {
        let (o, ticket, extra) = (o.clone(), ticket.clone(), extra.clone());
        set.spawn(async move {
            let signing = SigningKey::from_secret(&*o.signing.secret_bytes()).map_err(|e| e.to_string())?;
            let j = Join { relay: &c.url, session: &o.session, env: &o.env, ticket: &ticket, device: o.device, signing: &signing, role: e2e::RELAY_ROLE_VIEWER, extra };
            let (w, joined) = tokio::time::timeout(PROBE, super::join(j)).await.map_err(|_| "timed out".to_string())??;
            Ok::<_, String>((c, w, joined))
        });
    };
    // A host that offers its LAN advertises it over mDNS too. Anyone on the
    // LAN may answer for it, and a decoy can say `joined` with the host
    // there as fast as it likes, so what is found there is tried only once
    // every address the sealed welcome named has failed: it finds a host
    // whose address changed, and never wins against one that has not.
    let (found_tx, mut found) = mpsc::unbounded_channel();
    if candidates.iter().any(Candidate::on_lan) {
        let session = crate::daemon::ws::uuid(&o.session);
        let loopback = candidates.iter().any(|c| c.url.starts_with("ws://127."));
        tokio::task::spawn_blocking(move || super::mdns::browse(session, PROBE, loopback, found_tx));
    } else {
        drop(found_tx);
    }
    let mut tried: HashSet<String> = candidates.iter().map(|c| c.url.clone()).collect();
    for c in candidates {
        try_one(&mut set, c);
    }
    let (mut browsing, mut welcomed_failed, mut waiting) = (true, false, Vec::new());
    loop {
        if welcomed_failed {
            for c in waiting.drain(..) {
                try_one(&mut set, c);
            }
        }
        tokio::select! {
            r = set.join_next(), if !set.is_empty() => {
                if let Some(Ok(Ok(w))) = r {
                    let _ = won.send(Some(w));
                    return;
                }
                welcomed_failed = welcomed_failed || set.is_empty();
            }
            c = found.recv(), if browsing => match c {
                Some(c) if tried.insert(c.url.clone()) => waiting.push(c),
                Some(_) => {}
                None => browsing = false,
            },
            else => break,
        }
    }
    let _ = won.send(None);
}
