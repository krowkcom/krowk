//! The daemon's side of the socket: a listener around `Host`, so a turn is
//! the host's whatever becomes of the client that asked for it.
//!
//! Every session the daemon has seen is a hub: the host it runs on, the
//! clients following it, and the frames of the turn it is running. A
//! frame goes to every follower in the order the host sent it, numbered
//! (`line.seq`) and encoded once, each through its own bounded queue
//! (`outbox`), so two clients on one session see the same frames — none
//! drops what a slower one has not read. A client that falls behind is not
//! given a growing buffer: its session is caught up from its cursor once it
//! reads again, as an `attach` from there would (R-LAG-4). A client that
//! attaches late is sent the log up to its last event sent live, then the
//! running turn's frames after it (`replay`), then everything live: the
//! turn so far, including the typing of the item under way, which the log
//! alone does not hold (R-LAG-1).
//!
//! The same machinery serves two transports: the unix socket here, one
//! JSON object a line, and the WebSocket listener (`ws`), the same frames
//! in batches.
//!
//! One `Host` serves each working directory clients connect from, and
//! whether the client answers approval requests (`hello.answersApprovals`):
//! a new session starts where its client is, a resumed one keeps its own
//! directory, as it does in-process, and a `krowk -p` client's turns refuse
//! what would be asked exactly as in-process `-p` does. A request of an
//! answering client's turn that no answering client is left to see — the
//! last one went away — is denied, so no turn waits on nobody. Everything runs on one thread, as the
//! in-process host does: nothing here is shared across threads.
//!
//! It exits after the idle window with no client connected and no turn
//! running (R-HOST-1), so nothing stays resident when unused; run as a
//! service it has no window and stays.

use crate::engine::EngineError;
use crate::host::{Host, HostConfig};
use crate::log;
use super::outbox::{self, Caps, Out, Outbox, Pushed};
use super::replay::{self, Tail};
use crate::protocol::{ClientFrame, Command, ErrorInfo, HostSession, HostStatus, LogBody, ServerFrame, StreamLine, PROTOCOL_VERSION};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Notify};

pub struct Options {
    pub socket: PathBuf,
    /// None: never exit by itself.
    pub idle: Option<Duration>,
    pub krowk_version: String,
    /// Where the WebSocket listener binds: a loopback address, or none for
    /// no listener (`ws`).
    pub websocket: Option<SocketAddr>,
    /// How often a WebSocket connection is pinged.
    pub heartbeat: Duration,
    /// How much one client may have waiting.
    pub caps: Caps,
    /// How much of a running turn each session keeps for catching up.
    pub replay_cap: usize,
    /// For tests: the most, in microseconds, the daemon's thread has been
    /// late for a 5 ms tick since this was last set to 0 — how long
    /// something blocked every session and heartbeat at once (R-LAG-9).
    pub lateness: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// For tests: each time it is notified, every client is let go, as
    /// one is that falls past what a catch-up can read (`extent`), so a
    /// client's re-following from its cursor can be seen.
    pub kick: Option<std::sync::Arc<Notify>>,
}

impl Default for Options {
    fn default() -> Options {
        Options { socket: PathBuf::new(), idle: Some(super::DEFAULT_IDLE), krowk_version: String::new(), websocket: None, heartbeat: super::ws::HEARTBEAT, caps: Caps::default(), replay_cap: replay::CAP, lateness: None, kick: None }
    }
}

/// The most one write takes from a client's queue: big enough that a busy
/// session goes out in few writes, small enough that the others' frames
/// are never far behind it.
const BATCH_BYTES: usize = 256 * 1024;

/// Makes the host configuration for clients in one working directory,
/// whose turns ask a person (`true`) or refuse what would be asked: the
/// caller's, which owns the config, the keys and the price cache. It sets
/// `permissions.approvals` from the flag. It reads the config and the
/// environment, so it runs on the blocking pool, not the daemon's thread
/// (R-LAG-9): hence `Send + Sync`.
pub type Factory = Box<MakeHost>;

type MakeHost = dyn Fn(&Path, bool) -> Result<HostConfig, EngineError> + Send + Sync;

/// Serves until idle or told to stop (SIGTERM, SIGINT), on a runtime of its
/// own.
pub fn run(opts: Options, factory: Factory) -> Result<(), String> {
    if let Some(addr) = opts.websocket
        && !addr.ip().is_loopback()
    {
        return Err(format!("the WebSocket listener binds only to loopback, and {addr} is not — use 127.0.0.1:<port> (the relay reaches other devices)"));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))?;
    // Every session shares this thread: the logs' blocking work goes to the
    // blocking pool (R-LAG-9).
    log::off_thread();
    let local = tokio::task::LocalSet::new();
    let r = local.block_on(&rt, serve(opts, factory));
    // What `serve` must see done — the turns' syncs — it has waited for;
    // a blocking task still running (a directory's configuration on a
    // dead disk) is not waited for, or a second signal would not exit at
    // once.
    drop(local);
    rt.shutdown_background();
    r
}

pub(super) struct State {
    factory: std::sync::Arc<MakeHost>,
    /// The WebSocket listener's address and token, when it listens.
    pub(super) websocket: Option<(SocketAddr, String)>,
    hosts: HashMap<(PathBuf, bool), Rc<Host>>,
    sessions_dir: Option<PathBuf>,
    hubs: HashMap<String, Hub>,
    clients: HashMap<u64, Client>,
    next_client: u64,
    next_hub: u64,
    /// Turns running (`prompt` and `continue` commands under way).
    working: usize,
    started: Instant,
    pub(super) opts: Options,
    /// Rung whenever what idleness depends on changes.
    wake: Rc<Notify>,
    /// A client asked it to exit (`stop`), or a signal did.
    stopping: bool,
    /// Moved by every `reload`: a new directory's configuration read
    /// across one is read again, so no host is made with the instances
    /// the reload replaced.
    generation: u64,
    /// Times a client fell behind, to be caught up from its cursor.
    caught_up: u64,
    /// The last `line.seq` given out in each session: kept when a session
    /// is let go, so a `seq` is never reused while the daemon runs.
    seqs: HashMap<String, u64>,
    /// When this daemon started, in ms since the Unix epoch: the run a
    /// `seq` is numbered within (`welcome.epoch`).
    pub(super) epoch: u64,
    /// WebSocket connections that have not said an authenticated hello yet.
    pub(super) unauthenticated: usize,
}

struct Hub {
    /// The host its last command ran on: where an `interrupt`, `steer` or
    /// `approve` for it goes.
    host: Rc<Host>,
    followers: Vec<u64>,
    /// The running turn's frames after its last logged event, bounded;
    /// empty between turns.
    turn: Tail,
    running: bool,
    /// Commands under way in it that stream (`prompt`, `continue`).
    in_flight: usize,
    /// The id of the last of its own log events sent to its followers:
    /// where catching up from the log stops, since what the log has beyond
    /// it is on its way live.
    head: Option<String>,
    /// Its subagents' sessions seen on its stream: their approval requests
    /// are its followers' to answer too.
    children: HashSet<String>,
    /// How many events its log held when the streaming command under way
    /// registered: all of them were written before it, so none comes live,
    /// and catching up replays them whatever else it cannot tell yet.
    base: usize,
    /// When it was first seen, for the status listing's order.
    seen: u64,
}

struct Client {
    outbox: Rc<Outbox>,
    cwd: PathBuf,
    answers: bool,
}

pub(super) type Shared = Rc<RefCell<State>>;

/// Binds the socket, refusing when a daemon already answers on it. A socket
/// nothing answers on was left by one that died, and is replaced.
// blocking: at start, before the runtime serves anyone.
fn bind(path: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::PermissionsExt;
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(format!("a host daemon already listens on {}", path.display()));
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{} cannot be replaced: {e}", path.display())),
        _ => {}
    }
    let l = UnixListener::bind(path).map_err(|e| format!("{} cannot be listened on: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("{} cannot be made private: {e}", path.display()))?;
    Ok(l)
}

pub async fn serve(opts: Options, factory: Factory) -> Result<(), String> {
    let listener = bind(&opts.socket)?;
    let ours = inode(&opts.socket);
    let wake = Rc::new(Notify::new());
    let idle = opts.idle;
    let socket = opts.socket.clone();
    let websocket = match opts.websocket {
        Some(addr) => Some(super::ws::bind(addr, opts.socket.parent().unwrap_or(Path::new("/"))).await?),
        None => None,
    };
    let state: Shared = Rc::new(RefCell::new(State {
        factory: std::sync::Arc::from(factory),
        websocket: websocket.as_ref().map(|(l, t)| (l.local_addr().expect("a bound listener has an address"), t.clone())),
        hosts: HashMap::new(),
        sessions_dir: None,
        hubs: HashMap::new(),
        clients: HashMap::new(),
        next_client: 0,
        next_hub: 0,
        stopping: false,
        generation: 0,
        caught_up: 0,
        seqs: HashMap::new(),
        epoch: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_millis() as u64),
        unauthenticated: 0,
        working: 0,
        started: Instant::now(),
        opts,
        wake: wake.clone(),
    }));
    eprintln!("krowk host {} listening on {} (pid {}, idle exit {})", state.borrow().opts.krowk_version, socket.display(), std::process::id(), idle.map_or("never".into(), |d| format!("{d:?}")));
    // The TLS configuration every engine shares, built off the thread
    // before any turn needs it, and never on it: an engine made here takes
    // what `warm` built, or fails with why there is none.
    crate::http::warm_only();
    tokio::task::spawn_blocking(crate::http::warm);
    if let Some(kick) = state.borrow().opts.kick.clone() {
        let state = state.clone();
        tokio::task::spawn_local(async move {
            loop {
                kick.notified().await;
                let mut s = state.borrow_mut();
                let all: Vec<u64> = s.clients.keys().copied().collect();
                for c in all {
                    s.drop_client(c);
                }
            }
        });
    }
    if let Some(probe) = state.borrow().opts.lateness.clone() {
        tokio::task::spawn_local(async move {
            const TICK: Duration = Duration::from_millis(5);
            loop {
                let at = Instant::now();
                tokio::time::sleep(TICK).await;
                let late = at.elapsed().saturating_sub(TICK).as_micros() as u64;
                probe.fetch_max(late, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    if let Some((listener, _)) = websocket {
        eprintln!("krowk host WebSocket listening on ws://{}", listener.local_addr().map_err(|e| e.to_string())?);
        tokio::task::spawn_local(super::ws::accept(listener, state.clone()));
    }
    {
        let state = state.clone();
        tokio::task::spawn_local(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::task::spawn_local(connection(stream, state.clone()));
                    }
                    Err(e) => {
                        eprintln!("accept: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
    }
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut signalled = false;
    loop {
        let (quiet, stopping) = {
            let s = state.borrow();
            (s.clients.is_empty() && s.working == 0, s.stopping)
        };
        if stopping {
            eprintln!("asked to stop: exiting");
            break;
        }
        // The window starts again at every change: it is time with nothing
        // to do, not time since the start.
        let window = async {
            match idle {
                Some(d) if quiet => tokio::time::sleep(d).await,
                _ => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = wake.notified() => {}
            _ = window => {
                eprintln!("idle for {:?} with no session running: exiting", idle.unwrap_or_default());
                break;
            }
            _ = term.recv() => {
                eprintln!("terminated: interrupting the running turns, then exiting");
                signalled = true;
                break;
            }
            _ = int.recv() => {
                eprintln!("interrupted: interrupting the running turns, then exiting");
                signalled = true;
                break;
            }
        }
    }
    // Every way out takes no new turn from here, an idle exit's included:
    // a prompt that arrives while the engines are let go is refused.
    state.borrow_mut().stopping = true;
    // Only the socket this daemon bound: one a newer daemon put in its
    // place is that one's.
    // blocking: on the way out, with nobody left to serve.
    if inode(&socket).is_some() && inode(&socket) == ours {
        let _ = std::fs::remove_file(&socket);
    }
    // The way out waits on turns, engines and syncs — up to
    // `SHUTDOWN_GRACE` for each — and a second signal cuts it short: a
    // person who presses Ctrl-C again means now.
    let out = async {
        if signalled {
            wind_down(&state).await;
        }
        let hosts: Vec<Rc<Host>> = state.borrow().hosts.values().cloned().collect();
        for h in hosts {
            h.shutdown().await;
        }
        // The turns' syncs, still on the blocking pool: a runtime dropped
        // with them queued drops them unrun, and a `krowk host stop`
        // straight after a turn would leave that turn in the page cache
        // alone. Bounded like the engines' shutdown: a sync stuck on a dead
        // mount must not keep the daemon from exiting.
        if tokio::time::timeout(crate::host::SHUTDOWN_GRACE, log::synced()).await.is_err() {
            eprintln!("{} session log sync(s) had not finished after {:?}: exiting without them", log::pending_syncs(), crate::host::SHUTDOWN_GRACE);
        }
    };
    tokio::select! {
        () = out => {}
        _ = term.recv() => eprintln!("terminated again: exiting now"),
        _ = int.recv() => eprintln!("interrupted again: exiting now"),
    }
    Ok(())
}

fn stopping_error() -> EngineError {
    EngineError::new("host_stopping", "the host daemon is exiting — send it again, and the next daemon runs it")
}

/// On SIGTERM or SIGINT: no new turn is taken, the running ones are
/// interrupted the way a person interrupts them, and each is given until
/// `SHUTDOWN_GRACE` to end — its `turn.completed` logged and its sync
/// queued for `synced` — rather than being dropped with the runtime.
///
/// The sessions under way are looked at again on every change and every
/// few milliseconds, not once: a turn still setting up when the signal came
/// — its engine being made, its first request not sent — has no turn to
/// interrupt yet, and is interrupted once it has, rather than running on
/// until the grace is up.
async fn wind_down(state: &Shared) {
    let wake = state.borrow().wake.clone();
    let mut interrupted: HashSet<String> = HashSet::new();
    let _ = tokio::time::timeout(crate::host::SHUTDOWN_GRACE, async {
        loop {
            let running: Vec<(String, Rc<Host>)> = {
                let s = state.borrow();
                if s.working == 0 {
                    return;
                }
                // Interrupted once a turn: a session whose turn has ended
                // is looked at again, should another start in it.
                interrupted.retain(|id| s.hubs.get(id).is_some_and(|h| h.running));
                s.hubs.iter().filter(|(id, h)| h.running && !interrupted.contains(*id)).map(|(id, h)| (id.clone(), h.host.clone())).collect()
            };
            for (session_id, host) in running {
                interrupted.insert(session_id.clone());
                tokio::task::spawn_local(async move {
                    let (tx, _rx) = mpsc::channel::<StreamLine>(16);
                    let _ = host.execute(Command::Interrupt { session_id }, tx).await;
                });
            }
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    })
    .await;
}

// blocking: at start and on the way out, like `bind`.
fn inode(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().map(|m| m.ino())
}

fn error_info(e: &EngineError) -> ErrorInfo {
    ErrorInfo { code: e.code.clone(), message: e.message.clone(), http_status: (e.status != 0).then_some(e.status), resets_at_ms: e.resets_at_ms }
}

/// Answers a client's first frame: its id and outbox once it may be
/// served, else the frame that says why not, sent before the connection
/// closes. `token` is what a WebSocket client must prove it holds; the unix
/// socket's permissions already say whose it is.
pub(super) fn welcome(state: &Shared, hello: Option<ClientFrame>, token: Option<&str>) -> Result<(u64, Rc<Outbox>), Box<ServerFrame>> {
    let refuse = |code: &str, message: String, fix: String| Box::new(ServerFrame::Refused { code: code.into(), message, fix });
    // The token before anything else: a client that cannot prove it is this
    // user learns nothing, not even the daemon's pid or version.
    if let Some(want) = token {
        let given = match &hello {
            Some(ClientFrame::Hello { token: Some(g), .. }) => g.as_str(),
            _ => "",
        };
        if !same(given.as_bytes(), want.as_bytes()) {
            return Err(refuse("unauthorized", "the hello carries no token, or not this daemon's".into(), "send the token in host.token, beside the daemon's socket, as `hello.token`".into()));
        }
    }
    let (cwd, answers) = match hello {
        Some(ClientFrame::Hello { protocol_version, cwd, answers_approvals, .. }) if protocol_version == PROTOCOL_VERSION => (PathBuf::from(cwd), answers_approvals),
        Some(ClientFrame::Hello { protocol_version, .. }) => {
            let pid = std::process::id();
            return Err(refuse(
                "protocol_mismatch",
                format!("this krowk speaks protocol {protocol_version}, and the host daemon (pid {pid}, krowk {}) speaks {PROTOCOL_VERSION}", state.borrow().opts.krowk_version),
                format!("stop the daemon with `kill {pid}` once its sessions are done — the next krowk starts one of its own version"),
            ));
        }
        _ => return Err(refuse("bad_hello", "the first frame was not a hello".into(), "send {\"type\":\"hello\",\"protocolVersion\":1,\"cwd\":\"…\"} first".into())),
    };
    let mut s = state.borrow_mut();
    s.next_client += 1;
    let id = s.next_client;
    let outbox = Rc::new(Outbox::new(s.opts.caps));
    outbox.push("", control(&ServerFrame::Welcome { protocol_version: PROTOCOL_VERSION, krowk_version: s.opts.krowk_version.clone(), pid: std::process::id(), epoch: s.epoch }));
    s.clients.insert(id, Client { outbox: outbox.clone(), cwd, answers });
    s.wake.notify_one();
    Ok((id, outbox))
}

/// Whether two secrets are equal, in time that does not depend on where
/// they first differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn connection(stream: UnixStream, state: Shared) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let hello = match lines.next_line().await {
        Ok(Some(l)) => serde_json::from_str::<ClientFrame>(&l).ok(),
        _ => return,
    };
    let (id, outbox) = match welcome(&state, hello, None) {
        Ok(c) => c,
        Err(refused) => {
            let _ = w.write_all(&encode(&refused)).await;
            return;
        }
    };
    // Whatever is queued goes out in one write: the socket's own buffer is
    // the window, and a client that stops reading fills it, then its
    // outbox, then falls behind to its cursor (R-LAG-4).
    let writer = tokio::task::spawn_local({
        let state = state.clone();
        async move {
            let mut buf = Vec::with_capacity(BATCH_BYTES);
            while outbox.ready().await {
                for (session, cursor) in outbox.due() {
                    resync(&state, id, session, cursor);
                }
                buf.clear();
                for (_, outs) in outbox.take(BATCH_BYTES) {
                    for o in outs {
                        buf.extend_from_slice(&o.bytes);
                    }
                }
                if !buf.is_empty() && w.write_all(&buf).await.is_err() {
                    break;
                }
            }
        }
    });
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<ClientFrame>(&line) {
            Ok(f) => dispatch(&state, id, f),
            Err(e) => eprintln!("client {id}: a line that is no frame ({e}): {}", line.chars().take(200).collect::<String>()),
        }
    }
    gone(&state, id);
    let _ = writer.await;
}

/// One frame from client `id`, after its hello.
pub(super) fn dispatch(state: &Shared, id: u64, frame: ClientFrame) {
    match frame {
        ClientFrame::Execute { id: cmd_id, command } => {
            // Stopping: a turn begun now would end after the syncs were
            // waited for, or not at all.
            if state.borrow().stopping {
                let e = stopping_error();
                return state.borrow_mut().send(id, "", control(&ServerFrame::Done { id: cmd_id, result: None, error: Some(error_info(&e)) }));
            }
            tokio::task::spawn_local(execute(state.clone(), id, cmd_id, command));
        }
        ClientFrame::Attach { id: cmd_id, session_id, after_event_id, after_seq, epoch } => {
            // A `seq` counts only within the daemon run that numbered it.
            let after_seq = if epoch == Some(state.borrow().epoch) { after_seq.unwrap_or(0) } else { 0 };
            attach(state, id, cmd_id, session_id, after_event_id, after_seq);
        }
        ClientFrame::Status { id: cmd_id } => {
            let mut s = state.borrow_mut();
            let f = ServerFrame::Status { id: cmd_id, status: status(&s) };
            s.send(id, "", control(&f));
        }
        ClientFrame::Reload { id: cmd_id, changed, renamed_from, renamed_to } => {
            let error = reload(state, changed.as_deref(), renamed_from.zip(renamed_to)).err().map(|e| error_info(&e));
            state.borrow_mut().send(id, "", control(&ServerFrame::Done { id: cmd_id, result: None, error }));
        }
        ClientFrame::Leave { session_id } => {
            let mut s = state.borrow_mut();
            if let Some(h) = s.hubs.get_mut(&session_id) {
                h.followers.retain(|f| *f != id);
            }
            if let Some(c) = s.clients.get(&id) {
                c.outbox.forget(&session_id);
            }
            s.unanswerable();
            s.tidy();
        }
        ClientFrame::Stop { id: cmd_id, force } => {
            let mut s = state.borrow_mut();
            let others = s.clients.len().saturating_sub(1);
            let error = if s.working > 0 {
                Some(ErrorInfo {
                    code: "host_busy".into(),
                    message: format!("the host daemon is running {} turn(s) — let them finish (`krowk host status` lists them), then try again", s.working),
                    http_status: None,
                    resets_at_ms: None,
                })
            } else if others > 0 && !force {
                Some(ErrorInfo {
                    code: "host_in_use".into(),
                    message: format!("{others} other client(s) are connected to the host daemon (an open krowk, say) — close them, or `krowk host stop --force`: each reconnects to the next daemon"),
                    http_status: None,
                    resets_at_ms: None,
                })
            } else {
                None
            };
            if error.is_none() {
                s.stopping = true;
                s.wake.notify_one();
            }
            s.send(id, "", control(&ServerFrame::Done { id: cmd_id, result: None, error }));
        }
        ClientFrame::Hello { .. } => {}
    }
}

/// Client `id` is gone; what it started runs on — but a request only it
/// could have answered is answered now, so the turn does not wait on
/// nobody.
pub(super) fn gone(state: &Shared, id: u64) {
    state.borrow_mut().drop_client(id);
}

/// A frame as it goes out: one JSON object and its newline.
pub(super) fn encode(f: &ServerFrame) -> Rc<[u8]> {
    let mut v = serde_json::to_vec(f).expect("a frame serializes");
    v.push(b'\n');
    Rc::from(v)
}

/// A control frame, queued in the session it answers for.
fn control(f: &ServerFrame) -> Out {
    Out { bytes: encode(f), seq: 0, log: None, line: false, slot: None, mark: outbox::Cursor::default() }
}

/// A `line` frame around `json`, the line already encoded: built by hand so
/// a frame published to many followers is encoded once, with only the
/// runner's `cmd` told apart. Its field order is `ServerFrame::Line`'s.
fn line_out(json: &str, line: &StreamLine, session: &str, cmd: Option<u64>, seq: u64, own_log: Option<&Rc<str>>) -> Out {
    let mut v = Vec::with_capacity(json.len() + session.len() + 64);
    v.extend_from_slice(b"{\"type\":\"line\",\"line\":");
    v.extend_from_slice(json.as_bytes());
    if !session.is_empty() {
        v.extend_from_slice(b",\"session\":");
        v.extend_from_slice(&serde_json::to_vec(session).expect("a string serializes"));
    }
    if let Some(c) = cmd {
        v.extend_from_slice(format!(",\"cmd\":{c}").as_bytes());
    }
    if seq > 0 {
        v.extend_from_slice(format!(",\"seq\":{seq}").as_bytes());
    }
    v.extend_from_slice(b"}\n");
    Out { bytes: Rc::from(v), seq, log: own_log.cloned(), line: true, slot: outbox::slot_of(line), mark: outbox::Cursor::default() }
}

fn status(s: &State) -> HostStatus {
    let mut hubs: Vec<(&String, &Hub)> = s.hubs.iter().collect();
    hubs.sort_by_key(|(_, h)| std::cmp::Reverse(h.seen));
    HostStatus {
        pid: std::process::id(),
        krowk_version: s.opts.krowk_version.clone(),
        protocol_version: PROTOCOL_VERSION,
        socket: s.opts.socket.display().to_string(),
        uptime_ms: s.started.elapsed().as_millis() as u64,
        clients: s.clients.len() as u32,
        idle_exit_ms: s.opts.idle.map(|d| d.as_millis() as u64),
        websocket: s.websocket.as_ref().map(|(a, _)| a.to_string()),
        queued_bytes: s.clients.values().map(|c| c.outbox.bytes() as u64).sum(),
        caught_up: s.caught_up,
        background: Some(s.hosts.values().map(|h| h.background()).sum()),
        sessions: hubs.into_iter().map(|(id, h)| HostSession { session_id: id.clone(), running: h.running, clients: h.followers.len() as u32 }).collect(),
    }
}

/// The host for clients in `cwd` that do or do not answer approvals, made
/// on first need. Its between-turns frames (`Host::watch`) go to each
/// session's followers.
///
/// A new directory's configuration is read on the blocking pool: the
/// config file, the instances and the keys' paths are file reads, and on a
/// slow disk they would stall every other session's stream (R-LAG-9). Two
/// clients asking for the same new directory at once each read it; the
/// first back makes the host and the second takes that one.
async fn host_for(state: &Shared, cwd: &Path, answers: bool) -> Result<Rc<Host>, EngineError> {
    let key = (cwd.to_path_buf(), answers);
    if let Some(h) = state.borrow().hosts.get(&key) {
        return Ok(h.clone());
    }
    let cfg = loop {
        let (factory, generation) = {
            let s = state.borrow();
            (s.factory.clone(), s.generation)
        };
        let dir = cwd.to_path_buf();
        let cfg = tokio::task::spawn_blocking(move || factory(&dir, answers)).await.map_err(|e| EngineError::new("host_failed", format!("the host's configuration could not be read: {e}")))??;
        if let Some(h) = state.borrow().hosts.get(&key) {
            return Ok(h.clone());
        }
        // A reload ran while it was read: what was read may be what the
        // reload replaced.
        if state.borrow().generation == generation {
            break cfg;
        }
    };
    let dir = cfg.sessions_dir.clone();
    let host = Rc::new(Host::new(cfg));
    let mut s = state.borrow_mut();
    s.sessions_dir.get_or_insert(dir);
    s.hosts.insert(key, host.clone());
    let mut watch = host.watch();
    let weak = Rc::downgrade(state);
    tokio::task::spawn_local(async move {
        loop {
            let line = match watch.recv().await {
                Ok(l) => l,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let Some(state) = weak.upgrade() else { break };
            let mut s = state.borrow_mut();
            let session = line.session_id().to_string();
            let Some(followers) = s.hubs.get(&session).map(|h| h.followers.clone()) else { continue };
            let seq = s.next_seq(&session);
            let json = serde_json::to_string(&line).expect("a stream line serializes");
            for f in followers {
                s.send(f, &session, line_out(&json, &line, &session, None, seq, None));
            }
        }
    });
    Ok(host)
}

/// `reload`: every host's instances read again, the way each was made.
fn reload(state: &Shared, changed: Option<&str>, renamed: Option<(String, String)>) -> Result<(), EngineError> {
    state.borrow_mut().generation += 1;
    let hosts: Vec<((PathBuf, bool), Rc<Host>)> = state.borrow().hosts.iter().map(|(k, h)| (k.clone(), h.clone())).collect();
    let Some(((cwd, answers), _)) = hosts.first() else { return Ok(()) };
    // The instances are the config's and the environment's, not a
    // directory's: read once, and given to every host, or to none when the
    // read fails.
    let registry = (state.borrow().factory)(cwd, *answers)?.registry;
    for (_, host) in hosts {
        match &renamed {
            Some((from, to)) => host.set_registry_renamed(registry.clone(), from, to),
            None => host.set_registry(registry.clone(), changed),
        }
    }
    Ok(())
}

/// The session a command names.
fn named(cmd: &Command) -> Option<&str> {
    match cmd {
        Command::Prompt { session_id, .. } | Command::SwitchModel { session_id, .. } => session_id.as_deref(),
        Command::Interrupt { session_id } | Command::Steer { session_id, .. } | Command::Approve { session_id, .. } | Command::Fork { session_id, .. } | Command::Continue { session_id, .. } => Some(session_id),
    }
}

impl State {
    /// The session's hub, made on first need, with `client` following it.
    fn follow(&mut self, session: &str, host: &Rc<Host>, client: u64) -> &mut Hub {
        let seen = self.next_hub;
        let cap = self.opts.replay_cap;
        let hub = self.hubs.entry(session.to_string()).or_insert_with(|| Hub {
            host: host.clone(),
            followers: Vec::new(),
            turn: Tail::new(cap),
            running: false,
            in_flight: 0,
            head: None,
            children: HashSet::new(),
            base: 0,
            seen,
        });
        if hub.seen == seen {
            self.next_hub += 1;
        }
        if self.clients.contains_key(&client) && !hub.followers.contains(&client) {
            hub.followers.push(client);
        }
        hub
    }

    /// Queues `out` for `client` in `session`'s queue. A client past its cap
    /// is caught up from its cursor once it reads again; one that does not
    /// read even its answers is let go.
    fn send(&mut self, client: u64, session: &str, mut out: Out) {
        // A control frame is marked with where its session stands, to be
        // put back at that place should the client be caught up past it.
        if !out.line {
            out.mark = outbox::Cursor { seq: self.seqs.get(session).copied().unwrap_or(0), log: self.hubs.get(session).and_then(|h| h.head.as_deref()).map(Rc::from) };
        }
        let Some(c) = self.clients.get(&client) else { return };
        match c.outbox.push(session, out) {
            Pushed::Queued | Pushed::Dropped => {}
            Pushed::FellBehind(cursor) => {
                // Caught up once the transport has handed on what it could
                // (`Outbox::due`, then `resync`).
                eprintln!("client {client} fell behind on session {session} at seq {}: its queue is dropped to its cursor", cursor.seq);
                self.caught_up += 1;
            }
            Pushed::Overflow => {
                eprintln!("client {client} reads nothing, not even its answers: letting it go");
                self.drop_client(client);
            }
        }
    }

    /// The session's next `line.seq`.
    fn next_seq(&mut self, session: &str) -> u64 {
        let n = self.seqs.entry(session.to_string()).or_insert(0);
        *n += 1;
        *n
    }

    /// Lets `client` go: its outbox closes once what it has is handed on,
    /// and it follows nothing.
    fn drop_client(&mut self, client: u64) {
        let Some(c) = self.clients.remove(&client) else { return };
        c.outbox.close();
        for h in self.hubs.values_mut() {
            h.followers.retain(|f| *f != client);
        }
        self.unanswerable();
        self.tidy();
        self.wake.notify_one();
    }

    /// Whether any client following `hub` answers approval requests.
    fn answered(&self, hub: &Hub) -> bool {
        hub.followers.iter().any(|f| self.clients.get(f).is_some_and(|c| c.answers))
    }

    /// Denies the waiting requests of every session no answering client
    /// follows any more.
    fn unanswerable(&self) {
        for (id, hub) in &self.hubs {
            if hub.running && !self.answered(hub) {
                let mut all: Vec<String> = hub.children.iter().cloned().collect();
                all.push(id.clone());
                hub.host.deny_waiting(&all);
            }
        }
    }

    /// Lets go of the sessions nothing follows or runs, and the hosts no
    /// session of this daemon is on — each with its backend processes.
    fn tidy(&mut self) {
        self.hubs.retain(|_, h| h.running || h.in_flight > 0 || !h.followers.is_empty());
        let idle: Vec<(PathBuf, bool)> = self.hosts.iter().filter(|(_, h)| Rc::strong_count(h) == 1).map(|(k, _)| k.clone()).collect();
        for k in idle {
            if let Some(h) = self.hosts.remove(&k) {
                tokio::task::spawn_local(async move { h.shutdown().await });
            }
        }
    }

    /// One frame of a command run for `client` in `root`'s session — a
    /// subagent's frames included, which are the parent's turn's. Numbered,
    /// encoded once, and queued for every follower: nothing between the
    /// host's stream and the socket writes anywhere (R-LAG-1).
    fn publish(&mut self, root: &str, host: &Rc<Host>, client: u64, cmd: u64, line: StreamLine) {
        let seq = self.next_seq(root);
        let hub = self.follow(root, host, client);
        let own = line.session_id() == root;
        if !own {
            hub.children.insert(line.session_id().to_string());
        }
        let json = serde_json::to_string(&line).expect("a stream line serializes");
        let mut own_log: Option<Rc<str>> = None;
        if let StreamLine::Log(ev) = &line
            && own
        {
            hub.head = Some(ev.id.clone());
            own_log = Some(Rc::from(ev.id.as_str()));
            match ev.body {
                // The host that runs it is the one whose turn started —
                // never one whose command was refused (a second `--resume`
                // from another directory), where an interrupt would find
                // nothing.
                LogBody::TurnStarted { .. } => {
                    hub.running = true;
                    hub.host = host.clone();
                }
                LogBody::TurnCompleted { .. } => hub.running = false,
                _ => {}
            }
            // The log holds everything up to here: what catching up
            // replays of the turn starts after it.
            hub.turn.clear();
        } else if hub.running {
            hub.turn.push(root, seq, line.clone(), json.len() + 64);
        }
        let asks = matches!(&line, StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(_)));
        let followers = hub.followers.clone();
        for f in followers {
            let out = line_out(&json, &line, root, (f == client).then_some(cmd), seq, own_log.as_ref());
            self.send(f, root, out);
        }
        // Asked with nobody left who could answer: denied at once.
        if asks && !self.hubs.get(root).is_some_and(|h| self.answered(h)) {
            host.deny_waiting(&[line.session_id().to_string()]);
        }
    }
}

/// Registers a command that logs to the session it names with that
/// session's hub, counting the log first so nothing it writes is in the
/// count (see `execute`).
async fn register(state: &Shared, root: Option<&str>, host: &Rc<Host>, client: u64) {
    let dir = state.borrow().sessions_dir.clone();
    let base = match (root, dir) {
        (Some(r), Some(dir)) if log::valid_id(r) => {
            let p = dir.join(r).join(log::EVENTS_FILE);
            tokio::task::spawn_blocking(move || log::read_events(&p).map(|e| e.len()).unwrap_or(0)).await.unwrap_or(0)
        }
        _ => 0,
    };
    let mut s = state.borrow_mut();
    s.wake.notify_one();
    if let Some(r) = root {
        let hub = s.follow(r, host, client);
        if hub.in_flight == 0 {
            hub.base = base;
        }
        hub.in_flight += 1;
    }
}

async fn execute(state: Shared, client: u64, id: u64, cmd: Command) {
    let Some((cwd, answers)) = state.borrow().clients.get(&client).map(|c| (c.cwd.clone(), c.answers)) else { return };
    let mut root = named(&cmd).map(String::from);
    let turn = matches!(cmd, Command::Prompt { .. } | Command::Continue { .. });
    // Commands that log to the session they name before they publish it —
    // a turn, and a model switch's `model.switched` — register with its hub
    // (`in_flight`, `base`): a catch-up read that lands between the write
    // and the publish then stops at what was sent live, and the event
    // comes once, live, rather than in the page and again after it.
    let switch = matches!(cmd, Command::SwitchModel { .. });
    let registers = turn || switch;
    // A turn runs on the host of the client that asks for it — its
    // directory, and whether it answers approvals; the rest go to the host
    // running the session.
    let known = if turn { None } else { root.as_ref().and_then(|r| state.borrow().hubs.get(r).map(|h| h.host.clone())) };
    // Working from here: a `stop` while a new directory's configuration is
    // read, or the log is counted below, must not end the daemon under a
    // prompt it accepted.
    if turn {
        state.borrow_mut().working += 1;
    }
    let host = match known {
        Some(h) => Ok(h),
        None => host_for(&state, &cwd, answers).await,
    };
    // A new directory's read can outlast a signal: a turn whose host came
    // back once the daemon was stopping is refused, not begun.
    let host = match host {
        Ok(_) if turn && state.borrow().stopping => Err(stopping_error()),
        other => other,
    };
    let host = match host {
        Ok(h) => h,
        Err(e) => {
            let mut s = state.borrow_mut();
            if turn {
                s.working -= 1;
                s.wake.notify_one();
            }
            return s.send(client, "", control(&ServerFrame::Done { id, result: None, error: Some(error_info(&e)) }));
        }
    };
    if registers {
        register(&state, root.as_deref(), &host, client).await;
    }
    // The engine a turn makes needs the TLS configuration, whose build
    // reads the platform's roots: tens of milliseconds, a hundred on a
    // slow macOS runner. A turn sent as the daemon starts, before `warm`
    // is done, waits for it off the thread instead (R-LAG-9). A build that
    // fails is remembered for a few seconds (`http::tls`), so the engine's
    // own call right after is refused at once rather than built again on
    // the thread; a turn that needs no TLS (a backend's) runs as it did.
    // A model switch too, which makes an engine to check the model: the
    // daemon's `client` never builds the configuration itself
    // (`http::warm_only`). Nothing else waits for it — an interrupt least
    // of all.
    if registers && !crate::http::warmed() {
        let _ = tokio::task::spawn_blocking(crate::http::warm).await;
    }
    let (tx, mut rx) = mpsc::channel::<StreamLine>(1024);
    let exec = host.execute(cmd, tx);
    tokio::pin!(exec);
    let mut counted = root.is_some();
    let mut publish = |line: StreamLine| {
        let r = root.get_or_insert_with(|| line.session_id().to_string()).clone();
        let mut s = state.borrow_mut();
        if turn && !counted {
            // A new session's hub, made by its first line.
            let hub = s.follow(&r, &host, client);
            hub.in_flight += 1;
            counted = true;
        }
        s.publish(&r, &host, client, id, line);
    };
    let r = loop {
        tokio::select! {
            biased;
            Some(line) = rx.recv() => publish(line),
            r = &mut exec => break r,
        }
    };
    while let Ok(line) = rx.try_recv() {
        publish(line);
    }
    let mut s = state.borrow_mut();
    if turn {
        s.working -= 1;
    }
    if registers {
        if let Some(r) = &root
            && let Some(h) = s.hubs.get_mut(r)
        {
            h.in_flight = h.in_flight.saturating_sub(1);
            if h.in_flight == 0 {
                let followers = h.followers.clone();
                for f in followers {
                    s.send(f, r, control(&ServerFrame::Settled { session: r.clone() }));
                }
            }
        }
        s.wake.notify_one();
    }
    let (result, error) = match r {
        Ok(r) => (r, None),
        Err(e) => (None, Some(error_info(&e))),
    };
    // In the session's queue, behind its lines: a turn's `done` comes after
    // them, however far behind its client is.
    s.send(client, root.as_deref().unwrap_or(""), control(&ServerFrame::Done { id, result, error }));
    s.tidy();
}

/// One page of catching `client` up on `session` from `cursor`: a client
/// that fell behind, or one attaching. Asked for by the transport once the
/// client has read the last page (`Outbox::due`), so however far behind it
/// is the daemon holds at most a page for it.
pub(super) fn resync(state: &Shared, client: u64, session: String, cursor: outbox::Cursor) {
    let state = state.clone();
    tokio::task::spawn_local(async move {
        let Err(e) = page(&state, client, &session, cursor).await else { return };
        let mut s = state.borrow_mut();
        let Some(c) = s.clients.get(&client) else { return };
        let held = c.outbox.held(&session);
        match c.outbox.resynced(&session, Vec::new(), held, true) {
            Some(id) => s.send(client, &session, control(&ServerFrame::Attached { id, session_id: session.clone(), running: false, error: Some(error_info(&e)) })),
            // Nothing to catch it up from: it cannot be given a stream
            // without a gap, so it is let go, and reconnects from its
            // cursor itself.
            None => s.drop_client(client),
        }
    });
}

/// Catches `client` up on `session` from its cursor, in pages, then follows
/// it from there; answered by `attached` after the last page.
fn attach(state: &Shared, client: u64, id: u64, session: String, after: Option<String>, after_seq: u64) {
    let mut s = state.borrow_mut();
    if !log::valid_id(&session) {
        let e = EngineError::new("bad_session", format!("{session:?} is not a session id — the sessionId a result names"));
        return s.send(client, &session, control(&ServerFrame::Attached { id, session_id: session.clone(), running: false, error: Some(error_info(&e)) }));
    }
    if let Some(c) = s.clients.get(&client) {
        c.outbox.begin(&session, outbox::Cursor { seq: after_seq, log: after.as_deref().map(Rc::from) }, id);
    }
}

/// One page of `session`'s catching up from `cursor` — the log after its
/// event (all of it when none), then the running turn's frames after its
/// `seq` — handed to the client's outbox with the state borrowed: nothing
/// is published between the last page and what follows it live.
async fn page(state: &Shared, client: u64, session: &str, cursor: outbox::Cursor) -> Result<(), EngineError> {
    let Some((cwd, answers)) = state.borrow().clients.get(&client).map(|c| (c.cwd.clone(), c.answers)) else { return Ok(()) };
    let known = state.borrow().hubs.get(session).map(|h| h.host.clone());
    let host = match known {
        Some(h) => h,
        None => host_for(state, &cwd, answers).await?,
    };
    let Some(dir) = state.borrow().sessions_dir.clone() else { return Err(EngineError::new("no_session", "the daemon has no sessions directory")) };
    let path = dir.join(session).join(log::EVENTS_FILE);
    // Read off the daemon's thread, which every other stream shares; then
    // matched against the hub as it is once the read is back. While a turn
    // is under way the log is replayed only up to the last event already
    // sent live (the hub's head): what lies beyond it is on its way, and
    // comes in order with the frames around it. A head the read does not
    // reach yet — the log grew while it read — is read again.
    let mut tries = 0;
    loop {
        tries += 1;
        let p = path.clone();
        let events = match tokio::task::spawn_blocking(move || log::read_events(&p)).await {
            Ok(Ok(e)) => e,
            // A log there that does not read whole: its last line caught
            // half written, as a turn appends a large event. Not yet
            // written, rather than no session: read again.
            Ok(Err(_)) if path.is_file() && tries < MAX_READS => {
                tokio::time::sleep(READ_GAP).await;
                continue;
            }
            _ => return Err(EngineError::new("no_session", format!("there is no session {session} — `krowk sessions` lists them"))),
        };
        if decide(state, client, session, &host, events, tries, &cursor) {
            return Ok(());
        }
        tokio::time::sleep(READ_GAP).await;
    }
}

/// How much of a read of `session`'s log a page may replay, matched
/// against its hub as it is now: `under_way` whether a turn runs or a
/// streaming command is registered, `head` the last of its own events sent
/// live, `base` what the log held when the command registered. `Ok(None)`:
/// the read is behind what was sent live, and is to be read again.
///
/// The read ran on the blocking pool while the daemon's thread went on, so
/// the session may have moved past it — a turn may even have ended — and
/// every event it published meanwhile was dropped for this client, which
/// was behind. A read that does not reach the head misses them, under way
/// or not: taken as the whole log once the turn had ended, it lost the
/// turn's last logged events for good (R-LAG-4). With the head in the read
/// it has all that was sent live: up to the head while a turn runs, since
/// what lies beyond it comes live in order; all of it once none runs,
/// since nothing more comes.
///
/// `Ok(Some((upto, live)))`: replay the read up to `upto`, then — when
/// `live` — the running turn's frames after the head. `Err`: a read that
/// has not reached the head in `MAX_READS` — a page from it could have a
/// gap, so the client is let go instead, to re-follow from its cursor
/// (fail closed; `daemon::remote`).
fn extent(under_way: bool, head: Option<&str>, base: usize, events: &[crate::protocol::LogEvent], tries: u32) -> Result<Option<(usize, bool)>, ()> {
    let n = events.len();
    let at = head.and_then(|h| events.iter().position(|e| e.id == h));
    Ok(match (head, at) {
        // Nothing sent live: the log as it was before the command, all of
        // which is history, or all of it when none is under way.
        (None, _) if under_way => Some((base.min(n), false)),
        (None, _) => Some((n, false)),
        (Some(_), Some(at)) if under_way => Some((at + 1, true)),
        (Some(_), Some(_)) => Some((n, false)),
        // The read is behind what was sent: read again, and past that,
        // give up on this client rather than hand it a gap.
        (Some(_), None) if tries < MAX_READS => None,
        (Some(_), None) => return Err(()),
    })
}

/// Reads of the log, `READ_GAP` apart, a page waits for to reach the head.
const MAX_READS: u32 = 100;
const READ_GAP: Duration = Duration::from_millis(10);

/// Matches a read of the log against the hub as it is now, and hands the
/// client's outbox the next page, with the control frames it held put back
/// at their places — false when the read is behind what was sent live, and
/// is to be read again.
fn decide(state: &Shared, client: u64, session: &str, host: &Rc<Host>, events: Vec<crate::protocol::LogEvent>, tries: u32, cursor: &outbox::Cursor) -> bool {
    let mut s = state.borrow_mut();
    let hub = s.hubs.get(session);
    // Under way: `running` from its `turn.started`, or a command registered
    // whose first frame is on its way.
    let under_way = hub.is_some_and(|h| h.running || h.in_flight > 0);
    let Some((upto, live)) = (match extent(under_way, hub.and_then(|h| h.head.as_deref()), hub.map_or(0, |h| h.base), &events, tries) {
        Ok(page) => page,
        Err(()) => {
            eprintln!("client {client}: the log of session {session} never reached what was sent live: letting it go, to reconnect from its cursor");
            s.drop_client(client);
            return true;
        }
    }) else {
        return false;
    };
    let running = under_way;
    let tail = match hub {
        Some(h) if live => h.turn.after(cursor.seq),
        _ => Vec::new(),
    };
    // Gone, or it left the session while the log was read.
    let Some(outbox) = s.clients.get(&client).map(|c| c.outbox.clone()) else { return true };
    if !outbox.expects(session) {
        return true;
    }
    let from = cursor.log.as_deref().and_then(|a| events.iter().position(|e| e.id == a)).map_or(0, |i| i + 1);
    let cap = s.opts.caps.session_bytes;
    // The page: the log from the cursor up to the cap, and once the log is
    // all in, the running turn's frames. `at` is each frame's place — its
    // index in the log, or its `seq` — for putting the held frames back.
    let (mut frames, mut at): (Vec<Out>, Vec<Result<usize, u64>>) = (Vec::new(), Vec::new());
    let mut bytes = 0;
    let mut next = from;
    while next < upto && (bytes < cap || frames.is_empty()) {
        let ev = events[next].clone();
        let id: Rc<str> = Rc::from(ev.id.as_str());
        let line = StreamLine::Log(ev);
        let json = serde_json::to_string(&line).expect("a stream line serializes");
        let out = line_out(&json, &line, session, None, 0, Some(&id));
        bytes += out.bytes.len();
        frames.push(out);
        at.push(Ok(next));
        next += 1;
    }
    let last = next >= upto;
    if last {
        for (seq, line) in tail {
            let json = serde_json::to_string(&line).expect("a stream line serializes");
            frames.push(line_out(&json, &line, session, None, seq, None));
            at.push(Err(seq));
        }
    }
    // Each held frame goes after the frames that came before it: the log's
    // events up to its mark's, then the turn's frames up to its mark's
    // `seq`. One whose place is past this page waits for the next.
    let mut rest = Vec::new();
    let mut placed: Vec<(usize, Out)> = Vec::new();
    for h in outbox.held(session) {
        let li = h.mark.log.as_deref().and_then(|m| events.iter().position(|e| e.id == m));
        let before = |a: &Result<usize, u64>| match a {
            Ok(i) => li.is_some_and(|li| *i <= li),
            Err(q) => li.is_none_or(|li| li + 1 >= upto) && *q <= h.mark.seq,
        };
        let k = at.iter().take_while(|a| before(a)).count();
        if !last && li.is_some_and(|li| li >= next) {
            rest.push(h);
        } else {
            placed.push((k, h));
        }
    }
    let mut page = Vec::with_capacity(frames.len() + placed.len());
    let mut placed = placed.into_iter().peekable();
    for (i, f) in frames.into_iter().enumerate() {
        while let Some((_, h)) = placed.next_if(|(k, _)| *k == i) {
            page.push(h);
        }
        page.push(f);
    }
    page.extend(placed.map(|(_, h)| h));
    if last {
        s.follow(session, host, client);
    }
    if let Some(id) = outbox.resynced(session, page, rest, last) {
        s.send(client, session, control(&ServerFrame::Attached { id, session_id: session.to_string(), running, error: None }));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Item, LogEvent};

    fn said(id: &str) -> LogEvent {
        LogEvent { id: id.into(), parent_id: None, session_id: "s".into(), time_ms: 0, body: LogBody::ItemCompleted { turn_id: "t".into(), item_id: id.into(), item: Item::AssistantText { text: id.into() } } }
    }

    fn ex(under_way: bool, head: Option<&str>, base: usize, events: &[LogEvent], tries: u32) -> Option<(usize, bool)> {
        extent(under_way, head, base, events, tries).expect("under the cap")
    }

    /// R-LAG-4: the interleaving that lost a turn's last logged events. A
    /// client behind is paged from a read of the log taken while its turn
    /// ran; before the read is back the turn logs `item.completed` and
    /// `turn.completed` (dropped for the client, which is behind), and
    /// ends. The read, matched against the hub as it is now — nothing under
    /// way, its head the `turn.completed` the read does not have — is read
    /// again, where it was taken as the whole log: the last page, after
    /// which nothing brought those events back.
    ///
    /// And one that never reaches the head, however often it is read,
    /// lets the client go to reconnect from its cursor, rather than
    /// paging it with a gap.
    #[test]
    fn r_lag_4_a_read_taken_before_the_turn_ended_is_read_again_after_it() {
        let stale = [said("root"), said("user"), said("started")];
        let whole = [said("root"), said("user"), said("started"), said("item"), said("completed")];
        // The turn has ended since the read began.
        assert_eq!(ex(false, Some("completed"), 1, &stale, 1), None, "a read behind the head is read again, the turn over or not");
        assert_eq!(ex(false, Some("completed"), 1, &whole, 2), Some((5, false)), "all of it once it reaches the head");
        // Still under way: up to the head, the turn's frames after it.
        assert_eq!(ex(true, Some("started"), 1, &whole, 1), Some((3, true)));
        assert_eq!(ex(true, Some("item"), 1, &stale, 1), None);
        // Nothing sent live: the log before the command while one is under
        // way, all of it when none is.
        assert_eq!(ex(true, None, 2, &whole, 1), Some((2, false)));
        assert_eq!(ex(false, None, 2, &whole, 1), Some((5, false)));
        // A model switch registered (under way) whose `model.switched` is
        // written but not yet published: the read has it, the head is
        // before it, and the page stops at the head — it comes live, once.
        let switched = [said("root"), said("user"), said("done"), said("switched")];
        assert_eq!(ex(true, Some("done"), 3, &switched, 1), Some((3, true)));
        assert_eq!(ex(true, None, 3, &switched, 1), Some((3, false)), "nothing sent live yet: the log before the switch");
        // A head never reached: the client is let go after the last read,
        // under way or not, rather than paged with a gap.
        assert_eq!(ex(true, Some("gone"), 2, &whole, MAX_READS - 1), None);
        assert_eq!(extent(true, Some("gone"), 2, &whole, MAX_READS), Err(()));
        assert_eq!(extent(false, Some("gone"), 2, &whole, MAX_READS), Err(()));
    }
}
