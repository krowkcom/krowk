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
use super::{Answer, Batch, In, Join, Remote, Welcome, DEAD, FRAME, PING};
use crate::protocol::{Command, StreamLine};
use krowk_api::Client;
use krowk_client::e2e::{self, AccountKey, DeviceId, SessionKey, SigningKey};
use krowk_client::protocol::frame::KIND_BATCH;
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
/// One whose key or index does not open under this account's key is left
/// out and counted.
pub fn list(api: &Client, account: &AccountKey) -> Result<(Vec<Listed>, usize), String> {
    let mut out = Vec::new();
    let mut unreadable = 0;
    let mut before = String::new();
    loop {
        let page = api.list_sync_sessions(&before, 100).map_err(|e| e.to_string())?;
        for s in &page.sessions {
            // A listing leaves the index out; `show` has it.
            let full = api.show_sync_session(&s.id).map_err(|e| e.to_string())?;
            match open_key(&full.wrapped_key, &s.id, account).and_then(|k| store::open_index(&k, &s.id, &full.sealed_index)) {
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

fn open_key(wrapped: &str, id: &str, account: &AccountKey) -> Result<SessionKey, String> {
    e2e::unwrap_session_key(&e2e::unhex(wrapped).ok_or("the session's key is not hex")?, &crate::daemon::ws::uuid(id), account).map_err(|e| e.to_string())
}

/// What a viewer hands its screen, one `Vec` a display frame.
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
    /// Events the chunks held that the live stream could not bring.
    CaughtUp(Vec<serde_json::Value>),
    /// Something the viewer cannot go on from.
    Failed(String),
}

pub struct Options {
    pub relay: String,
    pub env: String,
    pub api: Arc<Client>,
    pub device: DeviceId,
    pub signing: SigningKey,
    pub account: AccountKey,
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
}

/// Attaches: the sealed index, the checkpoint and the tail (off the
/// thread), then the relay. Answers once the session as stored is read.
pub async fn attach(o: Options) -> Result<Viewer, String> {
    let started = Instant::now();
    let o = Arc::new(o);
    let (key, attached) = {
        let o = o.clone();
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            let s = o.api.show_sync_session(&o.session).map_err(|e| e.to_string())?;
            let key = open_key(&s.wrapped_key, &o.session, &o.account)?;
            let index = store::open_index(&key, &o.session, &s.sealed_index)?;
            let a = store::attach(&o.api, &key, &o.session, index, o.known)?;
            Ok((key, a))
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let attach_time = started.elapsed();
    let (commands, rx) = mpsc::unbounded_channel();
    let (tx, updates) = mpsc::channel(256);
    let first = vec![Update::Attached { events: attached.events.clone(), head: attached.head }];
    let _ = tx.send(first).await;
    tokio::spawn(live(o, key, attached, rx, tx));
    Ok(Viewer { commands, updates, attach_time })
}

async fn live(o: Arc<Options>, key: SessionKey, mut at_rest: Attached, mut commands: mpsc::UnboundedReceiver<Command>, out: mpsc::Sender<Vec<Update>>) {
    let raw = crate::daemon::ws::uuid(&o.session);
    let mut seen: HashSet<String> = at_rest.events.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect();
    let mut frame: Vec<Update> = Vec::new();
    let mut queued: VecDeque<Remote> = VecDeque::new();
    let mut unacked: BTreeMap<String, Remote> = BTreeMap::new();
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut link: Option<ViewerLink> = None;
    let mut ws: Option<super::Ws> = None;
    let mut host = false;
    let mut retry = Instant::now();
    let mut heard = Instant::now();
    let mut next_id = 0u64;
    let mut tick = tokio::time::interval(FRAME);
    let mut beat = tokio::time::interval(PING);
    loop {
        tokio::select! {
            c = commands.recv() => {
                let Some(command) = c else { break };
                next_id += 1;
                let r = Remote { id: format!("{}-{next_id}", o.device), command };
                let ready = host && link.as_ref().is_some_and(|l| l.welcomed());
                frame.push(Update::Sent { id: r.id.clone(), queued: !ready });
                if ready {
                    let sealed = link.as_mut().expect("welcomed").frame(&serde_json::to_vec(&r).expect("json"), false);
                    unacked.insert(r.id.clone(), r);
                    if let (Some(w), Ok(b)) = (ws.as_mut(), sealed) && !super::send(w, b).await { ws = None; }
                } else {
                    queued.push_back(r);
                }
            }
            _ = tick.tick(), if !frame.is_empty() => {
                if out.send(std::mem::take(&mut frame)).await.is_err() { break; }
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
                        let n = joined["link"].as_u64().unwrap_or(0);
                        let mut l = match link.take() { Some(l) => l.reconnect(n), None => ViewerLink::new(&key, raw, n) };
                        host = joined["host"].as_bool().unwrap_or(false);
                        held.clear();
                        if host && let Ok(h) = l.hello(b"{}") && !super::send(&mut w, h).await { retry = Instant::now() + Duration::from_secs(1); link = Some(l); continue; }
                        link = Some(l);
                        if !host { frame.push(Update::Host(false)); }
                        ws = Some(w);
                        heard = Instant::now();
                    }
                    Err(_) => retry = Instant::now() + Duration::from_secs(1),
                }
            }
            m = async { match ws.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => {
                heard = Instant::now();
                match m {
                    In::Closed => { ws = None; retry = Instant::now() + Duration::from_secs(1); }
                    In::Alive => {}
                    In::Control(v) if v["type"] == "host" => {
                        // A host arriving may be a new bridge, with none of
                        // this connection's chains: join again and say hello.
                        let present = v["present"].as_bool().unwrap_or(false);
                        if present { ws = None; retry = Instant::now(); } else { host = false; frame.push(Update::Host(false)); }
                    }
                    In::Control(v) if v["type"] == "resync" => {
                        // The relay cannot fill the gap: the chunks can, up to
                        // what the host has written; the live stream goes on.
                        let (api, key2, id) = (o.api.clone(), key.clone(), o.session.clone());
                        let mut a = at_rest.clone();
                        if let Ok(Ok((fresh, a))) = tokio::task::spawn_blocking(move || store::catch_up(&api, &key2, &id, &mut a, None).map(|f| (f, a))).await {
                            let fresh: Vec<_> = fresh.into_iter().filter(|e| e["id"].as_str().is_some_and(|i| seen.insert(i.to_string()))).collect();
                            at_rest = a;
                            if !fresh.is_empty() { frame.push(Update::CaughtUp(fresh)); }
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
                                    // The prefix check: the chunks must reach the head the host sealed.
                                    let (api, key2, id, want) = (o.api.clone(), key.clone(), o.session.clone(), w.head);
                                    let mut a = at_rest.clone();
                                    match tokio::task::spawn_blocking(move || store::catch_up(&api, &key2, &id, &mut a, want).map(|f| (f, a))).await {
                                        Ok(Ok((fresh, a))) => {
                                            let fresh: Vec<_> = fresh.into_iter().filter(|e| e["id"].as_str().is_some_and(|i| seen.insert(i.to_string()))).collect();
                                            at_rest = a;
                                            if !fresh.is_empty() { frame.push(Update::CaughtUp(fresh)); }
                                        }
                                        Ok(Err(e)) => { frame.push(Update::Failed(e)); let _ = out.send(std::mem::take(&mut frame)).await; return; }
                                        Err(_) => {}
                                    }
                                    host = true;
                                    frame.push(Update::Host(true));
                                    for r in w.approvals { frame.push(Update::Line(StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(r)))); }
                                    // What waited for a host goes now, in order.
                                    let resend: Vec<Remote> = unacked.values().cloned().collect();
                                    for r in resend.into_iter().chain(queued.drain(..)) {
                                        if let (Some(w), Ok(sealed)) = (ws.as_mut(), l.frame(&serde_json::to_vec(&r).expect("json"), false)) && super::send(w, sealed).await {
                                            unacked.insert(r.id.clone(), r);
                                        }
                                    }
                                    for b in std::mem::take(&mut held) {
                                        apply(l, &b, &mut seen, &mut frame, &mut at_rest);
                                    }
                                }
                                Ok(Outbound::Routed { body, .. }) => {
                                    if let Ok(Answer::Ack { id, error }) = serde_json::from_slice(&body) {
                                        unacked.remove(&id);
                                        frame.push(Update::Acked { id, error });
                                    }
                                }
                                Err(_) => {}
                            }
                        } else if !l.welcomed() {
                            held.push(b);
                        } else {
                            apply(l, &b, &mut seen, &mut frame, &mut at_rest);
                        }
                    }
                    In::Envelope(_) => {}
                }
            }
        }
    }
}

/// One stream batch: its lines, the log's events among them once each.
fn apply(l: &mut ViewerLink, b: &[u8], seen: &mut HashSet<String>, frame: &mut Vec<Update>, at_rest: &mut Attached) {
    let Ok((body, last)) = l.open_batch(b) else { return };
    let Ok(batch) = serde_json::from_slice::<Batch>(&body) else { return };
    let _ = at_rest;
    for line in batch.lines {
        if let StreamLine::Log(e) = &line && !seen.insert(e.id.clone()) {
            continue;
        }
        frame.push(Update::Line(line));
    }
    if last {
        frame.push(Update::Host(false));
    }
}
