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
}

impl Default for Options {
    fn default() -> Options {
        Options { socket: PathBuf::new(), idle: Some(super::DEFAULT_IDLE), krowk_version: String::new(), websocket: None, heartbeat: super::ws::HEARTBEAT, caps: Caps::default(), replay_cap: replay::CAP, lateness: None }
    }
}

/// The most one write takes from a client's queue: big enough that a busy
/// session goes out in few writes, small enough that the others' frames
/// are never far behind it.
const BATCH_BYTES: usize = 256 * 1024;

/// Makes the host configuration for clients in one working directory,
/// whose turns ask a person (`true`) or refuse what would be asked: the
/// caller's, which owns the config, the keys and the price cache. It sets
/// `permissions.approvals` from the flag.
pub type Factory = Box<dyn Fn(&Path, bool) -> Result<HostConfig, EngineError>>;

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
    local.block_on(&rt, serve(opts, factory))
}

pub(super) struct State {
    factory: Factory,
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
    /// A client asked it to exit (`stop`).
    stopping: bool,
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
        factory,
        websocket: websocket.as_ref().map(|(l, t)| (l.local_addr().expect("a bound listener has an address"), t.clone())),
        hosts: HashMap::new(),
        sessions_dir: None,
        hubs: HashMap::new(),
        clients: HashMap::new(),
        next_client: 0,
        next_hub: 0,
        stopping: false,
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
    // before any turn needs it.
    tokio::task::spawn_blocking(crate::http::warm);
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
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    // Only the socket this daemon bound: one a newer daemon put in its
    // place is that one's.
    // blocking: on the way out, with nobody left to serve.
    if inode(&socket).is_some() && inode(&socket) == ours {
        let _ = std::fs::remove_file(&socket);
    }
    let hosts: Vec<Rc<Host>> = state.borrow().hosts.values().cloned().collect();
    for h in hosts {
        h.shutdown().await;
    }
    Ok(())
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
        sessions: hubs.into_iter().map(|(id, h)| HostSession { session_id: id.clone(), running: h.running, clients: h.followers.len() as u32 }).collect(),
    }
}

/// The host for clients in `cwd` that do or do not answer approvals, made
/// on first need. Its between-turns frames (`Host::watch`) go to each
/// session's followers.
fn host_for(state: &Shared, cwd: &Path, answers: bool) -> Result<Rc<Host>, EngineError> {
    let key = (cwd.to_path_buf(), answers);
    if let Some(h) = state.borrow().hosts.get(&key) {
        return Ok(h.clone());
    }
    let cfg = (state.borrow().factory)(cwd, answers)?;
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

async fn execute(state: Shared, client: u64, id: u64, cmd: Command) {
    let Some((cwd, answers)) = state.borrow().clients.get(&client).map(|c| (c.cwd.clone(), c.answers)) else { return };
    let mut root = named(&cmd).map(String::from);
    let turn = matches!(cmd, Command::Prompt { .. } | Command::Continue { .. });
    // A turn runs on the host of the client that asks for it — its
    // directory, and whether it answers approvals; the rest go to the host
    // running the session.
    let known = if turn { None } else { root.as_ref().and_then(|r| state.borrow().hubs.get(r).map(|h| h.host.clone())) };
    let host = match known.map(Ok).unwrap_or_else(|| host_for(&state, &cwd, answers)) {
        Ok(h) => h,
        Err(e) => return state.borrow_mut().send(client, "", control(&ServerFrame::Done { id, result: None, error: Some(error_info(&e)) })),
    };
    if turn {
        // Working from here: a `stop` while the log is counted below must
        // not end the daemon under a prompt it accepted.
        state.borrow_mut().working += 1;
        // Counted before it registers: nothing it writes is in the count.
        let dir = state.borrow().sessions_dir.clone();
        let base = match (&root, dir) {
            (Some(r), Some(dir)) if log::valid_id(r) => {
                let p = dir.join(r).join(log::EVENTS_FILE);
                tokio::task::spawn_blocking(move || log::read_events(&p).map(|e| e.len()).unwrap_or(0)).await.unwrap_or(0)
            }
            _ => 0,
        };
        let mut s = state.borrow_mut();
        s.wake.notify_one();
        if let Some(r) = &root {
            let hub = s.follow(r, &host, client);
            if hub.in_flight == 0 {
                hub.base = base;
            }
            hub.in_flight += 1;
        }
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
    let host = known.map(Ok).unwrap_or_else(|| host_for(state, &cwd, answers))?;
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
            _ => return Err(EngineError::new("no_session", format!("there is no session {session} — `krowk sessions` lists them"))),
        };
        if decide(state, client, session, &host, events, tries, &cursor) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Matches a read of the log against the hub as it is now, and hands the
/// client's outbox the next page, with the control frames it held put back
/// at their places — false when the read is behind what was sent live, and
/// is to be read again.
fn decide(state: &Shared, client: u64, session: &str, host: &Rc<Host>, events: Vec<crate::protocol::LogEvent>, tries: u32, cursor: &outbox::Cursor) -> bool {
    let n = events.len();
    let mut s = state.borrow_mut();
    let decided = match s.hubs.get(session).filter(|h| h.in_flight > 0 || h.running) {
        None => Some((n, Vec::new(), false)),
        Some(hub) => {
            // Under way: `running` from its `turn.started`, or a
            // command registered whose first frame is on its way.
            let running = hub.running || hub.in_flight > 0;
            let at = hub.head.as_ref().and_then(|h| events.iter().position(|e| &e.id == h));
            match (&hub.head, at) {
                // Nothing sent live yet: the log as it was before the
                // command, all of which is history.
                (None, _) => Some((hub.base.min(n), Vec::new(), running)),
                (Some(_), Some(at)) => Some((at + 1, hub.turn.after(cursor.seq), running)),
                // The read is behind what was sent: read again, and
                // past that, only what cannot come twice.
                (Some(_), None) if tries < 100 => None,
                (Some(_), None) => Some((hub.base.min(n), Vec::new(), running)),
            }
        }
    };
    let Some((upto, tail, running)) = decided else { return false };
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
