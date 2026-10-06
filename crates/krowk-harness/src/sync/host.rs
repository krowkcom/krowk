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
use krowk_client::e2e::{self, DeviceId, SessionKey, SessionKeys, SigningKey};
use krowk_client::device_chain::{Chain, SignedEntry};
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
    /// This machine's tailnet node, kept in the session's sealed index so
    /// `krowk hosts` finds it on the tailnet (R-NET-4); None when Tailscale
    /// is down.
    pub tailnet: Option<super::tailscale::Tailnet>,
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
/// keys, the writer at the log's end, and the lease.
fn take(o: &Options) -> Result<(SessionKeys, Writer, Held), String> {
    let id = &o.session;
    let raw = crate::daemon::ws::uuid(id);
    let (keys, wrapped, index) = match o.api.show_sync_session(id) {
        // Any of the person's devices may take a session up again — the
        // handoff — but only one whose record a device listed now signed: a
        // removed device's, planted under a generation it held, is never
        // written under (`store::open_session_key`).
        Ok(s) => {
            let keys = store::open_session_key(&s, id, &o.keys, &o.chain, session_record::Signer::Listed)?;
            let index = Index { host: Some(hosted_on(o)), ..store::open_index(&keys, id, &s.sealed_index)? };
            (keys, s.wrapped_key, index)
        }
        // Published only when the registry has no session under the id; a
        // failed write leaves it with none, and the next host publishes
        // afresh.
        Err(e) if e.status == 404 => {
            let keys = SessionKeys::from(SessionKey::generate());
            let sealed_key = e2e::seal_session_keys(&keys, &raw, &o.keys, o.chain.generation()).map_err(|e| e.to_string())?;
            let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, o.keys.newest(), &o.signing).map_err(|e| e.to_string())?;
            let wrapped = e2e::hex(&sealed_key);
            let index = Index { title: o.title.clone(), cwd: o.cwd.clone(), host: Some(hosted_on(o)), ..Index::default() };
            let sealed = e2e::hex(&e2e::seal_session_index(keys.current(), &raw, &serde_json::to_vec(&index).expect("json")));
            o.api.put_sync_session(id, &wrapped, Some((&e2e::hex(&signature), &o.device.to_string())), Some(&sealed), None).map_err(|e| e.to_string())?;
            (keys, wrapped, index)
        }
        Err(e) => return Err(format!("the registry did not say whether it holds session {id} ({e}) — nothing was published; try again")),
    };
    let lease = o.api.acquire_lease(id, &o.device.to_string(), o.ttl, &o.env).map_err(|e| e.to_string())?;
    if lease.relay_ticket.is_empty() {
        return Err("the registry issued no host ticket: this device has no signing key on record — pair it again with `krowk sync join`".into());
    }
    let (keys, wrapped) = rotate_if_behind(o, keys, wrapped, &index, &lease.token)?;
    let writer = Writer::take_up(o.api.clone(), keys.clone(), id, wrapped, index, lease.fence)?;
    Ok((keys, writer, Held { token: lease.token, fence: lease.fence, ticket: lease.relay_ticket }))
}

/// This machine as the index names its host: what the index is next
/// written with, so `krowk hosts` lists the session under it.
fn hosted_on(o: &Options) -> store::HostedOn {
    store::HostedOn { device: o.device.to_string(), tailnet: o.tailnet.clone() }
}

/// A session sealed under an older user key generation than the device
/// list's has lived through a device's removal, and that device holds its
/// key. Before anything more is written, the host adds a key epoch: a new
/// key, current from here on, with the old ones kept to open what came
/// before, the ring sealed under the current generation and its record and
/// index written again with it, in one write under the lease. A rotation
/// the registry refuses leaves the session unhosted: nothing more is sealed
/// under a key a removed device holds.
fn rotate_if_behind(o: &Options, keys: SessionKeys, wrapped: String, index: &Index, token: &str) -> Result<(SessionKeys, String), String> {
    let generation = e2e::unhex(&wrapped).ok_or("the session's wrapped key is not hex").and_then(|w| e2e::session_key_generation(&w).map_err(|_| "the session's wrapped key is not one krowk reads"))?;
    if generation >= o.chain.generation() {
        return Ok((keys, wrapped));
    }
    let raw = crate::daemon::ws::uuid(&o.session);
    let rotated = keys.rotated().map_err(|e| e.to_string())?;
    let sealed_key = e2e::seal_session_keys(&rotated, &raw, &o.keys, o.chain.generation()).map_err(|e| e.to_string())?;
    let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, o.keys.newest(), &o.signing).map_err(|e| e.to_string())?;
    let wrapped = e2e::hex(&sealed_key);
    let sealed = e2e::hex(&e2e::seal_session_index(rotated.current(), &raw, &serde_json::to_vec(index).expect("json")));
    o.api
        .put_sync_session(&o.session, &wrapped, Some((&e2e::hex(&signature), &o.device.to_string())), Some(&sealed), Some(token))
        .map_err(|e| format!("session {} is sealed under user key generation {generation}, from before a device was removed, and the registry refused its new key ({e}) — nothing is written under the old one; try again", o.session))?;
    Ok((rotated, wrapped))
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

/// How often a running host reads the device list again: the list it
/// verified when it started is not the list for good, and a device removed
/// while it runs must not go on reading what it seals.
const LIST_EVERY: Duration = Duration::from_secs(60);

/// What a running host ends with when the list moved on under it and a
/// device was removed: `krowk sync host` takes the session up again, under
/// the new list and the user key it leaves current.
pub const LIST_MOVED: &str = "a device was removed from your device list while this host ran";

/// The list read again past the head `chain` holds and verified onto it:
/// `None` when nothing came, or the registry could not be reached (asked
/// again later), the longer chain when it only added devices, and why the
/// host must stop otherwise — a removal, this device's own, or entries that
/// do not verify.
fn list_news(o: &Options, chain: &Chain) -> Result<Option<Chain>, String> {
    let session = &o.session;
    let removed = || format!("this device was removed from your device list; it stopped hosting session {session}");
    let refused = |e: String| format!("your device list changed in a way this host does not follow ({e}); it stopped hosting session {session} — host it again");
    let mut next = chain.clone();
    let mut after = chain.head().seq;
    loop {
        let served = match o.api.device_list(Some(after)) {
            Ok(s) => s,
            Err(e) if e.code() == "device_revoked" => return Err(removed()),
            // A removal revokes the device's keys, so this may be one; the
            // key refused is all that is known, and all that is said.
            Err(e) if e.status == 401 => return Err(format!("the registry refused this machine's key — it may have been removed from your devices, or signed out; it stopped hosting session {session}")),
            Err(_) => return Ok(None),
        };
        // A list that ends before the head held, or holds another entry
        // there, is a new one: a start-over.
        if let Some(h) = &served.head
            && (h.seq < chain.head().seq || (h.seq == chain.head().seq && e2e::unhex(&h.hash).as_deref() != Some(&chain.head().hash[..])))
        {
            return Err(format!("your device list was started over; it stopped hosting session {session}"));
        }
        let from = next.head().seq;
        for e in served.entries.iter().filter(|e| e.seq > from) {
            let entry = e2e::unhex(&e.entry).zip(e2e::unhex(&e.signatures)).ok_or_else(|| refused("an entry is not hex".into()))?;
            let signed = SignedEntry::from_parts(entry.0, &entry.1).map_err(|e| refused(e.0))?;
            next = next.extend(&signed).map_err(|e| refused(e.0))?;
        }
        match served.next {
            Some(n) if n > after && !served.entries.is_empty() => after = n,
            _ => break,
        }
    }
    if next.head() == chain.head() {
        return Ok(None);
    }
    if !next.devices().iter().any(|d| d.id() == o.device) {
        return Err(removed());
    }
    if next.generation() > chain.generation() {
        return Err(format!("{LIST_MOVED}; session {session} is hosted again under the new list"));
    }
    Ok(Some(next))
}

/// Reads the device list again every `LIST_EVERY`, on a thread of its own
/// so a slow read never holds up the lease's renewal, until the bridge
/// stops or the list says it must (`stale`).
fn list_loop(o: Arc<Options>, stop: Arc<AtomicBool>, stale: Arc<Mutex<Option<String>>>) {
    let mut chain = o.chain.clone();
    let mut next = Instant::now() + LIST_EVERY;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100).min(next.saturating_duration_since(Instant::now())));
        if Instant::now() < next {
            continue;
        }
        next = Instant::now() + LIST_EVERY;
        match list_news(&o, &chain) {
            Ok(Some(longer)) => chain = longer,
            Ok(None) => {}
            Err(why) => {
                *stale.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                return;
            }
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
/// the model and effort the session last ran on, the configured toolset
/// and no budget of the viewer's choosing. Its permission mode is the one
/// the session's last turn ran in, capped at `default`: a mode that asks
/// less (`acceptEdits`, `bypassPermissions`, `unhinged`) is the host
/// person's choice for their own turns, and a remote turn never runs looser
/// than `default`. `plan`, which asks more, is kept.
fn under_session_settings(c: Command, mode: PermissionMode) -> Command {
    let mode = if mode == PermissionMode::Plan { PermissionMode::Plan } else { PermissionMode::Default };
    match c {
        Command::Prompt { session_id, text, images, .. } => Command::Prompt { session_id, text, images, model: None, permission_mode: mode, toolset: None, effort: None, budget: None },
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
    // Set when the device list moved on under the host: why it stops.
    let stale = Arc::new(Mutex::new(None));
    {
        let (o, held, halt, lost) = (o.clone(), held.clone(), halt.clone(), lost.clone());
        std::thread::spawn(move || renew_loop(o, held, halt, lost));
    }
    {
        let (o, halt, stale) = (o.clone(), halt.clone(), stale.clone());
        std::thread::spawn(move || list_loop(o, halt, stale));
    }
    let (heads_tx, mut heads) = mpsc::unbounded_channel();
    let head = writer.head();
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
    let link = HostLink::new(key.current(), raw);
    let listening = listen_direct(&o);
    let candidates = listening.as_ref().map(|l| l.candidates.clone()).unwrap_or_default();
    let (answers_tx, mut answers) = mpsc::unbounded_channel::<(u64, Answer)>();
    let mut h = Hosting {
        o: o.clone(),
        daemon,
        key,
        raw,
        link,
        held: held.clone(),
        lost: lost.clone(),
        stale,
        jobs,
        lines_tx,
        answers_tx,
        head,
        kept: VecDeque::new(),
        unacked_since: None,
        waiting: Vec::new(),
        approvals: BTreeMap::new(),
        log: Vec::new(),
        logged: HashSet::new(),
        mode: PermissionMode::Default,
        remote_turns: Arc::default(),
        done: HashMap::new(),
        done_order: VecDeque::new(),
        running: HashMap::new(),
        alone_since: Some(Instant::now()),
        present: None,
        dpresent: None,
        ws: None,
        heard: Instant::now(),
        retry: Instant::now(),
        unjoined: None,
        listening,
        candidates,
        unreached: Default::default(),
        dws: None,
        dheard: Instant::now(),
        dretry: Instant::now(),
    };
    let mut tick = tokio::time::interval(FRAME);
    let mut beat = tokio::time::interval(PING);
    let mut beaten = Instant::now();
    let mut sweep = tokio::time::interval(Duration::from_secs(1));
    let mut ended = Ok(());
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                h.line(line);
            }
            _ = tick.tick(), if !h.waiting.is_empty() => h.send_batch().await?,
            Some(head) = heads.recv() => h.head = Some(head),
            Some((to, a)) = answers.recv() => h.answer(to, a).await,
            _ = on_demand.recv() => { let _ = h.jobs.send(Write::Checkpoint(worktree(&h.o.cwd))); }
            _ = sweep.tick() => {
                if let Err(e) = h.sweep().await {
                    ended = Err(e);
                    break;
                }
            }
            _ = beat.tick() => h.beat(&mut beaten).await,
            _ = tokio::time::sleep_until(h.dretry.into()), if h.dws.is_none() && h.listening.is_some() => h.join_direct().await,
            _ = tokio::time::sleep_until(h.retry.into()), if h.ws.is_none() => h.join_relay().await,
            (direct, m) = recv_either(&mut h.ws, &mut h.dws) => h.inbound(direct, m).await,
            _ = stop.changed() => break,
        }
    }
    h.stop(&halt, writing).await;
    ended
}

/// The direct listener, when tailscaled gives this machine an address:
/// a second uplink, sent every batch the relay is, never in its place.
fn listen_direct(o: &Options) -> Option<super::direct::Listening> {
    match o.direct.as_ref().map(|c| super::direct::listen(c, crate::daemon::ws::uuid(&o.session), o.device)) {
        Some(Ok(l)) => Some(l),
        Some(Err(e)) => {
            eprintln!("krowk: no direct path ({e}); the session goes by the relay");
            None
        }
        None => None,
    }
}

/// The next message on either uplink: whether it came on the direct one,
/// and what it was.
async fn recv_either(ws: &mut Option<super::Ws>, dws: &mut Option<super::Ws>) -> (bool, In) {
    tokio::select! {
        m = async { match ws.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => (false, m),
        m = async { match dws.as_mut() { Some(w) => super::recv(w).await, None => std::future::pending().await } } => (true, m),
    }
}

/// The bridge as it runs: the session's lines sealed into batches for the
/// relay and, when there is one, the direct path, and its viewers'
/// frames answered.
struct Hosting {
    o: Arc<Options>,
    daemon: Arc<Daemon>,
    key: SessionKeys,
    raw: [u8; 16],
    link: HostLink,
    held: Arc<Mutex<Held>>,
    lost: Arc<AtomicBool>,
    /// Set when the device list moved on under the host: why it stops.
    stale: Arc<Mutex<Option<String>>>,
    jobs: std::sync::mpsc::Sender<Write>,
    lines_tx: mpsc::Sender<StreamLine>,
    answers_tx: mpsc::UnboundedSender<(u64, Answer)>,
    head: Option<store::Head>,
    kept: VecDeque<(u64, Vec<u8>)>,
    /// Since when the relay has acked none of what `kept` holds.
    unacked_since: Option<Instant>,
    waiting: Vec<StreamLine>,
    approvals: BTreeMap<String, (ApprovalRequest, Instant)>,
    /// Every event logged since the bridge followed the session, in order:
    /// what a viewer's catch-up is answered from.
    log: Vec<crate::protocol::LogEvent>,
    logged: HashSet<String>,
    mode: PermissionMode,
    /// Turns a viewer's prompt started: the only ones whose approvals the
    /// bridge ever denies by itself.
    remote_turns: Arc<Mutex<HashSet<String>>>,
    done: HashMap<String, Answer>,
    done_order: VecDeque<String>,
    /// Commands being taken, by id, with the link that last sent each: a
    /// viewer that moved between the relay and a direct path while one ran
    /// is answered where it is now, not on the link it left.
    running: HashMap<String, u64>,
    /// Since when no viewer has been here to answer.
    alone_since: Option<Instant>,
    /// Links the relay said are present since the host last joined; chains
    /// of any other link are forgotten a second after the join.
    present: Option<(HashSet<u64>, Instant)>,
    /// The same for the direct listener, which speaks for its own links.
    dpresent: Option<(HashSet<u64>, Instant)>,
    ws: Option<super::Ws>,
    heard: Instant,
    retry: Instant,
    /// The last reason the relay could not be joined, said once on stderr
    /// when it first comes or changes, not every second: a host that never
    /// reaches the relay otherwise looks exactly like one nobody watches.
    unjoined: Option<String>,
    listening: Option<super::direct::Listening>,
    candidates: Vec<super::direct::Candidate>,
    /// Viewers offered the direct addresses that never reached them.
    unreached: super::direct::Unreached,
    dws: Option<super::Ws>,
    dheard: Instant,
    dretry: Instant,
}

impl Hosting {
    /// A line of the session's, for the next batch; a logged one is
    /// written to the store and kept for catch-up.
    fn line(&mut self, line: StreamLine) {
        if let StreamLine::Log(e) = &line {
            // A line both followed and executed arrives once in the log.
            if !self.logged.insert(e.id.clone()) {
                return;
            }
            let _ = self.jobs.send(Write::Event(serde_json::to_value(e).expect("json")));
            match &e.body {
                LogBody::TurnCompleted { .. } => { let _ = self.jobs.send(Write::Flush); }
                LogBody::TurnStarted { permission_mode, .. } => self.mode = *permission_mode,
                _ => {}
            }
            self.log.push(e.clone());
        }
        match &line {
            StreamLine::Live(LiveEvent::ApprovalRequested(r)) => { self.approvals.insert(r.request_id.clone(), (r.clone(), Instant::now())); }
            StreamLine::Live(LiveEvent::ApprovalResolved { request_id, .. }) => { self.approvals.remove(request_id); }
            _ => {}
        }
        self.waiting.push(line);
    }

    /// The lines waiting, sealed as one batch and sent up both uplinks.
    async fn send_batch(&mut self) -> Result<(), String> {
        let body = serde_json::to_vec(&Batch { lines: std::mem::take(&mut self.waiting), head: self.head }).expect("json");
        let sealed = self.link.batch(&body, false).map_err(|e| e.to_string())?;
        self.kept.push_back((self.link.last_seq(), sealed.clone()));
        if self.kept.len() > self.o.keep.max(1) { self.kept.pop_front(); }
        self.unacked_since.get_or_insert_with(Instant::now);
        if let Some(w) = self.dws.as_mut() && !super::send(w, sealed.clone()).await { self.dws = None; self.dretry = Instant::now() + Duration::from_secs(1); }
        if let Some(w) = self.ws.as_mut() && !super::send(w, sealed).await { self.ws = None; }
        Ok(())
    }

    /// An answer for the viewer on link `to`, remembered when it acks a
    /// command, and sent where that viewer is now.
    async fn answer(&mut self, mut to: u64, a: Answer) {
        if let Answer::Ack { id, .. } = &a && !id.is_empty() {
            if let Some(now) = self.running.remove(id) { to = now; }
            self.remember(id, &a);
        }
        let sock = if to >= super::direct::FIRST_LINK { &mut self.dws } else { &mut self.ws };
        if let Some(w) = sock.as_mut() && let Ok(b) = self.link.to_viewer(to, &serde_json::to_vec(&a).expect("json"), false) && !super::send(w, b).await { *sock = None; }
    }

    /// A command's ack, for a command sent again: bounded, the oldest let
    /// go first.
    fn remember(&mut self, id: &str, a: &Answer) {
        if self.done.insert(id.to_string(), a.clone()).is_none() {
            self.done_order.push_back(id.to_string());
            if self.done_order.len() > REMEMBERED && let Some(old) = self.done_order.pop_front() { self.done.remove(&old); }
        }
    }

    /// The bridge's second: an error once the lease is another device's;
    /// otherwise viewers gone are forgotten, and a remote turn's approval
    /// nobody is here to answer is denied.
    async fn sweep(&mut self) -> Result<(), String> {
        if self.lost.load(Ordering::Relaxed) {
            return Err(format!("another device holds session {}'s lease now; this machine stopped syncing it", self.o.session));
        }
        if let Some(why) = self.stale.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return Err(why);
        }
        // Each uplink speaks for its own links only: a viewer on the
        // direct path is not one the relay could have named, nor the
        // other way round.
        if let Some((here, since)) = &self.present && since.elapsed() > Duration::from_secs(1) {
            for l in self.link.links() { if l < super::direct::FIRST_LINK && !here.contains(&l) { self.link.forget(l); } }
            self.present = None;
        }
        if let Some((here, since)) = &self.dpresent && since.elapsed() > Duration::from_secs(1) {
            for l in self.link.links() { if l >= super::direct::FIRST_LINK && !here.contains(&l) { self.link.forget(l); } }
            self.dpresent = None;
        }
        if self.link.viewers().is_empty() { self.alone_since.get_or_insert_with(Instant::now); } else { self.alone_since = None; }
        // However the host's link to its listener went, nothing is said
        // until it is back (`join_direct`).
        if self.dws.is_none() { self.unreached.direct_down(); }
        let same_user = self.o.direct.as_ref().is_some_and(|c| c.same_user);
        // Nor while the relay link is down: a viewer that left meanwhile
        // is known only once the relay's replay leaves it out.
        let due = if self.ws.is_some() { self.unreached.due(Instant::now()) } else { Vec::new() };
        for d in due {
            eprintln!("{}", super::direct::unreached_line(&self.o.session, &d, &self.candidates, same_user));
        }
        self.deny_late().await;
        Ok(())
    }

    /// Only a request of a turn a viewer started, and only once no viewer
    /// has been here to answer it for the whole wait: the person at the
    /// host's own terminal is never overruled.
    async fn deny_late(&mut self) {
        let turns = self.remote_turns.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let alone_since = self.alone_since;
        let late: Vec<_> = self.approvals.iter().filter(|(_, (r, t))| turns.contains(&r.turn_id) && alone_since.is_some_and(|a| a.elapsed() > APPROVAL_WAIT) && t.elapsed() > APPROVAL_WAIT).map(|(k, (r, _))| (k.clone(), r.clone())).collect();
        for (k, r) in late {
            self.approvals.remove(&k);
            let (tx, _) = mpsc::channel(1);
            let _ = self.daemon.execute(Command::Approve { session_id: r.session_id, request_id: r.request_id, decision: ApprovalDecision::Deny, answers: Vec::new() }, tx).await;
        }
    }

    /// A beat far later than `PING`: this process did not run in
    /// between (stopped, suspended, a laptop asleep) for longer than a
    /// relay keeps a silent link. What it holds may be a link the relay
    /// let go, or one it no longer counts as the host, with a viewer's
    /// command lost on it: joined again, and the viewers, told the
    /// host is present, send again what was never acknowledged.
    ///
    /// And a link that answers its heartbeats while the relay acks
    /// none of what the host sends (`link_dead`): a ghost, which
    /// neither an error nor a close will ever end.
    async fn beat(&mut self, beaten: &mut Instant) {
        let gap = beaten.elapsed();
        *beaten = Instant::now();
        let waited = self.unacked_since.map(|t| t.elapsed());
        if let Some(w) = self.ws.as_mut() && (link_dead(gap, self.heard.elapsed(), self.kept.len(), waited) || !super::ping(w).await) { self.ws = None; }
        if let Some(w) = self.dws.as_mut() && (link_dead(gap, self.dheard.elapsed(), 0, None) || !super::ping(w).await) { self.dws = None; }
    }

    /// Joins the direct listener again.
    async fn join_direct(&mut self) {
        // The listener's viewers went with the host's link to it,
        // however it was lost: none of them is here to answer, so a
        // remote turn's approval is denied after `APPROVAL_WAIT`
        // rather than waiting on a chain nobody holds.
        for l in self.link.links() { if l >= super::direct::FIRST_LINK { self.link.forget(l); } }
        self.dpresent = None;
        // The direct listener, joined as the relay is: the same
        // ticket, the same stream. What the relay has not yet
        // acknowledged seeds it, so a viewer moving over resumes
        // from its cursor there.
        let ticket = self.held.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let dial = self.listening.as_ref().expect("listening").dial.clone();
        let o = &self.o;
        let j = Join { relay: &dial, session: &o.session, env: &o.env, ticket: &ticket.ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_HOST, extra: json!({"fence": ticket.fence, "stream": e2e::hex(&self.link.stream())}) };
        self.dretry = Instant::now() + Duration::from_secs(2);
        if let Ok((mut w, joined)) = super::join(j).await && self.link.continues_after(joined["seq"].as_u64().unwrap_or(0)) {
            let at = joined["seq"].as_u64().unwrap_or(0);
            if self.resend(&mut w, at).await { self.dws = Some(w); self.dheard = Instant::now(); self.dpresent = Some((HashSet::new(), Instant::now())); self.unreached.direct_up(Instant::now()); }
        }
    }

    /// Joins the relay again.
    async fn join_relay(&mut self) {
        let ticket = self.held.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let o = &self.o;
        let j = Join { relay: &o.relay, session: &o.session, env: &o.env, ticket: &ticket.ticket, device: o.device, signing: &o.signing, role: e2e::RELAY_ROLE_HOST, extra: json!({"fence": ticket.fence, "stream": e2e::hex(&self.link.stream())}) };
        match super::join(j).await {
            Ok((mut w, joined)) => {
                let at = joined["seq"].as_u64().unwrap_or(0);
                // A relay holding more of this stream than it was sent, or
                // less than the oldest batch still kept (a cut outlasted
                // what is kept): this stream cannot follow on there, so
                // start another. Nothing is lost with the old one's
                // batches: every viewer is told `resync`, joins again,
                // and asks the host for the logged events it lacks.
                if !self.link.continues_after(at) || (at > 0 && self.kept.front().is_some_and(|(s, _)| *s > at + 1)) {
                    self.link = HostLink::new(self.key.current(), self.raw);
                    self.kept.clear();
                    drop(w);
                    self.retry = Instant::now();
                    return;
                }
                if self.resend(&mut w, at).await {
                    self.unjoined = None;
                    self.ws = Some(w);
                    self.heard = Instant::now();
                    // What the relay says it holds is as good as acked:
                    // what `kept` still counts is only what went out on
                    // this link, which the relay's ack count starts from.
                    while self.kept.front().is_some_and(|(s, _)| *s <= at) { self.kept.pop_front(); }
                    self.unacked_since = (!self.kept.is_empty()).then(Instant::now);
                    self.present = Some((HashSet::new(), Instant::now()));
                    // Who is on the relay is said again by its replay; a
                    // viewer that left while this link was down is not.
                    self.unreached.relay_lost();
                } else {
                    self.retry = Instant::now() + Duration::from_secs(1);
                }
            }
            Err(e) => {
                if self.unjoined.as_deref() != Some(e.as_str()) {
                    eprintln!("krowk: session {} is not on the relay {}: {e}; trying again every second", self.o.session, self.o.relay);
                    self.unjoined = Some(e);
                }
                self.retry = Instant::now() + Duration::from_secs(1);
            }
        }
    }

    /// Sends a link just joined the batches kept after `at`; false when
    /// one could not be sent.
    async fn resend(&self, w: &mut super::Ws, at: u64) -> bool {
        for (_, b) in self.kept.iter().filter(|(s, _)| *s > at) {
            if !super::send(w, b.clone()).await { return false; }
        }
        true
    }

    /// Lets an uplink go, to be joined again in a second.
    fn let_go(&mut self, direct: bool) {
        if direct { self.dws = None; self.dretry = Instant::now() + Duration::from_secs(1); } else { self.ws = None; self.retry = Instant::now() + Duration::from_secs(1); }
    }

    /// What came on an uplink.
    async fn inbound(&mut self, direct: bool, m: In) {
        if direct { self.dheard = Instant::now(); } else { self.heard = Instant::now(); }
        match m {
            In::Closed => self.let_go(direct),
            In::Alive => {}
            // A refusal comes before the relay closes the link — `replaced`,
            // `link_overflow`, `rate_limited` — and the close itself may never
            // arrive, while the edge goes on answering this link's heartbeats:
            // the link is let go here, and joined again, rather than kept as a
            // host connection the relay no longer counts.
            In::Control(v) if v["type"] == "error" => {
                let why = format!("the relay let this host's link go: {} — {}; joining again", v["code"].as_str().unwrap_or("?"), v["message"].as_str().unwrap_or(""));
                if self.unjoined.as_deref() != Some(why.as_str()) {
                    eprintln!("krowk: session {}: {why}", self.o.session);
                    self.unjoined = Some(why);
                }
                self.let_go(direct);
            }
            In::Control(v) => {
                if v["type"] == "viewer" && let Some(l) = v["link"].as_u64() {
                    if v["event"] == "left" { self.link.forget(l); }
                    if v["event"] == "joined" && let Some((here, _)) = if direct { self.dpresent.as_mut() } else { self.present.as_mut() } { here.insert(l); }
                    // The relay names the device; anything that is not an
                    // id is no device to say anything of.
                    if let Some(d) = v["device"].as_str().and_then(DeviceId::parse) {
                        match (v["event"].as_str(), direct) {
                            (Some("joined"), false) if !self.candidates.is_empty() => self.unreached.relay_joined(l, d, Instant::now()),
                            (Some("left"), false) => self.unreached.relay_left(l),
                            (Some("joined"), true) => self.unreached.direct_joined(d),
                            _ => {}
                        }
                    }
                }
            }
            // The direct listener's acks free nothing: what is kept
            // is kept for the relay.
            In::Envelope(b) if b[1] == KIND_ACK && !direct => {
                let upto = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
                while self.kept.front().is_some_and(|(s, _)| *s <= upto) { self.kept.pop_front(); }
                self.unacked_since = (!self.kept.is_empty()).then(Instant::now);
            }
            In::Envelope(b) if b[1] == KIND_ROUTED => self.routed(direct, &b).await,
            In::Envelope(_) => {}
        }
    }

    /// A viewer's envelope, routed to this host.
    async fn routed(&mut self, direct: bool, b: &[u8]) {
        let from = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
        match self.link.open(from, &b[HEADER..]) {
            Ok(Inbound::Hello { .. }) => {
                let body = serde_json::to_vec(&Welcome { head: self.head, approvals: self.approvals.values().map(|(r, _)| r.clone()).collect(), candidates: self.candidates.clone() }).expect("json");
                let sock = if direct { &mut self.dws } else { &mut self.ws };
                if let (Some(w), Ok(sealed)) = (sock.as_mut(), self.link.welcome(from, &body)) && !super::send(w, sealed).await { *sock = None; }
            }
            Ok(Inbound::Frame { body, .. }) => self.frame(from, &body),
            // What does not open is dropped: the relay or a stranger sent it.
            Err(_) => {}
        }
    }

    /// A viewer's frame, from link `from`.
    fn frame(&mut self, from: u64, body: &[u8]) {
        match serde_json::from_slice::<ViewerFrame>(body) {
            Ok(ViewerFrame::CatchUp { after }) => self.catch_up(from, after),
            // Run once: a command sent again (its ack lost, the
            // viewer reconnected) is answered with the ack it
            // had, or nothing while it is still being taken.
            Ok(ViewerFrame::Command(r)) if self.done.contains_key(&r.id) => {
                let _ = self.answers_tx.send((from, self.done[&r.id].clone()));
            }
            Ok(ViewerFrame::Command(r)) if self.running.contains_key(&r.id) => { self.running.insert(r.id, from); }
            Ok(ViewerFrame::Command(r)) if !for_this_session(&r.command, &self.o.session) => {
                let _ = self.answers_tx.send((from, Answer::Ack { id: r.id, error: Some(format!("a viewer of session {} may prompt, steer, interrupt or answer an approval of that session, and nothing else", self.o.session)) }));
            }
            // A rule for the whole project is written into the
            // host's repository settings: made at the host, not
            // from a viewer, which allows once or for the session.
            Ok(ViewerFrame::Command(r)) if matches!(r.command, Command::Approve { decision: ApprovalDecision::AllowProject, .. }) => {
                let _ = self.answers_tx.send((from, Answer::Ack { id: r.id, error: Some("a viewer allows a call once or for the session; a rule for the project is made on the host".into()) }));
            }
            Ok(ViewerFrame::Command(r)) => self.execute(from, r),
            Err(e) => { let _ = self.answers_tx.send((from, Answer::Ack { id: String::new(), error: Some(format!("not a frame this host reads: {e}")) })); }
        }
    }

    /// The logged events after `after`, a page at a time.
    fn catch_up(&self, from: u64, after: Option<String>) {
        let from_here = after.and_then(|a| self.log.iter().position(|e| e.id == a)).map_or(0, |i| i + 1);
        let rest = &self.log[from_here..];
        let pages: Vec<_> = rest.chunks(PAGE).collect();
        if pages.is_empty() {
            let _ = self.answers_tx.send((from, Answer::CatchUp { events: Vec::new(), more: false }));
        }
        for (i, p) in pages.iter().enumerate() {
            let _ = self.answers_tx.send((from, Answer::CatchUp { events: p.to_vec(), more: i + 1 < pages.len() }));
        }
    }

    /// Runs a viewer's command on the daemon, under the session's settings.
    fn execute(&mut self, from: u64, r: super::Remote) {
        self.running.insert(r.id.clone(), from);
        let command = under_session_settings(r.command, self.mode);
        let (daemon, answers, tx, turns) = (self.daemon.clone(), self.answers_tx.clone(), self.lines_tx.clone(), self.remote_turns.clone());
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

    /// Stops the bridge: the stream ended, the listener closed, and the
    /// lease given back once the writer is done.
    async fn stop(mut self, halt: &AtomicBool, writing: std::thread::JoinHandle<()>) {
        self.seal().await;
        drop(self.listening.take());
        let _ = self.jobs.send(Write::Flush);
        drop(self.jobs);
        halt.store(true, Ordering::Relaxed);
        // The lease goes back, so the next host takes it at once rather than
        // after its TTL, once the writer has put its last chunk — unless it is
        // another device's already.
        let token = self.held.lock().unwrap_or_else(|e| e.into_inner()).token.clone();
        let (o2, lost2) = (self.o.clone(), self.lost.clone());
        let _ = tokio::task::spawn_blocking(move || {
            let _ = writing.join();
            if !lost2.load(Ordering::Relaxed) {
                let _ = o2.api.release_lease(&o2.session, &token);
            }
        })
        .await;
    }

    /// The stream's end, sealed: a viewer that sees the link stop without
    /// it knows it was cut short.
    async fn seal(&mut self) {
        if let Ok(end) = self.link.batch(&serde_json::to_vec(&Batch { lines: std::mem::take(&mut self.waiting), head: self.head }).expect("json"), true) {
            if let Some(w) = self.dws.as_mut() {
                let _ = super::send(w, end.clone()).await;
            }
            if let Some(w) = self.ws.as_mut() {
                let _ = super::send(w, end).await;
            }
        }
    }
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
        Command::Prompt { session_id: Some("s".into()), text: "t".into(), images: Vec::new(), model: None, permission_mode: mode, toolset: None, effort: None, budget: None }
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
    /// The person's key, which each device holds one of (`tok#…`).
    const TOKEN: &str = "krowk_sk_sync_host_take_000000000000";

    struct Dev {
        key: e2e::DeviceKey,
        signing: SigningKey,
        /// The key it holds, which speaks for it.
        token: String,
    }

    impl Dev {
        fn new() -> Dev {
            let key = e2e::DeviceKey::generate();
            let token = format!("{TOKEN}#{}", key.id());
            Dev { key, signing: SigningKey::generate(), token }
        }
        fn subject(&self, name: &str) -> Subject {
            Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: self.key.public(), signing: self.signing.public() }
        }
        fn signing(&self) -> SigningKey {
            SigningKey::from_secret(&*self.signing.secret_bytes()).unwrap()
        }
    }

    /// A device list post of `b`, as the chain made it.
    fn post(b: &krowk_client::device_chain::Batch) -> krowk_api::sync::ListPost {
        krowk_api::sync::ListPost {
            entries: b.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
            links: b.links.iter().map(|l| e2e::hex(l)).collect(),
            wraps: b.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
            start_over: false,
        }
    }

    /// A laptop and a desktop on one person's list, generation 1, in the
    /// registry at `url` too: the laptop starts it, which binds its key,
    /// adds the desktop, and the desktop's key claims it.
    fn list(url: &str, laptop: &Dev, desktop: &Dev) -> (Chain, UserKey) {
        let (chain, start) = Chain::start(laptop.subject("laptop"), &laptop.signing, None, T0).unwrap();
        let (chain, add) = chain.batch(&start.newest, vec![Change::Add(desktop.subject("desktop"))], laptop.key.id(), &laptop.signing, T0 + 1).unwrap();
        let as_laptop = client(url, laptop);
        as_laptop.init_device_list(&post(&start)).unwrap();
        as_laptop.append_device_list(&post(&add)).unwrap();
        client(url, desktop).claim_key_device().unwrap();
        (chain, start.newest)
    }

    /// The stand-in registry at `url` as `d`: on its own key, signed by it.
    fn client(url: &str, d: &Dev) -> Client {
        Client::new(url, &d.token).signed_by(e2e::DeviceSigner::new(d.key.id(), d.signing()).shared())
    }

    /// `d` on the stand-in registry at `url`, hosting `session`.
    fn options(url: &str, d: &Dev, keys: UserKeys, chain: Chain, session: &str) -> Options {
        let api = Arc::new(client(url, d));
        Options { relay: String::new(), env: "development".into(), api, device: d.key.id(), signing: d.signing(), keys, chain, session: session.into(), title: "t".into(), cwd: String::new(), ttl: LEASE_TTL, keep: KEEP, direct: None, tailnet: None }
    }

    fn registry() -> (krowk_devregistry::Running, String) {
        let reg = krowk_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        let url = format!("{}/v1", reg.url());
        (reg, url)
    }

    /// D5 mB: a running host reads the list again. Nothing new, or a device
    /// added, it goes on; a removal moves the key on, and it stops for
    /// `krowk sync host` to take the session up again under the new list;
    /// its own removal stops it for good.
    #[test]
    fn d5_a_running_host_learns_of_a_removal_from_the_list() {
        let (_reg, url) = registry();
        let (laptop, desktop, phone) = (Dev::new(), Dev::new(), Dev::new());
        let id = "01a0ec7b-3333-7000-8000-0000000000da";
        let (chain, start) = Chain::start(laptop.subject("laptop"), &laptop.signing, None, T0).unwrap();
        let o = options(&url, &laptop, UserKeys::new(start.newest.clone(), []).unwrap(), chain.clone(), id);
        o.api.init_device_list(&post(&start)).unwrap();
        assert!(list_news(&o, &chain).unwrap().is_none(), "nothing new");

        let (two, add) = chain.batch(&start.newest, vec![Change::Add(desktop.subject("desktop")), Change::Add(phone.subject("phone"))], laptop.key.id(), &laptop.signing, T0 + 1).unwrap();
        o.api.append_device_list(&post(&add)).unwrap();
        assert_eq!(list_news(&o, &chain).unwrap().unwrap().head(), two.head(), "an add: it goes on, with the longer list");

        let (three, removed) = two.batch(&start.newest, vec![Change::Remove(phone.subject("phone"))], laptop.key.id(), &laptop.signing, T0 + 2).unwrap();
        o.api.append_device_list(&post(&removed)).unwrap();
        assert!(list_news(&o, &two).unwrap_err().starts_with(LIST_MOVED));

        // The desktop's own key: one person's, bound to its own device.
        let signer = e2e::DeviceSigner::new(desktop.key.id(), desktop.signing()).shared();
        let d = Client::new(&url, "krowk_sk_sync_host_take_000000000000#desktop").signed_by(signer);
        let (_, gone) = three.batch(&removed.newest, vec![Change::Remove(laptop.subject("laptop"))], desktop.key.id(), &desktop.signing, T0 + 3).unwrap();
        d.append_device_list(&post(&gone)).unwrap();
        // Its keys revoked with it, the read is refused, and the host says so.
        assert!(list_news(&o, &three).unwrap_err().contains("refused this machine's key"));
    }

    /// D8b: a session the laptop published is taken up again on the
    /// desktop — the handoff, with no local record of it there — because a
    /// device on the list signed its record.
    #[test]
    fn d8b_another_listed_device_takes_up_a_session_the_first_published() {
        let (_reg, url) = registry();
        let (laptop, desktop) = (Dev::new(), Dev::new());
        let (chain, user) = list(&url, &laptop, &desktop);
        let id = "01a0ec7b-3333-7000-8000-0000000000d1";
        let keys = || UserKeys::new(user.clone(), []).unwrap();
        let o = options(&url, &laptop, keys(), chain.clone(), id);
        let (key, _, held) = take(&o).unwrap();
        let s = o.api.show_sync_session(id).unwrap();
        assert_eq!(s.signer, laptop.key.id().to_string(), "the record names its publisher");
        o.api.release_lease(id, &held.token).unwrap();
        let (again, _, _) = take(&options(&url, &desktop, keys(), chain, id)).unwrap();
        assert_eq!(key.current().as_bytes(), again.current().as_bytes());
    }

    /// D8b (and M1 of #200's review): a record the registry holds unsigned,
    /// or signed by a device not on the list, is never taken up — a removed
    /// device's key planted under a generation it held is the residual the
    /// per-session key epochs ticket closes.
    #[test]
    fn d8b_the_host_never_takes_up_a_record_no_listed_device_signed() {
        let (_reg, url) = registry();
        let (laptop, desktop) = (Dev::new(), Dev::new());
        let (chain, user) = list(&url, &laptop, &desktop);
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
        let (chain, user) = list(&url, &laptop, &desktop);
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
        let (chain, g1) = list(&url, &laptop, &thief);
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

    /// Key epochs (#200's M1 residual): a session published at generation
    /// 1, when a device since removed held its key, is taken up again at
    /// generation 2. The host adds a key epoch before it writes: the ring is
    /// sealed under 2, which the removed device does not hold, and what is
    /// written from then on is under the new key — while the log still reads
    /// whole, from chunk 0. Taken up again at 2, it does not rotate twice.
    #[test]
    fn epochs_a_session_from_before_a_removal_is_rotated_when_taken_up() {
        let (_reg, url) = registry();
        let (laptop, thief) = (Dev::new(), Dev::new());
        let (chain, g1) = list(&url, &laptop, &thief);
        let id = "01a0ec7b-3333-7000-8000-0000000000d9";
        let raw = crate::daemon::ws::uuid(id);
        let before = options(&url, &laptop, UserKeys::new(g1.clone(), []).unwrap(), chain.clone(), id);
        let (old, mut w, held) = take(&before).unwrap();
        w.push(serde_json::json!({"id": "01a0ec7b-0000-7000-8000-000000000001", "type": "item.completed"}));
        w.flush(&held.token).unwrap();
        before.api.release_lease(id, &held.token).unwrap();
        let thiefs = UserKeys::new(g1.clone(), []).unwrap();
        assert!(e2e::unwrap_session_keys(&e2e::unhex(&before.api.show_sync_session(id).unwrap().wrapped_key).unwrap(), &raw, &thiefs).is_ok(), "the thief holds the key so far");

        let (chain, removed) = chain.batch(&g1, vec![Change::Remove(thief.subject("desktop"))], laptop.key.id(), &laptop.signing, T0 + 2).unwrap();
        let g2 = removed.newest;
        let after = Options { keys: UserKeys::new(g2.clone(), [g2.wrap_previous(&g1).unwrap()]).unwrap(), chain, ..before };
        let (keys, mut w, held) = take(&after).unwrap();
        assert_eq!((keys.epoch(), keys.at(0).unwrap().as_bytes()), (1, old.current().as_bytes()), "a key added, the old one kept");
        w.push(serde_json::json!({"id": "01a0ec7b-0000-7000-8000-000000000002", "type": "item.completed"}));
        w.flush(&held.token).unwrap();
        let s = after.api.show_sync_session(id).unwrap();
        let wrapped = e2e::unhex(&s.wrapped_key).unwrap();
        assert_eq!((e2e::session_key_generation(&wrapped).unwrap(), s.signer.as_str()), (2, laptop.key.id().to_string().as_str()));
        assert!(e2e::unwrap_session_keys(&wrapped, &raw, &thiefs).unwrap_err().0.contains("generation 2"), "the removed device opens nothing new");
        let listed = after.api.list_chunks(id, None, 10).unwrap();
        assert_eq!(after.api.read_chunk(&listed.chunks[1]).unwrap()[0], e2e::CHUNK_V2, "written under the new key");
        let index = store::open_index(&keys, id, &s.sealed_index).unwrap();
        assert_eq!(store::attach(&after.api, &keys, id, index, None).unwrap().events.len(), 2, "the log reads across the rotation");
        after.api.release_lease(id, &held.token).unwrap();

        let (again, _, _) = take(&after).unwrap();
        assert_eq!(again.epoch(), 1, "rotated once");
    }

    /// D8 (M2 of #200's review): a host behind the generation the list
    /// names publishes nothing.
    #[test]
    fn d8_a_host_behind_the_chains_generation_publishes_nothing() {
        let (_reg, url) = registry();
        let (laptop, desktop) = (Dev::new(), Dev::new());
        let (chain, user) = list(&url, &laptop, &desktop);
        let (chain, _) = chain.batch(&user, vec![Change::Remove(desktop.subject("desktop"))], laptop.key.id(), &laptop.signing, T0 + 2).unwrap();
        let id = "01a0ec7b-4444-7000-8000-0000000000d8";
        let o = options(&url, &laptop, UserKeys::new(user, []).unwrap(), chain, id);
        let refused = take(&o).err().expect("generation 1 seals nothing when the list names 2");
        assert!(refused.contains("names generation 2"), "{refused}");
        assert_eq!(o.api.show_sync_session(id).unwrap_err().status, 404, "nothing reached the registry");
    }
}
