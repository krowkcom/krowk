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

use super::store::{self, Attached, Head};
use super::direct::Candidate;
use super::{Answer, Batch, In, Join, Remote, ViewerFrame, Welcome, DEAD, FRAME, PING};
use crate::protocol::{Command, LiveEvent, StreamLine};
use krowk_api::Client;
use krowk_client::e2e::{self, DeviceId, SessionKeys, SigningKey};
use krowk_client::device_chain::Chain;
use krowk_client::session_record::Signer;
use krowk_client::user_key::UserKeys;
use krowk_client::protocol::frame::{KIND_ACK, KIND_BATCH};
use krowk_client::relay_link::{Outbound, ViewerLink};
use serde_json::json;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

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
    let handed = Arc::new(std::sync::Mutex::new(vec![Instant::now()]));
    let first = vec![Update::Attached { events: attached.events.clone(), head: attached.head }];
    let title = attached.index.title.clone();
    let _ = tx.send(first).await;
    tokio::spawn(live(o, key, attached, rx, handoff_rx, tx, handed.clone()));
    Ok(Viewer { commands, handoff, updates, attach_time, handed, title, host })
}

/// The last-applied-batch ack a viewer sends the relay (relay.md → Flow
/// control): kind 3, in the clear, `seq` the last batch it applied.
fn ack(session: &[u8; 16], seq: u64) -> Vec<u8> {
    let mut b = vec![krowk_client::protocol::frame::V, KIND_ACK, 0, 0];
    b.extend_from_slice(session);
    b.extend_from_slice(&seq.to_be_bytes());
    b
}

/// A viewer acks at least every this many batches, well inside the relay's
/// window of 16, and at the next display frame after any it applied.
const ACK_EVERY: u64 = 4;

/// Keeps the newest logged event id: ids are UUIDv7, so the greatest is
/// the latest.
fn newest(last: &mut Option<String>, id: &str) {
    if last.as_deref().is_none_or(|l| id > l) {
        *last = Some(id.to_string());
    }
}

async fn live(o: Arc<Options>, key: SessionKeys, at_rest: Attached, mut commands: mpsc::UnboundedReceiver<Command>, mut handoff: mpsc::UnboundedReceiver<()>, out: mpsc::Sender<Vec<Update>>, handed: Arc<std::sync::Mutex<Vec<Instant>>>) {
    let raw = crate::daemon::ws::uuid(&o.session);
    let seen: HashSet<String> = at_rest.events.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect();
    let mut last_id: Option<String> = None;
    for id in &seen {
        newest(&mut last_id, id);
    }
    let (won_tx, mut won) = mpsc::unbounded_channel::<Option<(Candidate, super::Ws, serde_json::Value)>>();
    let mut v = Watching {
        o,
        key,
        raw,
        at_rest,
        out,
        handed,
        seen,
        last_id,
        asked: HashSet::new(),
        frame: Vec::new(),
        queued: VecDeque::new(),
        unacked: BTreeMap::new(),
        held: Vec::new(),
        link: None,
        ws: None,
        host: false,
        retry: Instant::now(),
        unjoined: None,
        heard: Instant::now(),
        // Random per viewer run, so a restarted viewer's ids never read as a
        // repeat of an earlier run's at the host, which dedups by them.
        run_id: e2e::hex(&e2e::random::<8>()),
        next_id: 0,
        candidates: Vec::new(),
        won_tx,
        probing: false,
        reprobe: Instant::now(),
        backoff: REPROBE,
        moving: None,
        moving_until: Instant::now(),
        leaving: None,
        on: None,
        applied: 0,
        acked: 0,
        catch_ups: CatchUps::default(),
        handoff: None,
    };
    // The first frame went with the attach; the next is a frame after it,
    // and a late tick waits a whole frame rather than firing twice.
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + FRAME, FRAME);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut beat = tokio::time::interval(PING);
    loop {
        tokio::select! {
            c = commands.recv() => {
                let Some(command) = c else { break };
                v.command(command).await;
            }
            Some(()) = handoff.recv() => v.ask_handoff().await,
            _ = tick.tick(), if !v.frame.is_empty() || v.applied > v.acked => {
                if !v.tick().await { break; }
            }
            _ = beat.tick() => {
                if let Some(w) = v.ws.as_mut() && (v.heard.elapsed() > DEAD || !super::ping(w).await) { v.ws = None; }
            }
            _ = tokio::time::sleep_until(v.retry.into()), if v.ws.is_none() => v.join().await,
            _ = tokio::time::sleep_until(v.reprobe.into()), if v.may_probe() => v.probe(),
            Some(w) = won.recv() => v.won(w).await,
            _ = tokio::time::sleep_until(v.moving_until.into()), if v.moving.is_some() => v.move_late(),
            m = recv_on(&mut v.leaving) => v.leaving_in(m).await,
            m = recv_on(&mut v.ws) => {
                if !v.inbound(m).await { break; }
            }
        }
    }
}

/// The next message on `ws`; with none, never.
async fn recv_on(ws: &mut Option<super::Ws>) -> In {
    match ws.as_mut() {
        Some(w) => super::recv(w).await,
        None => std::future::pending().await,
    }
}

/// A viewer as it follows the session live: the stream read off the relay
/// or a direct path, its commands sent, and what to show handed on a
/// display frame at a time.
struct Watching {
    o: Arc<Options>,
    key: SessionKeys,
    raw: [u8; 16],
    at_rest: Attached,
    out: mpsc::Sender<Vec<Update>>,
    handed: Arc<std::sync::Mutex<Vec<Instant>>>,
    seen: HashSet<String>,
    last_id: Option<String>,
    /// Approval requests shown and not yet resolved, by request id: every
    /// welcome names the waiting ones and the stream carries them too, and
    /// a move to a direct path is a second welcome, so each is shown once.
    asked: HashSet<String>,
    frame: Vec<Update>,
    queued: VecDeque<Remote>,
    unacked: BTreeMap<String, Remote>,
    held: Vec<Vec<u8>>,
    link: Option<ViewerLink>,
    ws: Option<super::Ws>,
    host: bool,
    retry: Instant,
    /// Why the relay could not be joined, said once (`Update::Note`) as it
    /// comes or changes: otherwise a viewer that never reaches the relay only ever
    /// shows its prompts queued.
    unjoined: Option<String>,
    heard: Instant,
    run_id: String,
    next_id: u64,
    // Direct paths (R-NET-2): the candidates the host's welcome named, a
    // race of them against the relay in flight, the one being moved onto
    // (its hello said, its welcome awaited, the relay's connection still
    // read meanwhile), and the one the session is on.
    candidates: Vec<Candidate>,
    won_tx: mpsc::UnboundedSender<Option<(Candidate, super::Ws, serde_json::Value)>>,
    probing: bool,
    reprobe: Instant,
    /// How long until the next race: `REPROBE`, doubled after each that
    /// finds no direct path, up to `REPROBE_MAX`, and back to `REPROBE`
    /// once one is taken.
    backoff: Duration,
    moving: Option<Candidate>,
    /// When the direct welcome must have come by: past it, or with more
    /// than `HELD_MAX` batches waiting for it, the direct path is dropped.
    moving_until: Instant,
    leaving: Option<super::Ws>,
    on: Option<Candidate>,
    /// The last stream batch applied, and the last acked to the relay.
    applied: u64,
    acked: u64,
    /// The catch-up asked and not yet answered, and one owed after it.
    catch_ups: CatchUps,
    /// A handoff asked and not yet answered, by its id: asked again of the
    /// next host to welcome this viewer.
    handoff: Option<String>,
}

impl Watching {
    /// A command of the person's: sent once the host has welcomed this
    /// viewer, queued for its welcome until then.
    async fn command(&mut self, command: Command) {
        self.next_id += 1;
        let r = Remote { id: format!("{}-{}-{}", self.o.device, self.run_id, self.next_id), command };
        let ready = self.host && self.link.as_ref().is_some_and(|l| l.welcomed());
        self.frame.push(Update::Sent { id: r.id.clone(), queued: !ready });
        if ready {
            let sealed = self.link.as_mut().expect("welcomed").frame(&serde_json::to_vec(&ViewerFrame::Command(r.clone())).expect("json"), false);
            self.unacked.insert(r.id.clone(), r);
            if let (Some(w), Ok(b)) = (self.ws.as_mut(), sealed) && !super::send(w, b).await { self.ws = None; }
        } else {
            self.queued.push_back(r);
        }
    }

    /// Asks the host to hand the session to this device, now if a host
    /// has welcomed this viewer, else at the next welcome.
    async fn ask_handoff(&mut self) {
        self.next_id += 1;
        self.handoff = Some(format!("{}-{}-{}", self.o.device, self.run_id, self.next_id));
        if self.host && self.link.as_ref().is_some_and(|l| l.welcomed()) {
            self.send_handoff().await;
        }
    }

    async fn send_handoff(&mut self) {
        let (Some(id), Some(l)) = (self.handoff.clone(), self.link.as_mut()) else { return };
        let f = ViewerFrame::Handoff { id, device: self.o.device.to_string() };
        if let (Some(w), Ok(sealed)) = (self.ws.as_mut(), l.frame(&serde_json::to_vec(&f).expect("json"), false)) && !super::send(w, sealed).await { self.ws = None; }
    }

    /// A display frame: the batches applied acked, and what was gathered
    /// handed on. False once nobody takes it.
    async fn tick(&mut self) -> bool {
        if self.applied > self.acked && let Some(w) = self.ws.as_mut() {
            self.acked = self.applied;
            if !super::send(w, ack(&self.raw, self.applied)).await { self.ws = None; }
        }
        // Never two hand-offs inside one display frame, however late
        // the last tick ran (R-LAG-7).
        let last = self.handed.lock().unwrap_or_else(|e| e.into_inner()).last().copied();
        if !self.frame.is_empty() && last.is_none_or(|t| t.elapsed() >= FRAME) {
            self.handed.lock().unwrap_or_else(|e| e.into_inner()).push(Instant::now());
            if self.out.send(std::mem::take(&mut self.frame)).await.is_err() { return false; }
        }
        true
    }

    /// Joins the relay, with a fresh ticket.
    async fn join(&mut self) {
        let o = self.o.clone();
        let api = o.api.clone();
        let (id, device, env) = (o.session.clone(), o.device.to_string(), o.env.clone());
        let ticket = match tokio::task::spawn_blocking(move || api.relay_ticket(&id, &device, &env)).await {
            Ok(Ok(t)) => t.relay_ticket,
            _ => { self.retry = Instant::now() + Duration::from_secs(1); return; }
        };
        let extra = match self.link.as_ref().and_then(|l| l.cursor()) {
            Some((s, n)) => json!({"stream": e2e::hex(&s), "afterSeq": n}),
            None => json!({}),
        };
        let j = Join { relay: &o.relay, session: &o.session, env: &o.env, ticket: &ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_VIEWER, extra };
        match super::join(j).await {
            Ok((w, joined)) => self.joined(w, &joined).await,
            Err(e) => {
                if self.unjoined.as_deref() != Some(e.as_str()) {
                    self.frame.push(Update::Note(format!("not on the relay {}: {e}; trying again every second", o.relay)));
                    self.unjoined = Some(e);
                }
                self.retry = Instant::now() + Duration::from_secs(1);
            }
        }
    }

    /// On the relay: hello said to the host there, if one is.
    async fn joined(&mut self, mut w: super::Ws, joined: &serde_json::Value) {
        self.unjoined = None;
        (self.on, self.moving, self.leaving) = (None, None, None);
        let n = joined["link"].as_u64().unwrap_or(0);
        let mut l = match self.link.take() { Some(l) => l.reconnect(n), None => ViewerLink::new(self.key.current(), self.raw, n) };
        self.host = joined["host"].as_bool().unwrap_or(false);
        self.held.clear();
        self.catch_ups.reset();
        if self.host && let Ok(h) = l.hello(b"{}") && !super::send(&mut w, h).await { self.retry = Instant::now() + Duration::from_secs(1); self.link = Some(l); return; }
        // Where the stream stands for this viewer: its own cursor
        // on a stream it read, else the relay's count, live from
        // the next batch.
        self.applied = match l.cursor() { Some((s, n)) if joined["stream"].as_str() == Some(e2e::hex(&s).as_str()) => n, _ => joined["seq"].as_u64().unwrap_or(0) };
        self.acked = self.applied;
        self.link = Some(l);
        if !self.host { self.frame.push(Update::Host(false)); }
        self.ws = Some(w);
        self.heard = Instant::now();
    }

    /// Whether to race the candidates for a direct path now.
    fn may_probe(&self) -> bool {
        self.on.is_none() && self.moving.is_none() && !self.probing && !self.candidates.is_empty() && self.link.as_ref().is_some_and(|l| l.welcomed())
    }

    fn probe(&mut self) {
        self.probing = true;
        tokio::spawn(race(self.o.clone(), self.candidates.clone(), self.link.as_ref().and_then(|l| l.cursor()), self.won_tx.clone()));
    }

    /// A race's end.
    async fn won(&mut self, w: Option<(Candidate, super::Ws, serde_json::Value)>) {
        self.probing = false;
        match w {
            Some((c, dw, joined)) if self.on.is_none() && self.moving.is_none() && self.ws.is_some() && joined["host"].as_bool() == Some(true) => self.move_to(c, dw, &joined).await,
            // A candidate joined with no host there (its uplink to
            // the listener not up yet), or nobody answered: later.
            Some(_) | None => (self.reprobe, self.backoff) = later(self.backoff),
        }
    }

    /// The first candidate to join, with the host there: the same hello
    /// and welcome as on the relay, over it. The relay's connection is read
    /// until the welcome comes, and what it brings meanwhile waits with what
    /// the direct one does, so nothing between the two is lost.
    async fn move_to(&mut self, c: Candidate, mut dw: super::Ws, joined: &serde_json::Value) {
        let n = joined["link"].as_u64().unwrap_or(0);
        let Some(old) = self.link.take() else { return };
        let mut l = old.reconnect(n);
        let said = match l.hello(b"{}") {
            Ok(h) => super::send(&mut dw, h).await,
            Err(_) => false,
        };
        match said {
            true => {
                self.leaving = self.ws.take();
                self.ws = Some(dw);
                self.heard = Instant::now();
                self.moving = Some(c);
                self.moving_until = Instant::now() + PROBE;
            }
            false => self.back_to_relay(),
        }
        self.link = Some(l);
    }

    /// The direct welcome is late, or too much waits on it: the
    /// relay again, from the cursor, which its buffer fills.
    fn move_late(&mut self) {
        (self.moving, self.leaving) = (None, None);
        self.back_to_relay();
    }

    /// Off a direct path, or one being moved onto: the relay joined again
    /// at once, and the next race put off.
    fn back_to_relay(&mut self) {
        self.ws = None;
        self.retry = Instant::now();
        (self.reprobe, self.backoff) = later(self.backoff);
    }

    /// The relay's connection, while the session moves off it: its batches
    /// are the stream's too, and a gap among them is caught up like one on
    /// the live path.
    async fn leaving_in(&mut self, m: In) {
        match m {
            In::Closed => self.leaving = None,
            In::Envelope(b) if b[1] == KIND_BATCH && b[20..28] != [0; 8] => match self.link.as_mut() {
                Some(l) if l.welcomed() => {
                    let gap = apply(l, &b, &mut self.seen, &mut self.last_id, &mut self.asked, &mut self.frame, &mut self.applied);
                    if let Some(after) = gap.and_then(|after| gap_found(&mut self.catch_ups, after, false, &mut self.frame)) {
                        self.ask_catch_up(after).await;
                    }
                }
                _ => {
                    self.held.push(b);
                    if self.held.len() > HELD_MAX { self.moving_until = Instant::now(); }
                }
            },
            _ => {}
        }
    }

    /// What came on the connection the session is read from. False when
    /// the viewer cannot go on.
    async fn inbound(&mut self, m: In) -> bool {
        self.heard = Instant::now();
        match m {
            // A direct path gone falls back to the relay at once, from
            // the cursor, which the relay's buffer fills (R-NET-2).
            In::Closed if self.on.is_some() || self.moving.is_some() => self.back_to_relay(),
            In::Closed => { self.ws = None; self.retry = Instant::now() + Duration::from_secs(1); }
            In::Alive => {}
            In::Control(v) if v["type"] == "host" => self.presence(&v),
            // A frame sent as the host went: dropped by the relay, so the
            // host is not there, and what was sent waits, unacknowledged,
            // for the welcome of the next.
            In::Control(v) if v["type"] == "error" && v["code"] == "host_absent" => {
                if self.host { self.host = false; self.frame.push(Update::Host(false)); }
            }
            In::Control(v) if v["type"] == "resync" => return self.resync(&v).await,
            In::Control(_) => {}
            In::Envelope(b) if b[1] == KIND_BATCH => return self.batch(b).await,
            In::Envelope(_) => {}
        }
        true
    }

    /// Presence is the relay's word, not the host's. A host arriving — the
    /// first, or one back from a link the relay let go, which may have
    /// taken a command with it — is joined again and said hello to: its
    /// welcome sends again every command not yet acknowledged, and none
    /// runs twice (the host dedups by command id).
    fn presence(&mut self, v: &serde_json::Value) {
        let present = v["present"].as_bool().unwrap_or(false);
        let welcomed = self.link.as_ref().is_some_and(|l| l.welcomed());
        if !present && (self.on.is_some() || self.moving.is_some()) {
            // The host's uplink to its direct listener went, not
            // the host: back to the relay, where it may still be.
            self.back_to_relay();
        } else if present && (!welcomed || (self.on.is_none() && self.moving.is_none())) { self.ws = None; self.retry = Instant::now(); } else if !present { self.host = false; self.frame.push(Update::Host(false)); }
    }

    /// The relay cannot carry the stream on for this viewer. False when
    /// the viewer cannot go on.
    async fn resync(&mut self, v: &serde_json::Value) -> bool {
        if v["reason"] == "stream" {
            // Another stream: a new host, or one that could not carry
            // its count on. Join again and be welcomed onto it.
            self.ws = None;
            self.retry = Instant::now();
        } else if self.link.as_ref().is_some_and(|l| l.welcomed()) {
            // The relay cannot fill the gap: the host can, now, or once
            // the catch-up in flight ends.
            if let Some(after) = self.catch_ups.gap(self.last_id.clone(), false) {
                self.ask_catch_up(after).await;
            }
        } else {
            // No host: the chunks, up to what was written.
            match self.chunks(None).await {
                Some(Ok((fresh, a))) => self.caught_up(fresh, a),
                // The session moved to a key epoch this viewer opened it
                // before: nothing after it opens here until it is opened again.
                Some(Err(e)) if e.contains(e2e::ROTATED_SINCE_OPENED) => { self.frame.push(Update::Failed(e)); let _ = self.out.send(std::mem::take(&mut self.frame)).await; return false; }
                _ => {}
            }
        }
        true
    }

    /// The chunks written since this viewer last read them, up to `want`
    /// when it is known; None when the read itself did not run.
    async fn chunks(&self, want: Option<Head>) -> Option<Result<(Vec<serde_json::Value>, Attached), String>> {
        let (api, key2, id) = (self.o.api.clone(), self.key.clone(), self.o.session.clone());
        let mut a = self.at_rest.clone();
        tokio::task::spawn_blocking(move || store::catch_up(&api, &key2, &id, &mut a, want).map(|f| (f, a))).await.ok()
    }

    /// What the chunks held that was not seen yet, shown.
    fn caught_up(&mut self, fresh: Vec<serde_json::Value>, a: Attached) {
        let fresh = fresh_only(fresh, &mut self.seen, &mut self.last_id);
        self.at_rest = a;
        if !fresh.is_empty() { self.frame.push(Update::CaughtUp(fresh)); }
    }

    /// A batch on the stream, or one routed to this viewer. False when the
    /// viewer cannot go on.
    async fn batch(&mut self, b: Vec<u8>) -> bool {
        let Some(l) = self.link.as_mut() else { return true };
        let routed = b[20..28] == [0; 8];
        if routed {
            return self.routed(&b).await;
        } else if !l.welcomed() {
            self.held.push(b);
            if self.moving.is_some() && self.held.len() > HELD_MAX { self.moving_until = Instant::now(); }
        } else if let Some((_, have)) = l.cursor() && u64::from_be_bytes(b[20..28].try_into().expect("eight bytes")) <= have {
            // A batch this viewer opened already, by the other
            // uplink: a direct listener replays from the cursor
            // the race joined it at, and a relay still holds
            // what the direct path brought. It is acked all the
            // same — the listener counts its window in what it
            // sent, and holds the live batches behind it until
            // told the viewer has these.
            if let Some(w) = self.ws.as_mut() {
                self.acked = self.acked.max(have);
                if !super::send(w, ack(&self.raw, have)).await { self.ws = None; }
            }
        } else {
            let gap = apply(l, &b, &mut self.seen, &mut self.last_id, &mut self.asked, &mut self.frame, &mut self.applied);
            // A batch the relay dropped: the host fills the gap,
            // from the newest event held before this batch, now or
            // once the catch-up in flight ends.
            if let Some(after) = gap.and_then(|after| gap_found(&mut self.catch_ups, after, false, &mut self.frame)) {
                self.ask_catch_up(after).await;
            }
            if self.applied >= self.acked + ACK_EVERY && let Some(w) = self.ws.as_mut() {
                self.acked = self.applied;
                if !super::send(w, ack(&self.raw, self.applied)).await { self.ws = None; }
            }
        }
        true
    }

    /// A batch routed to this viewer: the host's welcome, or its answer.
    async fn routed(&mut self, b: &[u8]) -> bool {
        let Some(l) = self.link.as_mut() else { return true };
        match l.open_routed(b) {
            Ok(Outbound::Welcome { body, .. }) => return self.welcome(&body).await,
            Ok(Outbound::Routed { body, .. }) => self.answer(&body).await,
            Err(_) => {}
        }
        true
    }

    /// The host's welcome. False when the chunks could not be read up to
    /// the head it sealed.
    async fn welcome(&mut self, body: &[u8]) -> bool {
        let w: Welcome = serde_json::from_slice(body).unwrap_or_default();
        if let Some(c) = self.moving.take() {
            self.leaving = None;
            self.backoff = REPROBE;
            self.on = Some(c);
        } else if self.on.is_none() {
            self.candidates = w.candidates.clone();
        }
        self.frame.push(match &self.on { Some(c) => Update::Path { path: c.path().into(), via: Some(c.url.clone()) }, None => Update::Path { path: "relay".into(), via: None } });
        // The prefix check: the chunks must reach the head the host sealed.
        match self.chunks(w.head).await {
            Some(Ok((fresh, a))) => self.caught_up(fresh, a),
            Some(Err(e)) => { self.frame.push(Update::Failed(e)); let _ = self.out.send(std::mem::take(&mut self.frame)).await; return false; }
            None => {}
        }
        self.host = true;
        self.frame.push(Update::Host(true));
        for r in w.approvals {
            if self.asked.insert(r.request_id.clone()) { self.frame.push(Update::Line(StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(r)))); }
        }
        self.send_welcomed().await;
        let Some(l) = self.link.as_mut() else { return true };
        // Held batches arrived before the welcome, so the catch-up
        // `send_welcomed` just asked covers a gap among them (by order).
        let held = std::mem::take(&mut self.held);
        if let Some(after) = replay(l, held, &mut self.seen, &mut self.last_id, &mut self.asked, &mut self.frame, &mut self.applied).and_then(|after| self.catch_ups.gap(after, true)) {
            self.ask_catch_up(after).await;
        }
        let Some(l) = self.link.as_mut() else { return true };
        // What was held may all have been batches opened
        // already by the relay: acked all the same, or a
        // direct listener, its window full of them, sends
        // nothing more.
        if let (Some(w), Some((_, have))) = (self.ws.as_mut(), l.cursor()) {
            self.acked = self.acked.max(have);
            if !super::send(w, ack(&self.raw, have)).await { self.ws = None; }
        }
        true
    }

    /// What goes to a host once it has welcomed this viewer.
    async fn send_welcomed(&mut self) {
        let Some(l) = self.link.as_mut() else { return };
        // What the chunks do not hold yet — the running
        // turn's events — the host sends.
        let mut sends = vec![ViewerFrame::CatchUp { after: self.last_id.clone() }];
        self.catch_ups.asked(self.last_id.clone());
        // What waited for a host goes now, in order; what
        // was sent before and never acked goes again, and
        // runs once.
        let resend: Vec<Remote> = self.unacked.values().cloned().collect();
        for r in resend.into_iter().chain(self.queued.drain(..)) {
            self.unacked.insert(r.id.clone(), r.clone());
            sends.push(ViewerFrame::Command(r));
        }
        for f in sends {
            if let (Some(w), Ok(sealed)) = (self.ws.as_mut(), l.frame(&serde_json::to_vec(&f).expect("json"), false)) && !super::send(w, sealed).await { self.ws = None; break; }
        }
        self.send_handoff().await;
    }

    /// Asks the host for the logged events after `after`: a catch-up now
    /// in flight.
    async fn ask_catch_up(&mut self, after: Option<String>) {
        let Some(l) = self.link.as_mut() else { return };
        let Ok(f) = l.frame(&serde_json::to_vec(&ViewerFrame::CatchUp { after: after.clone() }).expect("json"), false) else { return };
        self.catch_ups.asked(after);
        if let Some(w) = self.ws.as_mut() && !super::send(w, f).await { self.ws = None; }
    }

    /// The host's answer to this viewer: a command's ack, or a page of
    /// the catch-up it asked for.
    async fn answer(&mut self, body: &[u8]) {
        match serde_json::from_slice(body) {
            Ok(Answer::Ack { id, error }) => {
                self.unacked.remove(&id);
                self.frame.push(Update::Acked { id, error });
            }
            Ok(Answer::Handoff { id, handed, error }) if self.handoff.as_deref() == Some(id.as_str()) => {
                self.handoff = None;
                self.frame.push(Update::Handoff(handed.ok_or_else(|| error.unwrap_or_else(|| "the host refused the handoff".into()))));
            }
            Ok(Answer::Handoff { .. }) => {}
            Ok(Answer::CatchUp { events, more }) => {
                let events: Vec<serde_json::Value> = events.iter().filter_map(|e| serde_json::to_value(e).ok()).collect();
                let fresh = fresh_only(events, &mut self.seen, &mut self.last_id);
                if !fresh.is_empty() { self.frame.push(Update::CaughtUp(fresh)); }
                // The last page: a gap it did not cover is asked now.
                if !more && let Some(after) = self.catch_ups.done() {
                    self.ask_catch_up(after).await;
                }
            }
            Err(_) => {}
        }
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

/// The next race's time, and the wait after it.
fn later(backoff: Duration) -> (Instant, Duration) {
    (Instant::now() + backoff, (backoff * 2).min(REPROBE_MAX))
}

/// How long one candidate has to answer the join.
pub const PROBE: Duration = Duration::from_secs(3);

/// Races every candidate: joins each, at the viewer's cursor, with a fresh
/// ticket, and hands on the first that joins, or None when none does. The
/// rest are dropped as it returns.
async fn race(o: Arc<Options>, candidates: Vec<Candidate>, cursor: Option<([u8; 16], u64)>, won: mpsc::UnboundedSender<Option<(Candidate, super::Ws, serde_json::Value)>>) {
    let api = o.api.clone();
    let (id, device, env) = (o.session.clone(), o.device.to_string(), o.env.clone());
    let Ok(Ok(ticket)) = tokio::task::spawn_blocking(move || api.relay_ticket(&id, &device, &env)).await else {
        let _ = won.send(None);
        return;
    };
    let ticket = ticket.relay_ticket;
    let extra = match cursor {
        Some((s, n)) => json!({"stream": e2e::hex(&s), "afterSeq": n}),
        None => json!({}),
    };
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

/// The events not seen before, each once, the newest id kept.
fn fresh_only(events: Vec<serde_json::Value>, seen: &mut HashSet<String>, last: &mut Option<String>) -> Vec<serde_json::Value> {
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
struct CatchUps {
    in_flight: Option<Option<String>>,
    owed: Option<Option<String>>,
}

impl CatchUps {
    /// A gap from `after`: the `after` to ask from now, if any.
    /// `covered_by_order` is true for a batch replayed from `held` at a
    /// welcome: it arrived before the welcome, so the catch-up asked after
    /// that welcome was answered from a log that holds its events.
    fn gap(&mut self, after: Option<String>, covered_by_order: bool) -> Option<Option<String>> {
        let Some(flying) = &self.in_flight else { return Some(after) };
        if covered_by_order && no_newer(flying, &after) {
            return None;
        }
        // Owed: the oldest, which covers the later ones.
        if self.owed.as_ref().is_none_or(|o| !no_newer(o, &after)) {
            self.owed = Some(after);
        }
        None
    }

    /// Asked (after a welcome, a resync, or a gap): now in flight from
    /// `after`.
    fn asked(&mut self, after: Option<String>) {
        self.in_flight = Some(after);
    }

    /// The last page came: what to ask next, if a gap was owed.
    fn done(&mut self) -> Option<Option<String>> {
        self.in_flight = None;
        self.owed.take()
    }

    /// Joined again: nothing in flight, nothing owed.
    fn reset(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// A gap from `after`, found as a batch applied: shown, once, and the
/// `after` of the catch-up to ask now, if any.
fn gap_found(c: &mut CatchUps, after: Option<String>, covered_by_order: bool, frame: &mut Vec<Update>) -> Option<Option<String>> {
    frame.push(Update::Gap);
    c.gap(after, covered_by_order)
}

/// The batches held for a welcome, applied in order, each gap among them
/// shown once. Answers the first gap's `after`: the oldest, which covers
/// the later ones.
fn replay(l: &mut ViewerLink, held: Vec<Vec<u8>>, seen: &mut HashSet<String>, last: &mut Option<String>, asked: &mut HashSet<String>, frame: &mut Vec<Update>, applied: &mut u64) -> Option<Option<String>> {
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
fn apply(l: &mut ViewerLink, b: &[u8], seen: &mut HashSet<String>, last: &mut Option<String>, asked: &mut HashSet<String>, frame: &mut Vec<Update>, applied: &mut u64) -> Option<Option<String>> {
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
    use krowk_client::e2e::SessionKey;
    use krowk_client::protocol::frame::HEADER;
    use krowk_client::relay_link::HostLink;

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

    /// R-OFF-2: batches 1 and 3 held for a welcome whose catch-up went out
    /// from an older event: the gap shows once, and the welcome's catch-up
    /// covers it, so no second one is asked.
    #[test]
    fn r_off_2_a_gap_among_held_batches_shows_and_is_covered_by_the_welcomes_catch_up() {
        let (mut link, [b1, _, b3]) = three_batches();
        let mut c = CatchUps::default();
        // `send_welcomed`: the catch-up from the newest event the chunks held.
        c.asked(Some(id(0)));
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

        // Joined again: nothing in flight, nothing owed.
        c.asked(Some(id(1)));
        c.gap(Some(id(2)), false);
        c.reset();
        assert!(!c.in_flight());
        assert_eq!(c.done(), None);
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
    }
}
