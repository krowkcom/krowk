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
use krowk_client::e2e::{self, DeviceId, SessionKey, SigningKey};
use krowk_client::device_chain::Chain;
use krowk_client::session_record;
use krowk_client::user_key::UserKeys;
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
    /// The person's user key, by the generations this device holds: a new
    /// session's key is sealed under the newest, an existing one opened
    /// under the generation it names.
    pub keys: UserKeys,
    /// The device list as this device verified it: a session record is
    /// taken up only when a device on it signed it, and a new session is
    /// sealed only under the generation it names as current.
    pub chain: Chain,
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
        // Any of the person's devices may take a session up again — the
        // handoff — but only one whose record a device listed now signed: a
        // removed device's, planted under a generation it held, is never
        // written under (`store::open_session_key`).
        Ok(s) => {
            let key = store::open_session_key(&s, id, &o.keys, &o.chain, session_record::Signer::Listed)?;
            let index = store::open_index(&key, id, &s.sealed_index)?;
            (key, s.wrapped_key, index)
        }
        // Published only when the registry has no session under the id; a
        // failed write leaves it with none, and the next host publishes
        // afresh.
        Err(e) if e.status == 404 => {
            let key = SessionKey::generate();
            let sealed_key = e2e::seal_session_key(&key, &raw, &o.keys, o.chain.generation()).map_err(|e| e.to_string())?;
            let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, o.keys.newest(), &o.signing).map_err(|e| e.to_string())?;
            let wrapped = e2e::hex(&sealed_key);
            let index = Index { title: o.title.clone(), cwd: o.cwd.clone(), ..Index::default() };
            let sealed = e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).expect("json")));
            o.api.put_sync_session(id, &wrapped, Some((&e2e::hex(&signature), &o.device.to_string())), Some(&sealed), None).map_err(|e| e.to_string())?;
            (key, wrapped, index)
        }
        Err(e) => return Err(format!("the registry did not say whether it holds session {id} ({e}) — nothing was published; try again")),
    };
    let lease = o.api.acquire_lease(id, &o.device.to_string(), o.ttl, &o.env).map_err(|e| e.to_string())?;
    if lease.relay_ticket.is_empty() {
        return Err("the registry issued no host ticket: this device has no signing key on record — pair it again with `krowk sync join`".into());
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

/// How many batches past the relay's last ack make a link that still
/// answers its heartbeats a dead one: a relay acks at least every 8th batch
/// a link sends it, viewers or none (relay.md → Flow control), so a tail
/// of fewer is never waited on.
const GHOST_UNACKED: usize = 8;
/// How long the oldest of them must have waited first: well past the 2
/// seconds a relay may take to hold a batch.
const GHOST_WAIT: Duration = Duration::from_secs(10);

/// Whether a link is let go at a heartbeat, to be joined again: the beat
/// came `gap` after the last, so this process did not run in between for
/// longer than a relay keeps a silent link; nothing has been heard on it
/// for `heard`; or it is a ghost, `unacked` batches past the relay's last
/// ack, the oldest `waited` ago.
fn link_dead(gap: Duration, heard: Duration, unacked: usize, waited: Option<Duration>) -> bool {
    gap > DEAD || heard > DEAD || (unacked >= GHOST_UNACKED && waited.is_some_and(|w| w > GHOST_WAIT))
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
/// the model and effort the session last ran on, the configured toolset
/// and no budget of the viewer's choosing. Its permission mode is the one
/// the session's last turn ran in, capped at `default`: a mode that asks
/// less (`acceptEdits`, `bypassPermissions`, `unhinged`) is the host
/// person's choice for their own turns, and a remote turn never runs looser
/// than `default`. `plan`, which asks more, is kept.
fn under_session_settings(c: Command, mode: PermissionMode) -> Command {
    let mode = if mode == PermissionMode::Plan { PermissionMode::Plan } else { PermissionMode::Default };
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
// Legacy: the bridge's whole run loop, one select over every source. TODO: split into helpers and drop this allow.
#[allow(clippy::cognitive_complexity)]
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
    // Since when the relay has acked none of what `kept` holds.
    let mut unacked_since: Option<Instant> = None;
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
    // Commands being taken, by id, with the link that last sent each: a
    // viewer that moved between the relay and a direct path while one ran
    // is answered where it is now, not on the link it left.
    let mut running: HashMap<String, u64> = HashMap::new();
    // Since when no viewer has been here to answer.
    let mut alone_since = Some(Instant::now());
    // Links the relay said are present since the host last joined; chains
    // of any other link are forgotten a second after the join.
    let mut present: Option<(HashSet<u64>, Instant)> = None;
    // The same for the direct listener, which speaks for its own links.
    let mut dpresent: Option<(HashSet<u64>, Instant)> = None;
    let mut ws: Option<super::Ws> = None;
    let mut heard = Instant::now();
    let mut retry = Instant::now();
    // The last reason the relay could not be joined, said once on stderr
    // when it first comes or changes, not every second: a host that never
    // reaches the relay otherwise looks exactly like one nobody watches.
    let mut unjoined: Option<String> = None;
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
    let mut beaten = Instant::now();
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
                unacked_since.get_or_insert_with(Instant::now);
                if let Some(w) = dws.as_mut() && !super::send(w, sealed.clone()).await { dws = None; dretry = Instant::now() + Duration::from_secs(1); }
                if let Some(w) = ws.as_mut() && !super::send(w, sealed).await { ws = None; }
            }
            Some(h) = heads.recv() => head = Some(h),
            Some((mut to, a)) = answers.recv() => {
                if let Answer::Ack { id, .. } = &a && !id.is_empty() {
                    if let Some(now) = running.remove(id) { to = now; }
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
                // Each uplink speaks for its own links only: a viewer on the
                // direct path is not one the relay could have named, nor the
                // other way round.
                if let Some((here, since)) = &present && since.elapsed() > Duration::from_secs(1) {
                    for l in link.links() { if l < super::direct::FIRST_LINK && !here.contains(&l) { link.forget(l); } }
                    present = None;
                }
                if let Some((here, since)) = &dpresent && since.elapsed() > Duration::from_secs(1) {
                    for l in link.links() { if l >= super::direct::FIRST_LINK && !here.contains(&l) { link.forget(l); } }
                    dpresent = None;
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
                // A beat far later than `PING`: this process did not run in
                // between (stopped, suspended, a laptop asleep) for longer than a
                // relay keeps a silent link. What it holds may be a link the relay
                // let go, or one it no longer counts as the host, with a viewer's
                // command lost on it: joined again, and the viewers, told the
                // host is present, send again what was never acknowledged.
                //
                // And a link that answers its heartbeats while the relay acks
                // none of what the host sends (`link_dead`): a ghost, which
                // neither an error nor a close will ever end.
                let gap = beaten.elapsed();
                beaten = Instant::now();
                let waited = unacked_since.map(|t| t.elapsed());
                if let Some(w) = ws.as_mut() && (link_dead(gap, heard.elapsed(), kept.len(), waited) || !super::ping(w).await) { ws = None; }
                if let Some(w) = dws.as_mut() && (link_dead(gap, dheard.elapsed(), 0, None) || !super::ping(w).await) { dws = None; }
            }
            _ = tokio::time::sleep_until(dretry.into()), if dws.is_none() && listening.is_some() => {
                // The listener's viewers went with the host's link to it,
                // however it was lost: none of them is here to answer, so a
                // remote turn's approval is denied after `APPROVAL_WAIT`
                // rather than waiting on a chain nobody holds.
                for l in link.links() { if l >= super::direct::FIRST_LINK { link.forget(l); } }
                dpresent = None;
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
                    if ok { dws = Some(w); dheard = Instant::now(); dpresent = Some((HashSet::new(), Instant::now())); }
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
                            unjoined = None;
                            ws = Some(w);
                            heard = Instant::now();
                            // What the relay says it holds is as good as acked:
                            // what `kept` still counts is only what went out on
                            // this link, which the relay's ack count starts from.
                            while kept.front().is_some_and(|(s, _)| *s <= at) { kept.pop_front(); }
                            unacked_since = (!kept.is_empty()).then(Instant::now);
                            present = Some((HashSet::new(), Instant::now()));
                        } else {
                            retry = Instant::now() + Duration::from_secs(1);
                        }
                    }
                    Err(e) => {
                        if unjoined.as_deref() != Some(e.as_str()) {
                            eprintln!("krowk: session {} is not on the relay {}: {e}; trying again every second", o.session, o.relay);
                            unjoined = Some(e);
                        }
                        retry = Instant::now() + Duration::from_secs(1);
                    }
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
                    // A refusal comes before the relay closes the link — `replaced`,
                    // `link_overflow`, `rate_limited` — and the close itself may never
                    // arrive, while the edge goes on answering this link's heartbeats:
                    // the link is let go here, and joined again, rather than kept as a
                    // host connection the relay no longer counts.
                    In::Control(v) if v["type"] == "error" => {
                        let why = format!("the relay let this host's link go: {} — {}; joining again", v["code"].as_str().unwrap_or("?"), v["message"].as_str().unwrap_or(""));
                        if unjoined.as_deref() != Some(why.as_str()) {
                            eprintln!("krowk: session {}: {why}", o.session);
                            unjoined = Some(why);
                        }
                        if direct { dws = None; dretry = Instant::now() + Duration::from_secs(1); } else { ws = None; retry = Instant::now() + Duration::from_secs(1); }
                    }
                    In::Control(v) => {
                        if v["type"] == "viewer" && let Some(l) = v["link"].as_u64() {
                            if v["event"] == "left" { link.forget(l); }
                            if v["event"] == "joined" && let Some((here, _)) = if direct { dpresent.as_mut() } else { present.as_mut() } { here.insert(l); }
                        }
                    }
                    // The direct listener's acks free nothing: what is kept
                    // is kept for the relay.
                    In::Envelope(b) if b[1] == KIND_ACK && !direct => {
                        let upto = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
                        while kept.front().is_some_and(|(s, _)| *s <= upto) { kept.pop_front(); }
                        unacked_since = (!kept.is_empty()).then(Instant::now);
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
                                Ok(ViewerFrame::Command(r)) if running.contains_key(&r.id) => { running.insert(r.id, from); }
                                Ok(ViewerFrame::Command(r)) if !for_this_session(&r.command, &o.session) => {
                                    let _ = answers_tx.send((from, Answer::Ack { id: r.id, error: Some(format!("a viewer of session {} may prompt, steer, interrupt or answer an approval of that session, and nothing else", o.session)) }));
                                }
                                // A rule for the whole project is written into the
                                // host's repository settings: made at the host, not
                                // from a viewer, which allows once or for the session.
                                Ok(ViewerFrame::Command(r)) if matches!(r.command, Command::Approve { decision: ApprovalDecision::AllowProject, .. }) => {
                                    let _ = answers_tx.send((from, Answer::Ack { id: r.id, error: Some("a viewer allows a call once or for the session; a rule for the project is made on the host".into()) }));
                                }
                                Ok(ViewerFrame::Command(r)) => {
                                    running.insert(r.id.clone(), from);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(mode: PermissionMode) -> Command {
        Command::Prompt { session_id: Some("s".into()), text: "t".into(), model: None, permission_mode: mode, toolset: None, effort: None, budget: None }
    }

    /// R-PERM-2: a remote prompt never runs looser than `default`, whatever
    /// it asks for and whatever the session last ran in.
    #[test]
    fn r_perm_2_a_remote_prompt_runs_no_looser_than_default() {
        for session in [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::BypassPermissions, PermissionMode::Unhinged] {
            for asked in [PermissionMode::Unhinged, PermissionMode::BypassPermissions, PermissionMode::Default] {
                let Command::Prompt { permission_mode, .. } = under_session_settings(prompt(asked), session) else { unreachable!() };
                assert_eq!(permission_mode, PermissionMode::Default, "session {session:?}, asked {asked:?}");
            }
        }
        let Command::Prompt { permission_mode, .. } = under_session_settings(prompt(PermissionMode::Unhinged), PermissionMode::Plan) else { unreachable!() };
        assert_eq!(permission_mode, PermissionMode::Plan, "plan asks more, and stays");
    }

    /// A link is let go at a beat after a pause past `DEAD`, after silence
    /// past it, or as a ghost: eight batches the relay has not acked, the
    /// oldest past `GHOST_WAIT`. Fewer, or not waited on as long, is a
    /// relay still holding them.
    #[test]
    fn a_paused_silent_or_ghost_link_is_let_go() {
        let (s, now) = (Duration::from_secs, Duration::ZERO);
        assert!(!link_dead(PING, now, 0, None), "a live link");
        assert!(link_dead(DEAD + s(1), now, 0, None), "a beat after a pause, the laptop asleep");
        assert!(link_dead(PING, DEAD + s(1), 0, None), "nothing heard");
        assert!(link_dead(PING, now, GHOST_UNACKED, Some(GHOST_WAIT + s(1))), "a ghost: heartbeats answered, nothing acked");
        assert!(!link_dead(PING, now, GHOST_UNACKED - 1, Some(s(60))), "fewer unacked");
        assert!(!link_dead(PING, now, GHOST_UNACKED, Some(s(2))), "not waited on long");
    }

    use krowk_client::device_chain::{Change, Kind, Subject};
    use krowk_client::user_key::UserKey;

    const T0: u64 = 1_790_000_000;

    struct Dev {
        key: e2e::DeviceKey,
        signing: SigningKey,
    }

    impl Dev {
        fn new() -> Dev {
            Dev { key: e2e::DeviceKey::generate(), signing: SigningKey::generate() }
        }
        fn subject(&self, name: &str) -> Subject {
            Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: self.key.public(), signing: self.signing.public() }
        }
        fn signing(&self) -> SigningKey {
            SigningKey::from_secret(&*self.signing.secret_bytes()).unwrap()
        }
    }

    /// A laptop and a desktop on one person's list, generation 1.
    fn list(laptop: &Dev, desktop: &Dev) -> (Chain, UserKey) {
        let (chain, start) = Chain::start(laptop.subject("laptop"), &laptop.signing, None, T0).unwrap();
        let (chain, _) = chain.batch(&start.newest, vec![Change::Add(desktop.subject("desktop"))], laptop.key.id(), &laptop.signing, T0 + 1).unwrap();
        (chain, start.newest)
    }

    /// `d` on the stand-in registry at `url`, hosting `session`.
    fn options(url: &str, d: &Dev, keys: UserKeys, chain: Chain, session: &str) -> Options {
        let signer = e2e::DeviceSigner::new(d.key.id(), d.signing()).shared();
        let api = Arc::new(Client::new(url, "krowk_sk_sync_host_take_000000000000").signed_by(signer));
        api.register_device(&e2e::hex(&d.key.public().0), &e2e::hex(&d.signing.public().0), "host", &"0".repeat(32)).unwrap();
        Options { relay: String::new(), env: "development".into(), api, device: d.key.id(), signing: d.signing(), keys, chain, session: session.into(), title: "t".into(), cwd: String::new(), ttl: LEASE_TTL, keep: KEEP, direct: None }
    }

    fn registry() -> (krowk_devregistry::Running, String) {
        let reg = krowk_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        let url = format!("{}/v1", reg.url());
        (reg, url)
    }

    /// D8b: a session the laptop published is taken up again on the
    /// desktop — the handoff, with no local record of it there — because a
    /// device on the list signed its record.
    #[test]
    fn d8b_another_listed_device_takes_up_a_session_the_first_published() {
        let (_reg, url) = registry();
        let (laptop, desktop) = (Dev::new(), Dev::new());
        let (chain, user) = list(&laptop, &desktop);
        let id = "01a0ec7b-3333-7000-8000-0000000000d1";
        let keys = || UserKeys::new(user.clone(), []).unwrap();
        let o = options(&url, &laptop, keys(), chain.clone(), id);
        let (key, _, held) = take(&o).unwrap();
        let s = o.api.show_sync_session(id).unwrap();
        assert_eq!(s.signer, laptop.key.id().to_string(), "the record names its publisher");
        o.api.release_lease(id, &held.token).unwrap();
        let (again, _, _) = take(&options(&url, &desktop, keys(), chain, id)).unwrap();
        assert_eq!(key.as_bytes(), again.as_bytes());
    }

    /// D8b (and M1 of #200's review): a record the registry holds unsigned,
    /// or signed by a device not on the list, is never taken up — a removed
    /// device's key planted under a generation it held is the residual the
    /// per-session key epochs ticket closes.
    #[test]
    fn d8b_the_host_never_takes_up_a_record_no_listed_device_signed() {
        let (_reg, url) = registry();
        let (laptop, desktop) = (Dev::new(), Dev::new());
        let (chain, user) = list(&laptop, &desktop);
        let o = |id: &str| options(&url, &laptop, UserKeys::new(user.clone(), []).unwrap(), chain.clone(), id);
        let raw = |id: &str| crate::daemon::ws::uuid(id);
        let planted = |id: &str| e2e::wrap_session_key(&SessionKey::generate(), &raw(id), &user);

        let unsigned = "01a0ec7b-3333-7000-8000-0000000000d2";
        let host = o(unsigned);
        host.api.put_sync_session(unsigned, &e2e::hex(&planted(unsigned)), None, None, None).unwrap();
        assert!(take(&host).err().unwrap().contains("names no signer"));

        let foreign = "01a0ec7b-3333-7000-8000-0000000000d3";
        let stranger = Dev::new();
        let w = planted(foreign);
        let sig = session_record::sign(&raw(foreign), &w, session_record::SEAL_USER, &user, &stranger.signing).unwrap();
        let host = o(foreign);
        host.api.put_sync_session(foreign, &e2e::hex(&w), Some((&e2e::hex(&sig), &stranger.key.id().to_string())), None, None).unwrap();
        assert!(take(&host).err().unwrap().contains("never held"));

        // Signed by a listed device's key, but named as another's.
        let misnamed = "01a0ec7b-3333-7000-8000-0000000000d4";
        let w = planted(misnamed);
        let sig = session_record::sign(&raw(misnamed), &w, session_record::SEAL_USER, &user, &stranger.signing).unwrap();
        let host = o(misnamed);
        host.api.put_sync_session(misnamed, &e2e::hex(&w), Some((&e2e::hex(&sig), &desktop.key.id().to_string())), None, None).unwrap();
        assert!(take(&host).err().unwrap().contains("does not verify"));
    }

    /// D8b (a minor of #200's review): a publish the registry refused
    /// leaves nothing behind that stops the next host publishing again —
    /// there is no record of it but the registry's own.
    #[test]
    fn d8b_a_refused_publish_is_published_again() {
        let config = krowk_devregistry::Config { max_sessions: 1, ..Default::default() };
        let _reg = krowk_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), config).unwrap();
        let url = format!("{}/v1", _reg.url());
        let (laptop, desktop) = (Dev::new(), Dev::new());
        let (chain, user) = list(&laptop, &desktop);
        let o = |id: &str| options(&url, &laptop, UserKeys::new(user.clone(), []).unwrap(), chain.clone(), id);
        take(&o("01a0ec7b-3333-7000-8000-0000000000d5")).unwrap();
        let second = o("01a0ec7b-3333-7000-8000-0000000000d6");
        for _ in 0..2 {
            let refused = take(&second).err().expect("the registry is full");
            assert!(refused.contains("session_limit_reached"), "each host tries the publish afresh: {refused}");
            assert_eq!(second.api.show_sync_session(&second.session).unwrap_err().status, 404);
        }
    }

    /// D8b (M1 of #206's review, #200's M1 restored): thief T, on the list
    /// at generation 1, is removed and the key rotates to 2. The registry
    /// serves, for the id the laptop hosts, T's session key wrapped under
    /// generation 1 — which the laptop opens down the chain — signed by T.
    /// The host does not take it up: nothing is sealed under a key T holds.
    #[test]
    fn d8b_the_host_never_takes_up_a_removed_devices_planted_record() {
        let (_reg, url) = registry();
        let (laptop, thief) = (Dev::new(), Dev::new());
        let (chain, g1) = list(&laptop, &thief);
        let (chain, removed) = chain.batch(&g1, vec![Change::Remove(thief.subject("desktop"))], laptop.key.id(), &laptop.signing, T0 + 2).unwrap();
        let g2 = removed.newest;
        let keys = UserKeys::new(g2.clone(), [g2.wrap_previous(&g1).unwrap()]).unwrap();
        let id = "01a0ec7b-3333-7000-8000-0000000000d7";
        let raw = crate::daemon::ws::uuid(id);
        let thiefs = SessionKey::generate();
        let planted = e2e::wrap_session_key(&thiefs, &raw, &g1);
        let sig = session_record::sign(&raw, &planted, session_record::SEAL_USER, &g1, &thief.signing).unwrap();
        let o = options(&url, &laptop, keys, chain, id);
        o.api.put_sync_session(id, &e2e::hex(&planted), Some((&e2e::hex(&sig), &thief.key.id().to_string())), None, None).unwrap();
        assert!(e2e::unwrap_session_key(&planted, &raw, &o.keys).is_ok(), "the planted key opens down the chain");
        let refused = take(&o).err().expect("a removed device's record is not taken up");
        assert!(refused.contains("no longer on your device list"), "{refused}");
        assert_eq!(o.api.show_sync_session(id).unwrap().wrapped_key, e2e::hex(&planted), "nothing was written over it");
    }

    /// D8 (M2 of #200's review): a host behind the generation the list
    /// names publishes nothing.
    #[test]
    fn d8_a_host_behind_the_chains_generation_publishes_nothing() {
        let (_reg, url) = registry();
        let (laptop, desktop) = (Dev::new(), Dev::new());
        let (chain, user) = list(&laptop, &desktop);
        let (chain, _) = chain.batch(&user, vec![Change::Remove(desktop.subject("desktop"))], laptop.key.id(), &laptop.signing, T0 + 2).unwrap();
        let id = "01a0ec7b-4444-7000-8000-0000000000d8";
        let o = options(&url, &laptop, UserKeys::new(user, []).unwrap(), chain, id);
        let refused = take(&o).err().expect("generation 1 seals nothing when the list names 2");
        assert!(refused.contains("names generation 2"), "{refused}");
        assert!(o.api.show_sync_session(id).is_err(), "nothing reached the registry");
    }
}
