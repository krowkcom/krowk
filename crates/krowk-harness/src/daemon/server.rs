//! The daemon's side of the socket: a listener around `Host`, so a turn is
//! the host's whatever becomes of the client that asked for it.
//!
//! Every session the daemon has seen is a hub: the host it runs on, the
//! clients following it, and the frames of the turn it is running. A
//! frame goes to every follower in the order the host sent it, each through
//! its own unbounded queue, so two clients on one session see the same
//! frames — none drops what a slower one has not read. A client that
//! attaches late is sent the log up to where the running turn's frames
//! begin, then those frames, then everything live: the turn so far,
//! including the typing of the item under way, which the log alone does
//! not hold (R-LAG-1).
//!
//! One `Host` serves each working directory clients connect from: a new
//! session starts where its client is, and a resumed one keeps its own
//! directory, as it does in-process. Everything runs on one thread, as the
//! in-process host does: nothing here is shared across threads.
//!
//! It exits after the idle window with no client connected and no turn
//! running (R-HOST-1), so nothing stays resident when unused; run as a
//! service it has no window and stays.

use crate::engine::EngineError;
use crate::host::{Host, HostConfig};
use crate::log;
use crate::protocol::{ClientFrame, Command, ErrorInfo, HostSession, HostStatus, LogBody, ServerFrame, StreamLine, PROTOCOL_VERSION};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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
}

/// Makes the host configuration for clients in one working directory: the
/// caller's, which owns the config, the keys and the price cache.
pub type Factory = Box<dyn Fn(&Path) -> Result<HostConfig, EngineError>>;

/// Serves until idle or told to stop (SIGTERM, SIGINT), on a runtime of its
/// own.
pub fn run(opts: Options, factory: Factory) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, serve(opts, factory))
}

struct State {
    factory: Factory,
    hosts: HashMap<PathBuf, Rc<Host>>,
    sessions_dir: Option<PathBuf>,
    hubs: HashMap<String, Hub>,
    clients: HashMap<u64, Client>,
    next_client: u64,
    /// Turns running (`prompt` and `continue` commands under way).
    working: usize,
    started: Instant,
    opts: Options,
    /// Rung whenever what idleness depends on changes.
    wake: Rc<Notify>,
}

struct Hub {
    host: Rc<Host>,
    followers: Vec<u64>,
    /// The frames since the running turn's `turn.started`.
    turn: Vec<StreamLine>,
    running: bool,
    /// When it was first seen, for the status listing's order.
    seen: u64,
}

struct Client {
    tx: mpsc::UnboundedSender<ServerFrame>,
    cwd: PathBuf,
    /// Log events sent while catching up, not to be sent again live.
    replayed: HashSet<String>,
}

type Shared = Rc<RefCell<State>>;

/// Binds the socket, refusing when a daemon already answers on it. A socket
/// nothing answers on was left by one that died, and is replaced.
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
    let state: Shared = Rc::new(RefCell::new(State {
        factory,
        hosts: HashMap::new(),
        sessions_dir: None,
        hubs: HashMap::new(),
        clients: HashMap::new(),
        next_client: 0,
        working: 0,
        started: Instant::now(),
        opts,
        wake: wake.clone(),
    }));
    eprintln!("krowk host {} listening on {} (pid {}, idle exit {})", state.borrow().opts.krowk_version, socket.display(), std::process::id(), idle.map_or("never".into(), |d| format!("{d:?}")));
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
        let quiet = {
            let s = state.borrow();
            s.clients.is_empty() && s.working == 0
        };
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
    if inode(&socket).is_some() && inode(&socket) == ours {
        let _ = std::fs::remove_file(&socket);
    }
    let hosts: Vec<Rc<Host>> = state.borrow().hosts.values().cloned().collect();
    for h in hosts {
        h.shutdown().await;
    }
    Ok(())
}

fn inode(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().map(|m| m.ino())
}

fn error_info(e: &EngineError) -> ErrorInfo {
    ErrorInfo { code: e.code.clone(), message: e.message.clone(), http_status: (e.status != 0).then_some(e.status), resets_at_ms: e.resets_at_ms }
}

async fn connection(stream: UnixStream, state: Shared) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let hello = match lines.next_line().await {
        Ok(Some(l)) => l,
        _ => return,
    };
    let refuse = |code: &str, message: String, fix: String| ServerFrame::Refused { code: code.into(), message, fix };
    let cwd = match serde_json::from_str::<ClientFrame>(&hello) {
        Ok(ClientFrame::Hello { protocol_version, cwd, .. }) if protocol_version == PROTOCOL_VERSION => PathBuf::from(cwd),
        Ok(ClientFrame::Hello { protocol_version, .. }) => {
            let pid = std::process::id();
            let r = refuse(
                "protocol_mismatch",
                format!("this krowk speaks protocol {protocol_version}, and the host daemon (pid {pid}, krowk {}) speaks {PROTOCOL_VERSION}", state.borrow().opts.krowk_version),
                format!("stop the daemon with `kill {pid}` once its sessions are done — the next krowk starts one of its own version"),
            );
            let _ = w.write_all(frame(&r).as_bytes()).await;
            return;
        }
        _ => {
            let _ = w.write_all(frame(&refuse("bad_hello", "the first line was not a hello".into(), "send {\"type\":\"hello\",\"protocolVersion\":1,\"cwd\":\"…\"} first".into())).as_bytes()).await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerFrame>();
    let id = {
        let mut s = state.borrow_mut();
        s.next_client += 1;
        let id = s.next_client;
        let _ = tx.send(ServerFrame::Welcome { protocol_version: PROTOCOL_VERSION, krowk_version: s.opts.krowk_version.clone(), pid: std::process::id() });
        s.clients.insert(id, Client { tx, cwd: cwd.clone(), replayed: HashSet::new() });
        s.wake.notify_one();
        id
    };
    let writer = tokio::task::spawn_local(async move {
        while let Some(f) = rx.recv().await {
            if w.write_all(frame(&f).as_bytes()).await.is_err() {
                break;
            }
        }
    });
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<ClientFrame>(&line) {
            Ok(ClientFrame::Execute { id: cmd_id, command }) => {
                tokio::task::spawn_local(execute(state.clone(), id, cwd.clone(), cmd_id, command));
            }
            Ok(ClientFrame::Attach { id: cmd_id, session_id, after_event_id }) => attach(&state, id, cmd_id, &session_id, after_event_id.as_deref()),
            Ok(ClientFrame::Status { id: cmd_id }) => {
                let s = state.borrow();
                let f = ServerFrame::Status { id: cmd_id, status: status(&s) };
                if let Some(c) = s.clients.get(&id) {
                    let _ = c.tx.send(f);
                }
            }
            Ok(ClientFrame::Hello { .. }) => {}
            Err(e) => eprintln!("client {id}: a line that is no frame ({e}): {}", line.chars().take(200).collect::<String>()),
        }
    }
    // The client is gone; what it started runs on.
    {
        let mut s = state.borrow_mut();
        s.clients.remove(&id);
        for h in s.hubs.values_mut() {
            h.followers.retain(|f| *f != id);
        }
        s.wake.notify_one();
    }
    let _ = writer.await;
}

fn frame(f: &ServerFrame) -> String {
    serde_json::to_string(f).expect("a frame serializes") + "\n"
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
        sessions: hubs.into_iter().map(|(id, h)| HostSession { session_id: id.clone(), running: h.running, clients: h.followers.len() as u32 }).collect(),
    }
}

/// The host for clients in `cwd`, made on first need. Its between-turns
/// frames (`Host::watch`) go to each session's followers.
fn host_for(state: &Shared, cwd: &Path) -> Result<Rc<Host>, EngineError> {
    if let Some(h) = state.borrow().hosts.get(cwd) {
        return Ok(h.clone());
    }
    let cfg = (state.borrow().factory)(cwd)?;
    let dir = cfg.sessions_dir.clone();
    let host = Rc::new(Host::new(cfg));
    let mut s = state.borrow_mut();
    s.sessions_dir.get_or_insert(dir);
    s.hosts.insert(cwd.to_path_buf(), host.clone());
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
            let s = state.borrow();
            if let Some(hub) = s.hubs.get(line.session_id()) {
                for f in &hub.followers {
                    if let Some(c) = s.clients.get(f) {
                        let _ = c.tx.send(ServerFrame::Line { line: line.clone() });
                    }
                }
            }
        }
    });
    Ok(host)
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
        let seen = self.hubs.len() as u64;
        let hub = self.hubs.entry(session.to_string()).or_insert_with(|| Hub { host: host.clone(), followers: Vec::new(), turn: Vec::new(), running: false, seen });
        if self.clients.contains_key(&client) && !hub.followers.contains(&client) {
            hub.followers.push(client);
        }
        hub
    }

    /// One frame of a command run for `client` in `root`'s session — a
    /// subagent's frames included, which are the parent's turn's.
    fn publish(&mut self, root: &str, host: &Rc<Host>, client: u64, line: StreamLine) {
        let hub = self.follow(root, host, client);
        if let StreamLine::Log(ev) = &line
            && ev.session_id == root
        {
            match ev.body {
                LogBody::TurnStarted { .. } => {
                    hub.turn.clear();
                    hub.running = true;
                }
                LogBody::TurnCompleted { .. } => hub.running = false,
                _ => {}
            }
        }
        hub.turn.push(line.clone());
        let followers = hub.followers.clone();
        for f in followers {
            let Some(c) = self.clients.get_mut(&f) else { continue };
            if let StreamLine::Log(ev) = &line
                && c.replayed.remove(&ev.id)
            {
                continue;
            }
            let _ = c.tx.send(ServerFrame::Line { line: line.clone() });
        }
    }
}

async fn execute(state: Shared, client: u64, cwd: PathBuf, id: u64, cmd: Command) {
    let reply = |state: &Shared, f: ServerFrame| {
        if let Some(c) = state.borrow().clients.get(&client) {
            let _ = c.tx.send(f);
        }
    };
    let mut root = named(&cmd).map(String::from);
    let known = root.as_ref().and_then(|r| state.borrow().hubs.get(r).map(|h| h.host.clone()));
    let host = match known.map(Ok).unwrap_or_else(|| host_for(&state, &cwd)) {
        Ok(h) => h,
        Err(e) => return reply(&state, ServerFrame::Done { id, result: None, error: Some(error_info(&e)) }),
    };
    let turn = matches!(cmd, Command::Prompt { .. } | Command::Continue { .. });
    if turn {
        let mut s = state.borrow_mut();
        s.working += 1;
        s.wake.notify_one();
        if let Some(r) = &root {
            s.follow(r, &host, client);
        }
    }
    let (tx, mut rx) = mpsc::channel::<StreamLine>(1024);
    let exec = host.execute(cmd, tx);
    tokio::pin!(exec);
    let mut publish = |line: StreamLine| {
        let r = root.get_or_insert_with(|| line.session_id().to_string()).clone();
        state.borrow_mut().publish(&r, &host, client, line);
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
    if turn {
        let mut s = state.borrow_mut();
        s.working -= 1;
        s.wake.notify_one();
    }
    let (result, error) = match r {
        Ok(r) => (r, None),
        Err(e) => (None, Some(error_info(&e))),
    };
    reply(&state, ServerFrame::Done { id, result, error });
}

/// Catches `client` up on `session` and follows it from here on.
fn attach(state: &Shared, client: u64, id: u64, session: &str, after: Option<&str>) {
    let refuse = |state: &Shared, e: EngineError| {
        if let Some(c) = state.borrow().clients.get(&client) {
            let _ = c.tx.send(ServerFrame::Attached { id, session_id: session.to_string(), running: false, error: Some(error_info(&e)) });
        }
    };
    let Some(cwd) = state.borrow().clients.get(&client).map(|c| c.cwd.clone()) else { return };
    let host = match state.borrow().hubs.get(session).map(|h| h.host.clone()).map(Ok).unwrap_or_else(|| host_for(state, &cwd)) {
        Ok(h) => h,
        Err(e) => return refuse(state, e),
    };
    let Some(dir) = state.borrow().sessions_dir.clone() else { return refuse(state, EngineError::new("no_session", "the daemon has no sessions directory")) };
    let events = match log::read_events(&dir.join(session).join(log::EVENTS_FILE)) {
        Ok(e) => e,
        Err(_) => return refuse(state, EngineError::new("no_session", format!("there is no session {session} — `krowk sessions` lists them"))),
    };
    let mut s = state.borrow_mut();
    let hub = s.follow(session, &host, client);
    let running = hub.running;
    // A running turn's frames are held from its start; the log up to the
    // last event among them is read from disk, and what the log has beyond
    // that is still on its way to this hub, so it comes live.
    let (upto, tail): (usize, Vec<StreamLine>) = if running {
        let last = hub.turn.iter().rposition(|l| matches!(l, StreamLine::Log(_)));
        let at = last.and_then(|i| match &hub.turn[i] {
            StreamLine::Log(ev) => events.iter().position(|e| e.id == ev.id),
            _ => None,
        });
        match (last, at) {
            (Some(i), Some(at)) => (at + 1, hub.turn[i + 1..].to_vec()),
            _ => (events.len(), Vec::new()),
        }
    } else {
        (events.len(), Vec::new())
    };
    let from = after.and_then(|a| events.iter().position(|e| e.id == a)).map_or(0, |i| i + 1);
    let Some(c) = s.clients.get_mut(&client) else { return };
    for ev in events.into_iter().take(upto).skip(from) {
        c.replayed.insert(ev.id.clone());
        let _ = c.tx.send(ServerFrame::Line { line: StreamLine::Log(ev) });
    }
    for line in tail {
        let _ = c.tx.send(ServerFrame::Line { line });
    }
    let _ = c.tx.send(ServerFrame::Attached { id, session_id: session.to_string(), running, error: None });
}
