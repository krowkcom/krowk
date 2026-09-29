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
//! whichever answers first decides (R-PERM-2). A request of a turn a
//! viewer started that no viewer is left to answer is denied after
//! `APPROVAL_WAIT`, so such a turn never waits on a device that never
//! comes; a turn started at the host's own terminal waits for its person.
//!
//! Losing the relay loses nothing (R-OFF-2): the turn runs on in the
//! daemon, the chunks go on being written, and every batch sealed while the
//! link was down is kept and sent once it is back, on the same stream,
//! numbered on from the last sent.

use super::store::{self, Index, Writer};
use super::{Answer, Batch, In, Join, ViewerFrame, Welcome, DEAD, FRAME, PING};
use crate::daemon::client::Client as Daemon;
use crate::protocol::{ApprovalDecision, ApprovalRequest, Command, LiveEvent, LogBody, PermissionMode, StreamLine};
use krowk_api::Client;
use krowk_client::e2e::{self, AccountKey, DeviceId, SessionKey, SigningKey};
use krowk_client::protocol::frame::{KIND_ACK, KIND_ROUTED, HEADER};
use krowk_client::relay_link::{HostLink, Inbound};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// The lease's TTL, and how often it is renewed: a third of it
/// (harness.md → The host daemon).
pub const LEASE_TTL: u64 = 60;
pub fn renew_every(ttl: u64) -> Duration {
    Duration::from_secs(ttl / 3)
}

/// How long an approval of a turn a viewer started waits, with no viewer
/// connected, before the bridge denies it. A turn started at the host's own
/// terminal is never denied by the bridge: its person answers it.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(300);

/// Batches kept for the relay while its link is down: past this the oldest
/// go, the bridge starts a new stream when it is back, and each viewer,
/// told to resync, asks the host for the logged events it lacks.
pub const KEEP: usize = 1024;

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
    pub account: AccountKey,
    pub session: String,
    pub title: String,
    pub cwd: String,
    pub ttl: u64,
    /// Batches kept for the relay while its link is down (`KEEP`).
    pub keep: usize,
    /// Direct paths, offered beside the relay (R-NET-1); None for the
    /// relay alone.
    pub direct: Option<super::direct::Config>,
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
/// lease lapsed takes it again when nobody else has. One that finds another
/// device holding it sets `lost`: the bridge then writes nothing more and
/// ends (R-SYNC-2's one writer).
fn renew_loop(o: Arc<Options>, held: Arc<Mutex<Held>>, stop: Arc<AtomicBool>, lost: Arc<AtomicBool>) {
    let every = renew_every(o.ttl);
    let mut next = Instant::now() + every;
    while !stop.load(Ordering::Relaxed) && !lost.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100).min(next.saturating_duration_since(Instant::now())));
        if Instant::now() < next {
            continue;
        }
        let h = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let device = o.device.to_string();
        let answer = match o.api.renew_lease(&o.session, &device, &h.token, o.ttl, &o.env) {
            Err(e) if e.code().contains("lease") => o.api.acquire_lease(&o.session, &device, o.ttl, &o.env),
            other => other,
        };
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
            Err(e) if e.code() == "lease_held" => lost.store(true, Ordering::Relaxed),
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

/// How often a write the registry refused is tried again.
const RETRY: Duration = Duration::from_secs(2);

fn writer_loop(mut w: Writer, held: Arc<Mutex<Held>>, rx: std::sync::mpsc::Receiver<Write>, heads: mpsc::UnboundedSender<store::Head>, lost: Arc<AtomicBool>) {
    loop {
        let job = match rx.recv_timeout(RETRY) {
            Ok(j) => Some(j),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // A lease another device holds now: nothing more is written.
        if lost.load(Ordering::Relaxed) {
            continue;
        }
        let h = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let done = w.refence(h.fence).and_then(|()| match job {
            Some(Write::Event(e)) => {
                w.push(e);
                Ok(())
            }
            Some(Write::Flush) => w.flush(&h.token),
            Some(Write::Checkpoint(tree)) => w.checkpoint(tree, &h.token),
            // What was owed is put again, the same chunk under the same index.
            None if w.owes() => w.flush(&h.token),
            None => Ok(()),
        });
        if let Err(e) = done {
            eprintln!("krowk: a chunk of the session could not be written yet ({e}); it is kept and put again");
        }
        if let Some(head) = w.head() {
            let _ = heads.send(head);
        }
    }
    // Stopping: what is owed gets a last few tries.
    for _ in 0..3 {
        if !w.owes() || lost.load(Ordering::Relaxed) {
            break;
        }
        let h = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if w.flush(&h.token).is_err() {
            std::thread::sleep(RETRY);
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

/// A viewer's prompt runs under the session's own settings, never its own:
/// the model and effort the session last ran on, the permission mode its
/// last turn ran in, the configured toolset and no budget of the viewer's
/// choosing. A viewer cannot pick `unhinged`, switch the model or lift a
/// budget through a prompt; the person at the host's machine sets those.
fn under_session_settings(c: Command, mode: PermissionMode) -> Command {
    match c {
        Command::Prompt { session_id, text, .. } => Command::Prompt { session_id, text, model: None, permission_mode: mode, toolset: None, effort: None, budget: None },
        other => other,
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

/// Commands run, by their viewer's id, with the ack each was answered:
/// bounded, the oldest let go first.
const REMEMBERED: usize = 4096;

/// Logged events a catch-up page holds.
const PAGE: usize = 256;

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
    let halt = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    {
        let (o, held, halt, lost) = (o.clone(), held.clone(), halt.clone(), lost.clone());
        std::thread::spawn(move || renew_loop(o, held, halt, lost));
    }
    let (heads_tx, mut heads) = mpsc::unbounded_channel();
    let mut head = writer.head();
    let (jobs, jobs_rx) = std::sync::mpsc::channel();
    let writing = {
        let (held, lost) = (held.clone(), lost.clone());
        std::thread::spawn(move || writer_loop(writer, held, jobs_rx, heads_tx, lost))
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
    // Every event logged since the bridge followed the session, in order:
    // what a viewer's catch-up is answered from.
    let mut log: Vec<crate::protocol::LogEvent> = Vec::new();
    let mut logged = HashSet::new();
    let mut mode = PermissionMode::Default;
    // Turns a viewer's prompt started: the only ones whose approvals the
    // bridge ever denies by itself.
    let remote_turns: Arc<Mutex<HashSet<String>>> = Arc::default();
    let mut done: HashMap<String, Answer> = HashMap::new();
    let mut done_order: VecDeque<String> = VecDeque::new();
    let mut running: HashSet<String> = HashSet::new();
    // Since when no viewer has been here to answer.
    let mut alone_since = Some(Instant::now());
    // Links the relay said are present since the host last joined; chains
    // of any other link are forgotten a second after the join.
    let mut present: Option<(HashSet<u64>, Instant)> = None;
    let mut ws: Option<super::Ws> = None;
    let mut heard = Instant::now();
    let mut retry = Instant::now();
    // The direct listener, when tailscaled gives this machine an address:
    // a second uplink, sent every batch the relay is, never in its place.
    let listening = match o.direct.as_ref().map(|c| super::direct::listen(c, crate::daemon::ws::uuid(&o.session), o.device)) {
        Some(Ok(l)) => Some(l),
        Some(Err(e)) => {
            eprintln!("krowk: no direct path ({e}); the session goes by the relay");
            None
        }
        None => None,
    };
    let candidates = listening.as_ref().map(|l| l.candidates.clone()).unwrap_or_default();
    let mut dws: Option<super::Ws> = None;
    let mut dheard = Instant::now();
    let mut dretry = Instant::now();
    let (answers_tx, mut answers) = mpsc::unbounded_channel::<(u64, Answer)>();
    let mut tick = tokio::time::interval(FRAME);
    let mut beat = tokio::time::interval(PING);
    let mut sweep = tokio::time::interval(Duration::from_secs(1));
    let mut ended = Ok(());
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
                    match &e.body {
                        LogBody::TurnCompleted { .. } => { let _ = jobs.send(Write::Flush); }
                        LogBody::TurnStarted { permission_mode, .. } => mode = *permission_mode,
                        _ => {}
                    }
                    log.push(e.clone());
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
                if kept.len() > o.keep.max(1) { kept.pop_front(); }
                if let Some(w) = dws.as_mut() && !super::send(w, sealed.clone()).await { dws = None; dretry = Instant::now() + Duration::from_secs(1); }
                if let Some(w) = ws.as_mut() && !super::send(w, sealed).await { ws = None; }
            }
            Some(h) = heads.recv() => head = Some(h),
            Some((to, a)) = answers.recv() => {
                if let Answer::Ack { id, .. } = &a && !id.is_empty() {
                    running.remove(id);
                    if done.insert(id.clone(), a.clone()).is_none() {
                        done_order.push_back(id.clone());
                        if done_order.len() > REMEMBERED && let Some(old) = done_order.pop_front() { done.remove(&old); }
                    }
                }
                let sock = if to >= super::direct::FIRST_LINK { &mut dws } else { &mut ws };
                if let Some(w) = sock.as_mut() && let Ok(b) = link.to_viewer(to, &serde_json::to_vec(&a).expect("json"), false) && !super::send(w, b).await { *sock = None; }
            }
            _ = on_demand.recv() => { let _ = jobs.send(Write::Checkpoint(worktree(&o.cwd))); }
            _ = sweep.tick() => {
                if lost.load(Ordering::Relaxed) {
                    ended = Err(format!("another device holds session {}'s lease now; this machine stopped syncing it", o.session));
                    break;
                }
                if let Some((here, since)) = &present && since.elapsed() > Duration::from_secs(1) {
                    for l in link.links() { if !here.contains(&l) { link.forget(l); } }
                    present = None;
                }
                if link.viewers().is_empty() { alone_since.get_or_insert_with(Instant::now); } else { alone_since = None; }
                // Only a request of a turn a viewer started, and only once no
                // viewer has been here to answer it for the whole wait: the
                // person at the host's own terminal is never overruled.
                let turns = remote_turns.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let late: Vec<_> = approvals.iter().filter(|(_, (r, t))| turns.contains(&r.turn_id) && alone_since.is_some_and(|a| a.elapsed() > APPROVAL_WAIT) && t.elapsed() > APPROVAL_WAIT).map(|(k, (r, _))| (k.clone(), r.clone())).collect();
                for (k, r) in late {
                    approvals.remove(&k);
                    let (tx, _) = mpsc::channel(1);
                    let _ = daemon.execute(Command::Approve { session_id: r.session_id, request_id: r.request_id, decision: ApprovalDecision::Deny }, tx).await;
                }
            }
            _ = beat.tick() => {
                if let Some(w) = ws.as_mut() && (heard.elapsed() > DEAD || !super::ping(w).await) { ws = None; }
                if let Some(w) = dws.as_mut() && (dheard.elapsed() > DEAD || !super::ping(w).await) { dws = None; }
            }
            _ = tokio::time::sleep_until(dretry.into()), if dws.is_none() && listening.is_some() => {
                // The direct listener, joined as the relay is: the same
                // ticket, the same stream. What the relay has not yet
                // acknowledged seeds it, so a viewer moving over resumes
                // from its cursor there.
                let ticket = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let dial = listening.as_ref().expect("listening").dial.clone();
                let j = Join { relay: &dial, session: &o.session, env: &o.env, ticket: &ticket.ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_HOST, extra: json!({"fence": ticket.fence, "stream": e2e::hex(&link.stream())}) };
                dretry = Instant::now() + Duration::from_secs(2);
                if let Ok((mut w, joined)) = super::join(j).await && link.continues_after(joined["seq"].as_u64().unwrap_or(0)) {
                    let at = joined["seq"].as_u64().unwrap_or(0);
                    let mut ok = true;
                    for (_, b) in kept.iter().filter(|(s, _)| *s > at) {
                        if !super::send(&mut w, b.clone()).await { ok = false; break; }
                    }
                    if ok { dws = Some(w); dheard = Instant::now(); }
                }
            }
            _ = tokio::time::sleep_until(retry.into()), if ws.is_none() => {
                let ticket = held.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let j = Join { relay: &o.relay, session: &o.session, env: &o.env, ticket: &ticket.ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_HOST, extra: json!({"fence": ticket.fence, "stream": e2e::hex(&link.stream())}) };
                match super::join(j).await {
                    Ok((mut w, joined)) => {
                        let at = joined["seq"].as_u64().unwrap_or(0);
                        // A relay holding more of this stream than it was sent, or
                        // less than the oldest batch still kept (a cut outlasted
                        // what is kept): this stream cannot follow on there, so
                        // start another. Nothing is lost with the old one's
                        // batches: every viewer is told `resync`, joins again,
                        // and asks the host for the logged events it lacks.
                        if !link.continues_after(at) || (at > 0 && kept.front().is_some_and(|(s, _)| *s > at + 1)) {
                            link = HostLink::new(&key, raw);
                            kept.clear();
                            drop(w);
                            retry = Instant::now();
                            continue;
                        }
                        let mut ok = true;
                        for (_, b) in kept.iter().filter(|(s, _)| *s > at) {
                            if !super::send(&mut w, b.clone()).await { ok = false; break; }
                        }
                        if ok {
                            ws = Some(w);
                            heard = Instant::now();
                            present = Some((HashSet::new(), Instant::now()));
                        } else {
                            retry = Instant::now() + Duration::from_secs(1);
                        }
                    }
                    Err(_) => retry = Instant::now() + Duration::from_secs(1),
                }
            }
            (direct, m) = async {
                tokio::select! {
                    m = async { match ws.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => (false, m),
                    m = async { match dws.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => (true, m),
                }
            } => {
                if direct { dheard = Instant::now(); } else { heard = Instant::now(); }
                let sock = if direct { &mut dws } else { &mut ws };
                match m {
                    In::Closed if direct => { dws = None; dretry = Instant::now() + Duration::from_secs(1); }
                    In::Closed => { ws = None; retry = Instant::now() + Duration::from_secs(1); }
                    In::Alive => {}
                    In::Control(v) => {
                        if v["type"] == "viewer" && let Some(l) = v["link"].as_u64() {
                            if v["event"] == "left" { link.forget(l); }
                            if v["event"] == "joined" && let Some((here, _)) = present.as_mut() { here.insert(l); }
                        }
                    }
                    // The direct listener's acks free nothing: what is kept
                    // is kept for the relay.
                    In::Envelope(b) if b[1] == KIND_ACK && !direct => {
                        let upto = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
                        while kept.front().is_some_and(|(s, _)| *s <= upto) { kept.pop_front(); }
                    }
                    In::Envelope(b) if b[1] == KIND_ROUTED => {
                        let from = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
                        match link.open(from, &b[HEADER..]) {
                            Ok(Inbound::Hello { .. }) => {
                                let body = serde_json::to_vec(&Welcome { head, approvals: approvals.values().map(|(r, _)| r.clone()).collect(), candidates: candidates.clone() }).expect("json");
                                if let (Some(w), Ok(sealed)) = (sock.as_mut(), link.welcome(from, &body)) && !super::send(w, sealed).await { *sock = None; }
                            }
                            Ok(Inbound::Frame { body, .. }) => match serde_json::from_slice::<ViewerFrame>(&body) {
                                Ok(ViewerFrame::CatchUp { after }) => {
                                    let from_here = after.and_then(|a| log.iter().position(|e| e.id == a)).map_or(0, |i| i + 1);
                                    let rest = &log[from_here..];
                                    let pages: Vec<_> = rest.chunks(PAGE).collect();
                                    if pages.is_empty() {
                                        let _ = answers_tx.send((from, Answer::CatchUp { events: Vec::new(), more: false }));
                                    }
                                    for (i, p) in pages.iter().enumerate() {
                                        let _ = answers_tx.send((from, Answer::CatchUp { events: p.to_vec(), more: i + 1 < pages.len() }));
                                    }
                                }
                                // Run once: a command sent again (its ack lost, the
                                // viewer reconnected) is answered with the ack it
                                // had, or nothing while it is still being taken.
                                Ok(ViewerFrame::Command(r)) if done.contains_key(&r.id) => {
                                    let _ = answers_tx.send((from, done[&r.id].clone()));
                                }
                                Ok(ViewerFrame::Command(r)) if running.contains(&r.id) => {}
                                Ok(ViewerFrame::Command(r)) if !for_this_session(&r.command, &o.session) => {
                                    let _ = answers_tx.send((from, Answer::Ack { id: r.id, error: Some(format!("a viewer of session {} may prompt, steer, interrupt or answer an approval of that session, and nothing else", o.session)) }));
                                }
                                Ok(ViewerFrame::Command(r)) => {
                                    running.insert(r.id.clone());
                                    let command = under_session_settings(r.command, mode);
                                    let (daemon, answers, tx, turns) = (daemon.clone(), answers_tx.clone(), lines_tx.clone(), remote_turns.clone());
                                    tokio::spawn(async move {
                                        let streaming = matches!(command, Command::Prompt { .. });
                                        let (mine, mut rx) = mpsc::channel(1024);
                                        // The turn's lines go where every other line
                                        // goes, the turn remembered as a viewer's.
                                        tokio::spawn(async move {
                                            while let Some(l) = rx.recv().await {
                                                if let StreamLine::Log(e) = &l && let LogBody::TurnStarted { turn_id, .. } = &e.body {
                                                    turns.lock().unwrap_or_else(|e| e.into_inner()).insert(turn_id.clone());
                                                }
                                                if tx.send(l).await.is_err() { return; }
                                            }
                                        });
                                        let run = daemon.execute(command, mine);
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
                                Err(e) => { let _ = answers_tx.send((from, Answer::Ack { id: String::new(), error: Some(format!("not a frame this host reads: {e}")) })); }
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
    if let Ok(end) = link.batch(&serde_json::to_vec(&Batch { lines: std::mem::take(&mut waiting), head }).expect("json"), true) {
        if let Some(w) = dws.as_mut() {
            let _ = super::send(w, end.clone()).await;
        }
        if let Some(w) = ws.as_mut() {
            let _ = super::send(w, end).await;
        }
    }
    drop(listening);
    let _ = jobs.send(Write::Flush);
    drop(jobs);
    halt.store(true, Ordering::Relaxed);
    // The lease goes back, so the next host takes it at once rather than
    // after its TTL, once the writer has put its last chunk — unless it is
    // another device's already.
    let token = held.lock().unwrap_or_else(|e| e.into_inner()).token.clone();
    let (o2, lost2) = (o.clone(), lost.clone());
    let _ = tokio::task::spawn_blocking(move || {
        let _ = writing.join();
        if !lost2.load(Ordering::Relaxed) {
            let _ = o2.api.release_lease(&o2.session, &token);
        }
    })
    .await;
    ended
}

/// Waits for Ctrl-C or SIGTERM: `krowk sync host` then ends the stream with
/// its final sealed batch and lets the lease go, rather than leaving
/// viewers to find the stream cut short and the lease to lapse.
pub async fn interrupted() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let t = async {
        match term.as_mut() {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = t => {} }
}
