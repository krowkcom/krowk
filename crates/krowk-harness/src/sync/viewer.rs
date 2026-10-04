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
                Ok(index) => out.push(Listed { id: s.id.clone(), index, holder: full.lease.map(|l| l.device) }),
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
    let (tx, updates) = mpsc::channel(256);
    let handed = Arc::new(std::sync::Mutex::new(vec![Instant::now()]));
    let first = vec![Update::Attached { events: attached.events.clone(), head: attached.head }];
    let title = attached.index.title.clone();
    let _ = tx.send(first).await;
    tokio::spawn(live(o, key, attached, rx, tx, handed.clone()));
    Ok(Viewer { commands, updates, attach_time, handed, title, host })
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

// Legacy: the viewer's whole live loop, one select over every source. TODO: split into helpers and drop this allow.
#[allow(clippy::cognitive_complexity)]
async fn live(o: Arc<Options>, key: SessionKeys, mut at_rest: Attached, mut commands: mpsc::UnboundedReceiver<Command>, out: mpsc::Sender<Vec<Update>>, handed: Arc<std::sync::Mutex<Vec<Instant>>>) {
    let raw = crate::daemon::ws::uuid(&o.session);
    let mut seen: HashSet<String> = at_rest.events.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect();
    let mut last_id: Option<String> = None;
    // Approval requests shown and not yet resolved, by request id: every
    // welcome names the waiting ones and the stream carries them too, and
    // a move to a direct path is a second welcome, so each is shown once.
    let mut asked: HashSet<String> = HashSet::new();
    for id in &seen {
        newest(&mut last_id, id);
    }
    let mut frame: Vec<Update> = Vec::new();
    let mut queued: VecDeque<Remote> = VecDeque::new();
    let mut unacked: BTreeMap<String, Remote> = BTreeMap::new();
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut link: Option<ViewerLink> = None;
    let mut ws: Option<super::Ws> = None;
    let mut host = false;
    let mut retry = Instant::now();
    // Why the relay could not be joined, said once (`Update::Note`) as it
    // comes or changes: otherwise a viewer that never reaches the relay only ever
    // shows its prompts queued.
    let mut unjoined: Option<String> = None;
    let mut heard = Instant::now();
    // Random per viewer run, so a restarted viewer's ids never read as a
    // repeat of an earlier run's at the host, which dedups by them.
    let run_id = e2e::hex(&e2e::random::<8>());
    let mut next_id = 0u64;
    // Direct paths (R-NET-2): the candidates the host's welcome named, a
    // race of them against the relay in flight, the one being moved onto
    // (its hello said, its welcome awaited, the relay's connection still
    // read meanwhile), and the one the session is on.
    let mut candidates: Vec<Candidate> = Vec::new();
    let (won_tx, mut won) = mpsc::unbounded_channel::<Option<(Candidate, super::Ws, serde_json::Value)>>();
    let mut probing = false;
    let mut reprobe = Instant::now();
    // How long until the next race: `REPROBE`, doubled after each that
    // finds no direct path, up to `REPROBE_MAX`, and back to `REPROBE`
    // once one is taken.
    let mut backoff = REPROBE;
    let mut moving: Option<Candidate> = None;
    // When the direct welcome must have come by: past it, or with more
    // than `HELD_MAX` batches waiting for it, the direct path is dropped.
    let mut moving_until = Instant::now();
    let mut leaving: Option<super::Ws> = None;
    let mut on: Option<Candidate> = None;
    // The last stream batch applied, and the last acked to the relay.
    let (mut applied, mut acked) = (0u64, 0u64);
    // A catch-up asked and not yet answered: another is not asked meanwhile.
    let mut catching_up = false;
    // The first frame went with the attach; the next is a frame after it,
    // and a late tick waits a whole frame rather than firing twice.
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + FRAME, FRAME);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut beat = tokio::time::interval(PING);
    loop {
        tokio::select! {
            c = commands.recv() => {
                let Some(command) = c else { break };
                next_id += 1;
                let r = Remote { id: format!("{}-{run_id}-{next_id}", o.device), command };
                let ready = host && link.as_ref().is_some_and(|l| l.welcomed());
                frame.push(Update::Sent { id: r.id.clone(), queued: !ready });
                if ready {
                    let sealed = link.as_mut().expect("welcomed").frame(&serde_json::to_vec(&ViewerFrame::Command(r.clone())).expect("json"), false);
                    unacked.insert(r.id.clone(), r);
                    if let (Some(w), Ok(b)) = (ws.as_mut(), sealed) && !super::send(w, b).await { ws = None; }
                } else {
                    queued.push_back(r);
                }
            }
            _ = tick.tick(), if !frame.is_empty() || applied > acked => {
                if applied > acked && let Some(w) = ws.as_mut() {
                    acked = applied;
                    if !super::send(w, ack(&raw, applied)).await { ws = None; }
                }
                // Never two hand-offs inside one display frame, however late
                // the last tick ran (R-LAG-7).
                let last = handed.lock().unwrap_or_else(|e| e.into_inner()).last().copied();
                if !frame.is_empty() && last.is_none_or(|t| t.elapsed() >= FRAME) {
                    handed.lock().unwrap_or_else(|e| e.into_inner()).push(Instant::now());
                    if out.send(std::mem::take(&mut frame)).await.is_err() { break; }
                }
            }
            _ = beat.tick() => {
                if let Some(w) = ws.as_mut() && (heard.elapsed() > DEAD || !super::ping(w).await) { ws = None; }
            }
            _ = tokio::time::sleep_until(retry.into()), if ws.is_none() => {
                let api = o.api.clone();
                let (id, device, env) = (o.session.clone(), o.device.to_string(), o.env.clone());
                let ticket = match tokio::task::spawn_blocking(move || api.relay_ticket(&id, &device, &env)).await {
                    Ok(Ok(t)) => t.relay_ticket,
                    _ => { retry = Instant::now() + Duration::from_secs(1); continue; }
                };
                let extra = match link.as_ref().and_then(|l| l.cursor()) {
                    Some((s, n)) => json!({"stream": e2e::hex(&s), "afterSeq": n}),
                    None => json!({}),
                };
                let j = Join { relay: &o.relay, session: &o.session, env: &o.env, ticket: &ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_VIEWER, extra };
                match super::join(j).await {
                    Ok((mut w, joined)) => {
                        unjoined = None;
                        (on, moving, leaving) = (None, None, None);
                        let n = joined["link"].as_u64().unwrap_or(0);
                        let mut l = match link.take() { Some(l) => l.reconnect(n), None => ViewerLink::new(key.current(), raw, n) };
                        host = joined["host"].as_bool().unwrap_or(false);
                        held.clear();
                        catching_up = false;
                        if host && let Ok(h) = l.hello(b"{}") && !super::send(&mut w, h).await { retry = Instant::now() + Duration::from_secs(1); link = Some(l); continue; }
                        // Where the stream stands for this viewer: its own cursor
                        // on a stream it read, else the relay's count, live from
                        // the next batch.
                        applied = match l.cursor() { Some((s, n)) if joined["stream"].as_str() == Some(e2e::hex(&s).as_str()) => n, _ => joined["seq"].as_u64().unwrap_or(0) };
                        acked = applied;
                        link = Some(l);
                        if !host { frame.push(Update::Host(false)); }
                        ws = Some(w);
                        heard = Instant::now();
                    }
                    Err(e) => {
                        if unjoined.as_deref() != Some(e.as_str()) {
                            frame.push(Update::Note(format!("not on the relay {}: {e}; trying again every second", o.relay)));
                            unjoined = Some(e);
                        }
                        retry = Instant::now() + Duration::from_secs(1);
                    }
                }
            }
            _ = tokio::time::sleep_until(reprobe.into()), if on.is_none() && moving.is_none() && !probing && !candidates.is_empty() && link.as_ref().is_some_and(|l| l.welcomed()) => {
                probing = true;
                tokio::spawn(race(o.clone(), candidates.clone(), link.as_ref().and_then(|l| l.cursor()), won_tx.clone()));
            }
            Some(w) = won.recv() => {
                probing = false;
                match w {
                    // The first candidate to join, with the host there: the
                    // same hello and welcome as on the relay, over it. The
                    // relay's connection is read until the welcome comes, and
                    // what it brings meanwhile waits with what the direct one
                    // does, so nothing between the two is lost.
                    Some((c, mut dw, joined)) if on.is_none() && moving.is_none() && ws.is_some() && joined["host"].as_bool() == Some(true) => {
                        let n = joined["link"].as_u64().unwrap_or(0);
                        let Some(old) = link.take() else { continue };
                        let mut l = old.reconnect(n);
                        let said = match l.hello(b"{}") {
                            Ok(h) => super::send(&mut dw, h).await,
                            Err(_) => false,
                        };
                        match said {
                            true => {
                                leaving = ws.take();
                                ws = Some(dw);
                                heard = Instant::now();
                                moving = Some(c);
                                moving_until = Instant::now() + PROBE;
                            }
                            false => { ws = None; retry = Instant::now(); (reprobe, backoff) = later(backoff); }
                        }
                        link = Some(l);
                    }
                    // A candidate joined with no host there (its uplink to
                    // the listener not up yet), or nobody answered: later.
                    Some(_) | None => (reprobe, backoff) = later(backoff),
                }
            }
            _ = tokio::time::sleep_until(moving_until.into()), if moving.is_some() => {
                // The direct welcome is late, or too much waits on it: the
                // relay again, from the cursor, which its buffer fills.
                (moving, leaving) = (None, None);
                ws = None;
                retry = Instant::now();
                (reprobe, backoff) = later(backoff);
            }
            m = async { match leaving.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => {
                // The relay's connection, while the session moves off it:
                // its batches are the stream's too.
                match m {
                    In::Closed => leaving = None,
                    In::Envelope(b) if b[1] == KIND_BATCH && b[20..28] != [0; 8] => match link.as_mut() {
                        Some(l) if l.welcomed() => {
                            apply(l, &b, &mut seen, &mut last_id, &mut asked, &mut frame, &mut applied);
                        }
                        _ => {
                            held.push(b);
                            if held.len() > HELD_MAX { moving_until = Instant::now(); }
                        }
                    },
                    _ => {}
                }
            }
            m = async { match ws.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => {
                heard = Instant::now();
                match m {
                    // A direct path gone falls back to the relay at once, from
                    // the cursor, which the relay's buffer fills (R-NET-2).
                    In::Closed if on.is_some() || moving.is_some() => { ws = None; retry = Instant::now(); (reprobe, backoff) = later(backoff); }
                    In::Closed => { ws = None; retry = Instant::now() + Duration::from_secs(1); }
                    In::Alive => {}
                    In::Control(v) if v["type"] == "host" => {
                        // Presence is the relay's word, not the host's. A host
                        // arriving — the first, or one back from a link the relay
                        // let go, which may have taken a command with it — is
                        // joined again and said hello to: its welcome sends again
                        // every command not yet acknowledged, and none runs twice
                        // (the host dedups by command id).
                        let present = v["present"].as_bool().unwrap_or(false);
                        let welcomed = link.as_ref().is_some_and(|l| l.welcomed());
                        if !present && (on.is_some() || moving.is_some()) {
                            // The host's uplink to its direct listener went, not
                            // the host: back to the relay, where it may still be.
                            ws = None;
                            retry = Instant::now();
                            (reprobe, backoff) = later(backoff);
                        } else if present && (!welcomed || (on.is_none() && moving.is_none())) { ws = None; retry = Instant::now(); } else if !present { host = false; frame.push(Update::Host(false)); }
                    }
                    // A frame sent as the host went: dropped by the relay, so the
                    // host is not there, and what was sent waits, unacknowledged,
                    // for the welcome of the next.
                    In::Control(v) if v["type"] == "error" && v["code"] == "host_absent" => {
                        if host { host = false; frame.push(Update::Host(false)); }
                    }
                    In::Control(v) if v["type"] == "resync" => {
                        if v["reason"] == "stream" {
                            // Another stream: a new host, or one that could not carry
                            // its count on. Join again and be welcomed onto it.
                            ws = None;
                            retry = Instant::now();
                        } else if let Some(l) = link.as_mut().filter(|l| l.welcomed()) {
                            // The relay cannot fill the gap: the host can.
                            if !catching_up && let Ok(b) = l.frame(&serde_json::to_vec(&ViewerFrame::CatchUp { after: last_id.clone() }).expect("json"), false) {
                                catching_up = true;
                                if let Some(w) = ws.as_mut() && !super::send(w, b).await { ws = None; }
                            }
                        } else {
                            // No host: the chunks, up to what was written.
                            let (api, key2, id) = (o.api.clone(), key.clone(), o.session.clone());
                            let mut a = at_rest.clone();
                            match tokio::task::spawn_blocking(move || store::catch_up(&api, &key2, &id, &mut a, None).map(|f| (f, a))).await {
                                Ok(Ok((fresh, a))) => {
                                    let fresh = fresh_only(fresh, &mut seen, &mut last_id);
                                    at_rest = a;
                                    if !fresh.is_empty() { frame.push(Update::CaughtUp(fresh)); }
                                }
                                // The session moved to a key epoch this viewer opened it
                                // before: nothing after it opens here until it is opened again.
                                Ok(Err(e)) if e.contains(e2e::ROTATED_SINCE_OPENED) => { frame.push(Update::Failed(e)); let _ = out.send(std::mem::take(&mut frame)).await; return; }
                                _ => {}
                            }
                        }
                    }
                    In::Control(_) => {}
                    In::Envelope(b) if b[1] == KIND_BATCH => {
                        let Some(l) = link.as_mut() else { continue };
                        let routed = b[20..28] == [0; 8];
                        if routed {
                            match l.open_routed(&b) {
                                Ok(Outbound::Welcome { body, .. }) => {
                                    let w: Welcome = serde_json::from_slice(&body).unwrap_or_default();
                                    if let Some(c) = moving.take() {
                                        leaving = None;
                                        backoff = REPROBE;
                                        on = Some(c);
                                    } else if on.is_none() {
                                        candidates = w.candidates.clone();
                                    }
                                    frame.push(match &on { Some(c) => Update::Path { path: c.path().into(), via: Some(c.url.clone()) }, None => Update::Path { path: "relay".into(), via: None } });
                                    // The prefix check: the chunks must reach the head the host sealed.
                                    let (api, key2, id, want) = (o.api.clone(), key.clone(), o.session.clone(), w.head);
                                    let mut a = at_rest.clone();
                                    match tokio::task::spawn_blocking(move || store::catch_up(&api, &key2, &id, &mut a, want).map(|f| (f, a))).await {
                                        Ok(Ok((fresh, a))) => {
                                            let fresh = fresh_only(fresh, &mut seen, &mut last_id);
                                            at_rest = a;
                                            if !fresh.is_empty() { frame.push(Update::CaughtUp(fresh)); }
                                        }
                                        Ok(Err(e)) => { frame.push(Update::Failed(e)); let _ = out.send(std::mem::take(&mut frame)).await; return; }
                                        Err(_) => {}
                                    }
                                    host = true;
                                    frame.push(Update::Host(true));
                                    for r in w.approvals {
                                        if asked.insert(r.request_id.clone()) { frame.push(Update::Line(StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(r)))); }
                                    }
                                    // What the chunks do not hold yet — the running
                                    // turn's events — the host sends.
                                    let mut sends = vec![ViewerFrame::CatchUp { after: last_id.clone() }];
                                    catching_up = true;
                                    // What waited for a host goes now, in order; what
                                    // was sent before and never acked goes again, and
                                    // runs once.
                                    let resend: Vec<Remote> = unacked.values().cloned().collect();
                                    for r in resend.into_iter().chain(queued.drain(..)) {
                                        unacked.insert(r.id.clone(), r.clone());
                                        sends.push(ViewerFrame::Command(r));
                                    }
                                    for f in sends {
                                        if let (Some(w), Ok(sealed)) = (ws.as_mut(), l.frame(&serde_json::to_vec(&f).expect("json"), false)) && !super::send(w, sealed).await { ws = None; break; }
                                    }
                                    for b in std::mem::take(&mut held) {
                                        apply(l, &b, &mut seen, &mut last_id, &mut asked, &mut frame, &mut applied);
                                    }
                                    // What was held may all have been batches opened
                                    // already by the relay: acked all the same, or a
                                    // direct listener, its window full of them, sends
                                    // nothing more.
                                    if let (Some(w), Some((_, have))) = (ws.as_mut(), l.cursor()) {
                                        acked = acked.max(have);
                                        if !super::send(w, ack(&raw, have)).await { ws = None; }
                                    }
                                }
                                Ok(Outbound::Routed { body, .. }) => match serde_json::from_slice(&body) {
                                    Ok(Answer::Ack { id, error }) => {
                                        unacked.remove(&id);
                                        frame.push(Update::Acked { id, error });
                                    }
                                    Ok(Answer::CatchUp { events, more }) => {
                                        let events: Vec<serde_json::Value> = events.iter().filter_map(|e| serde_json::to_value(e).ok()).collect();
                                        let fresh = fresh_only(events, &mut seen, &mut last_id);
                                        if !fresh.is_empty() { frame.push(Update::CaughtUp(fresh)); }
                                        if !more { catching_up = false; }
                                    }
                                    Err(_) => {}
                                },
                                Err(_) => {}
                            }
                        } else if !l.welcomed() {
                            held.push(b);
                            if moving.is_some() && held.len() > HELD_MAX { moving_until = Instant::now(); }
                        } else if let Some((_, have)) = l.cursor() && u64::from_be_bytes(b[20..28].try_into().expect("eight bytes")) <= have {
                            // A batch this viewer opened already, by the other
                            // uplink: a direct listener replays from the cursor
                            // the race joined it at, and a relay still holds
                            // what the direct path brought. It is acked all the
                            // same — the listener counts its window in what it
                            // sent, and holds the live batches behind it until
                            // told the viewer has these.
                            if let Some(w) = ws.as_mut() {
                                acked = acked.max(have);
                                if !super::send(w, ack(&raw, have)).await { ws = None; }
                            }
                        } else {
                            let gap = apply(l, &b, &mut seen, &mut last_id, &mut asked, &mut frame, &mut applied);
                            // A batch the relay dropped: the host fills the gap,
                            // from the newest event held before this batch.
                            if let Some(after) = gap && !catching_up && let Ok(f) = l.frame(&serde_json::to_vec(&ViewerFrame::CatchUp { after }).expect("json"), false) {
                                catching_up = true;
                                frame.push(Update::Gap);
                                if let Some(w) = ws.as_mut() && !super::send(w, f).await { ws = None; }
                            }
                            if applied >= acked + ACK_EVERY && let Some(w) = ws.as_mut() {
                                acked = applied;
                                if !super::send(w, ack(&raw, applied)).await { ws = None; }
                            }
                        }
                    }
                    In::Envelope(_) => {}
                }
            }
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
    for c in candidates {
        let (o, ticket, extra) = (o.clone(), ticket.clone(), extra.clone());
        set.spawn(async move {
            let signing = SigningKey::from_secret(&*o.signing.secret_bytes()).map_err(|e| e.to_string())?;
            let j = Join { relay: &c.url, session: &o.session, env: &o.env, ticket: &ticket, device: o.device, signing: &signing, role: e2e::RELAY_ROLE_VIEWER, extra };
            let (w, joined) = tokio::time::timeout(PROBE, super::join(j)).await.map_err(|_| "timed out".to_string())??;
            Ok::<_, String>((c, w, joined))
        });
    }
    while let Some(r) = set.join_next().await {
        if let Ok(Ok(w)) = r {
            let _ = won.send(Some(w));
            return;
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
