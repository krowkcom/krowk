//! The host's side of a synced session: a client of the host daemon that
//! follows one session, holds its lease, streams its frames over the relay
//! and writes its log as chunks (engineering/harness.md → Sync).
//!
//! It is the session's one writer at the registry (R-SYNC-2): it takes the
//! lease, renews it every third of its TTL (20 seconds for 60) and keeps the
//! host ticket each renewal carries for its next join. The daemon stays the
//! one thing that runs turns: what a viewer asks — a prompt, a steer, an
//! interrupt, an approval — the bridge executes on the daemon as any client
//! would, and answers the viewer with an ack. It says it answers approvals,
//! so a request reaches the viewers as it reaches the terminal, and
//! whichever answers first decides (R-PERM-2); one nobody answers within
//! `APPROVAL_WAIT` is denied, so a turn never waits on a device that never
//! comes.
//!
//! Losing the relay loses nothing (R-OFF-2): the turn runs on in the
//! daemon, the chunks go on being written, and every batch sealed while the
//! link was down is kept and sent once it is back, on the same stream,
//! numbered on from the last sent.

use super::store::{self, Index, Writer};
use super::{Answer, Batch, In, Join, Remote, Welcome, DEAD, FRAME, PING};
use crate::daemon::client::Client as Daemon;
use crate::protocol::{ApprovalDecision, ApprovalRequest, Command, LiveEvent, LogBody, StreamLine};
use krowk_api::Client;
use krowk_client::e2e::{self, AccountKey, DeviceId, SessionKey, SigningKey};
use krowk_client::protocol::frame::{KIND_ACK, KIND_ROUTED, HEADER};
use krowk_client::relay_link::{HostLink, Inbound};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// The lease's TTL, and how often it is renewed: a third of it
/// (harness.md → The host daemon).
pub const LEASE_TTL: u64 = 60;
pub fn renew_every(ttl: u64) -> Duration {
    Duration::from_secs(ttl / 3)
}

/// How long an approval waits for any client before it is denied.
/// Only while no viewer is connected: a request the terminal on the host's
/// own machine is showing waits for its person as it always did.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(300);

/// Batches kept for the relay while its link is down: past this the oldest
/// go, and a viewer resuming across them is told to resync and reads them
/// from the chunks.
pub const KEEP: usize = 1024;

pub struct Options {
    pub relay: String,
    pub env: String,
    pub api: Arc<Client>,
    pub device: DeviceId,
    pub signing: SigningKey,
    pub account: AccountKey,
    pub session: String,
    pub title: String,
    pub cwd: String,
    pub ttl: u64,
}

/// The lease as the bridge holds it: the token only the holder has, its
/// fence, and the freshest host ticket.
#[derive(Clone, Default)]
struct Held {
    token: String,
    fence: u64,
    ticket: String,
}

/// Opens (or creates) the synced session and takes its lease: the session
/// key, the writer at the log's end, and the lease.
fn take(o: &Options) -> Result<(SessionKey, Writer, Held), String> {
    let id = &o.session;
    let raw = crate::daemon::ws::uuid(id);
    let (key, wrapped, index) = match o.api.show_sync_session(id) {
        Ok(s) => {
            let key = e2e::unwrap_session_key(&e2e::unhex(&s.wrapped_key).ok_or("the session's wrapped key is not hex")?, &raw, &o.account).map_err(|e| e.to_string())?;
            let index = store::open_index(&key, id, &s.sealed_index)?;
            (key, s.wrapped_key, index)
        }
        Err(_) => {
            let key = SessionKey::generate();
            let wrapped = e2e::hex(&e2e::wrap_session_key(&key, &raw, &o.account));
            let index = Index { title: o.title.clone(), cwd: o.cwd.clone(), ..Index::default() };
            let sealed = e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).expect("json")));
            o.api.put_sync_session(id, &wrapped, Some(&sealed), None).map_err(|e| e.to_string())?;
            (key, wrapped, index)
        }
    };
    let lease = o.api.acquire_lease(id, &o.device.to_string(), o.ttl, &o.env).map_err(|e| e.to_string())?;
    if lease.relay_ticket.is_empty() {
        return Err("the registry issued no host ticket: register this device's signing key (krowk sync register)".into());
    }
    let writer = Writer::take_up(o.api.clone(), key.clone(), id, wrapped, index, lease.fence)?;
    Ok((key, writer, Held { token: lease.token, fence: lease.fence, ticket: lease.relay_ticket }))
}

/// Renews the lease every third of its TTL, on a thread of its own (the
/// registry client blocks), keeping the latest ticket in `held`. A renewal
/// that fails is tried again every two seconds; one refused because the
/// lease lapsed takes it again.
fn renew_loop(o: Arc<Options>, held: Arc<Mutex<Held>>, stop: Arc<std::sync::atomic::AtomicBool>) {
    let every = renew_every(o.ttl);
    let mut next = Instant::now() + every;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100).min(next.saturating_duration_since(Instant::now())));
        if Instant::now() < next {
            continue;
        }
        let h = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let device = o.device.to_string();
        let answer = o.api.renew_lease(&o.session, &device, &h.token, o.ttl, &o.env).or_else(|e| if e.to_string().contains("lease") { o.api.acquire_lease(&o.session, &device, o.ttl, &o.env) } else { Err(e) });
        match answer {
            Ok(l) => {
                let mut h = held.lock().unwrap_or_else(|e| e.into_inner());
                if !l.token.is_empty() {
                    h.token = l.token;
                }
                h.fence = l.fence;
                if !l.relay_ticket.is_empty() {
                    h.ticket = l.relay_ticket;
                }
                next = Instant::now() + every;
            }
            Err(_) => next = Instant::now() + Duration::from_secs(2),
        }
    }
}

/// What the writer thread is asked to do, in order.
enum Write {
    Event(Value),
    Flush,
    Checkpoint(Option<String>),
}

fn writer_loop(mut w: Writer, held: Arc<Mutex<Held>>, rx: std::sync::mpsc::Receiver<Write>, heads: mpsc::UnboundedSender<store::Head>) {
    for job in rx {
        let h = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
        w.refence(h.fence);
        let done = match job {
            Write::Event(e) => {
                w.push(e);
                Ok(())
            }
            Write::Flush => w.flush(&h.token),
            Write::Checkpoint(tree) => w.checkpoint(tree, &h.token),
        };
        if let Err(e) = done {
            // The events stay waiting and go with the next flush.
            eprintln!("krowk: a chunk of the session could not be written ({e}); it goes with the next");
        }
        if let Some(head) = w.head() {
            let _ = heads.send(head);
        }
    }
}

/// What a viewer may ask of the host: its session's prompt, steer,
/// interrupt and approval, and nothing else. A viewer holds this session's
/// key and no other, so a command naming another session — or none, which
/// would start one in the host's directory — is refused, never run.
fn for_this_session(c: &Command, session: &str) -> bool {
    match c {
        Command::Prompt { session_id, .. } => session_id.as_deref() == Some(session),
        Command::Steer { session_id, .. } | Command::Interrupt { session_id } | Command::Approve { session_id, .. } => session_id == session,
        _ => false,
    }
}

fn worktree(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git").args(["-C", cwd, "rev-parse", "HEAD"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A handle on a running bridge: `checkpoint` cuts one on demand, `stop`
/// ends the stream with its final sealed batch and lets the lease go.
pub struct Bridge {
    pub stop: watch::Sender<bool>,
    pub checkpoint: mpsc::UnboundedSender<()>,
}

/// Runs the bridge until `stop`. `daemon` is a client of the daemon that
/// says it answers approvals.
pub async fn run(o: Options, daemon: Arc<Daemon>, mut stop: watch::Receiver<bool>, mut on_demand: mpsc::UnboundedReceiver<()>) -> Result<(), String> {
    let o = Arc::new(o);
    let taken = {
        let o = o.clone();
        tokio::task::spawn_blocking(move || take(&o)).await.map_err(|e| e.to_string())??
    };
    let (key, writer, held) = taken;
    let held = Arc::new(Mutex::new(held));
    let halt = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (o, held, halt) = (o.clone(), held.clone(), halt.clone());
        std::thread::spawn(move || renew_loop(o, held, halt));
    }
    let (heads_tx, mut heads) = mpsc::unbounded_channel();
    let mut head = writer.head();
    let (jobs, jobs_rx) = std::sync::mpsc::channel();
    let writing = {
        let held = held.clone();
        std::thread::spawn(move || writer_loop(writer, held, jobs_rx, heads_tx))
    };
    // A checkpoint as the bridge takes the session up, so a device
    // attaching reads one chunk and the tail, not the whole log.
    let _ = jobs.send(Write::Checkpoint(worktree(&o.cwd)));

    let (lines_tx, mut lines) = mpsc::channel(4096);
    daemon.attach(&o.session, None, lines_tx.clone()).await.map_err(|e| e.to_string())?;
    let raw = crate::daemon::ws::uuid(&o.session);
    let mut link = HostLink::new(&key, raw);
    let mut kept: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    let mut waiting: Vec<StreamLine> = Vec::new();
    let mut approvals: BTreeMap<String, (ApprovalRequest, Instant)> = BTreeMap::new();
    let mut logged = std::collections::HashSet::new();
    let mut ws: Option<super::Ws> = None;
    let mut heard = Instant::now();
    let mut retry = Instant::now();
    let (answers_tx, mut answers) = mpsc::unbounded_channel::<(u64, Answer)>();
    let mut tick = tokio::time::interval(FRAME);
    let mut beat = tokio::time::interval(PING);
    let mut sweep = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                if let StreamLine::Log(e) = &line {
                    // A line both followed and executed arrives once in the log.
                    if !logged.insert(e.id.clone()) {
                        continue;
                    }
                    let _ = jobs.send(Write::Event(serde_json::to_value(e).expect("json")));
                    if matches!(e.body, LogBody::TurnCompleted { .. }) {
                        let _ = jobs.send(Write::Flush);
                    }
                }
                match &line {
                    StreamLine::Live(LiveEvent::ApprovalRequested(r)) => { approvals.insert(r.request_id.clone(), (r.clone(), Instant::now())); }
                    StreamLine::Live(LiveEvent::ApprovalResolved { request_id, .. }) => { approvals.remove(request_id); }
                    _ => {}
                }
                waiting.push(line);
            }
            _ = tick.tick(), if !waiting.is_empty() => {
                let body = serde_json::to_vec(&Batch { lines: std::mem::take(&mut waiting), head }).expect("json");
                let sealed = link.batch(&body, false).map_err(|e| e.to_string())?;
                kept.push_back((link.last_seq(), sealed.clone()));
                if kept.len() > KEEP { kept.pop_front(); }
                if let Some(w) = ws.as_mut() && !super::send(w, sealed).await { ws = None; }
            }
            Some(h) = heads.recv() => head = Some(h),
            Some((to, a)) = answers.recv() => {
                if let Some(w) = ws.as_mut() && let Ok(b) = link.to_viewer(to, &serde_json::to_vec(&a).expect("json"), false) && !super::send(w, b).await { ws = None; }
            }
            _ = on_demand.recv() => { let _ = jobs.send(Write::Checkpoint(worktree(&o.cwd))); }
            _ = sweep.tick() => {
                // Only while no viewer is here to answer: a person at A's own
                // terminal answering slowly is not overruled.
                let late: Vec<_> = approvals.iter().filter(|(_, (_, t))| link.viewers().is_empty() && t.elapsed() > APPROVAL_WAIT).map(|(k, (r, _))| (k.clone(), r.clone())).collect();
                for (k, r) in late {
                    approvals.remove(&k);
                    let (tx, _) = mpsc::channel(1);
                    let _ = daemon.execute(Command::Approve { session_id: r.session_id, request_id: r.request_id, decision: ApprovalDecision::Deny }, tx).await;
                }
            }
            _ = beat.tick() => {
                if let Some(w) = ws.as_mut() && (heard.elapsed() > DEAD || !super::ping(w).await) { ws = None; }
            }
            _ = tokio::time::sleep_until(retry.into()), if ws.is_none() => {
                let ticket = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let j = Join { relay: &o.relay, session: &o.session, env: &o.env, ticket: &ticket.ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_HOST, extra: json!({"fence": ticket.fence, "stream": e2e::hex(&link.stream())}) };
                match super::join(j).await {
                    Ok((mut w, joined)) => {
                        let at = joined["seq"].as_u64().unwrap_or(0);
                        // A relay holding more of this stream than it was sent, or
                        // holding less than the oldest batch still kept (a cut
                        // outlasted what is kept): this stream cannot follow on
                        // there, so start another, and viewers read the gap from
                        // the chunks.
                        if !link.continues_after(at) || (at > 0 && kept.front().is_some_and(|(s, _)| *s > at + 1)) {
                            // A relay holding more of this stream than it was sent: start another.
                            link = HostLink::new(&key, raw);
                            kept.clear();
                            drop(w);
                            retry = Instant::now();
                            continue;
                        }
                        let mut ok = true;
                        for (seq, b) in kept.iter().filter(|(s, _)| *s > at) {
                            let _ = seq;
                            if !super::send(&mut w, b.clone()).await { ok = false; break; }
                        }
                        if ok { ws = Some(w); heard = Instant::now(); } else { retry = Instant::now() + Duration::from_secs(1); }
                    }
                    Err(_) => retry = Instant::now() + Duration::from_secs(1),
                }
            }
            m = async { match ws.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => {
                heard = Instant::now();
                match m {
                    In::Closed => { ws = None; retry = Instant::now() + Duration::from_secs(1); }
                    In::Alive => {}
                    In::Control(v) => {
                        if v["type"] == "viewer" && v["event"] == "left" && let Some(l) = v["link"].as_u64() { link.forget(l); }
                    }
                    In::Envelope(b) if b[1] == KIND_ACK => {
                        let upto = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
                        while kept.front().is_some_and(|(s, _)| *s <= upto) { kept.pop_front(); }
                    }
                    In::Envelope(b) if b[1] == KIND_ROUTED => {
                        let from = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
                        match link.open(from, &b[HEADER..]) {
                            Ok(Inbound::Hello { .. }) => {
                                let body = serde_json::to_vec(&Welcome { head, approvals: approvals.values().map(|(r, _)| r.clone()).collect() }).expect("json");
                                if let (Some(w), Ok(sealed)) = (ws.as_mut(), link.welcome(from, &body)) && !super::send(w, sealed).await { ws = None; }
                            }
                            Ok(Inbound::Frame { body, .. }) => match serde_json::from_slice::<Remote>(&body) {
                                Ok(r) if !for_this_session(&r.command, &o.session) => {
                                    let _ = answers_tx.send((from, Answer::Ack { id: r.id, error: Some(format!("a viewer of session {} may prompt, steer, interrupt or answer an approval of that session, and nothing else", o.session)) }));
                                }
                                Ok(r) => {
                                    let (daemon, answers, tx) = (daemon.clone(), answers_tx.clone(), lines_tx.clone());
                                    tokio::spawn(async move {
                                        let streaming = matches!(r.command, Command::Prompt { .. } | Command::Continue { .. });
                                        let run = daemon.execute(r.command, tx);
                                        if streaming {
                                            // A prompt is acknowledged as taken, not when its turn ends.
                                            let _ = answers.send((from, Answer::Ack { id: r.id, error: None }));
                                            let _ = run.await;
                                        } else {
                                            let error = run.await.err().map(|e| e.to_string());
                                            let _ = answers.send((from, Answer::Ack { id: r.id, error }));
                                        }
                                    });
                                }
                                Err(e) => { let _ = answers_tx.send((from, Answer::Ack { id: String::new(), error: Some(format!("not a command: {e}")) })); }
                            },
                            // What does not open is dropped: the relay or a stranger sent it.
                            Err(_) => {}
                        }
                    }
                    In::Envelope(_) => {}
                }
            }
            _ = stop.changed() => break,
        }
    }
    // The stream's end, sealed: a viewer that sees the link stop without it
    // knows it was cut short.
    if let Some(w) = ws.as_mut() && let Ok(end) = link.batch(&serde_json::to_vec(&Batch { lines: std::mem::take(&mut waiting), head }).expect("json"), true) {
        let _ = super::send(w, end).await;
    }
    let _ = jobs.send(Write::Flush);
    drop(jobs);
    halt.store(true, std::sync::atomic::Ordering::Relaxed);
    // The lease goes back, so the next host takes it at once rather than
    // after its TTL, once the writer has put its last chunk.
    let token = held.lock().unwrap_or_else(|e| e.into_inner()).token.clone();
    let o2 = o.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = writing.join();
        o2.api.release_lease(&o2.session, &token)
    })
    .await;
    Ok(())
}
