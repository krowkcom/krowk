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

/// Makes the host configuration for clients in one working directory,
/// whose turns ask a person (`true`) or refuse what would be asked: the
/// caller's, which owns the config, the keys and the price cache. It sets
/// `permissions.approvals` from the flag.
pub type Factory = Box<dyn Fn(&Path, bool) -> Result<HostConfig, EngineError>>;

/// Serves until idle or told to stop (SIGTERM, SIGINT), on a runtime of its
/// own.
pub fn run(opts: Options, factory: Factory) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, serve(opts, factory))
}

struct State {
    factory: Factory,
    hosts: HashMap<(PathBuf, bool), Rc<Host>>,
    sessions_dir: Option<PathBuf>,
    hubs: HashMap<String, Hub>,
    clients: HashMap<u64, Client>,
    next_client: u64,
    next_hub: u64,
    /// Turns running (`prompt` and `continue` commands under way).
    working: usize,
    started: Instant,
    opts: Options,
    /// Rung whenever what idleness depends on changes.
    wake: Rc<Notify>,
    /// A client asked it to exit (`stop`).
    stopping: bool,
}

struct Hub {
    /// The host its last command ran on: where an `interrupt`, `steer` or
    /// `approve` for it goes.
    host: Rc<Host>,
    followers: Vec<u64>,
    /// The frames of the running turn, from its `turn.started`; empty
    /// between turns.
    turn: Vec<StreamLine>,
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
    /// When it was first seen, for the status listing's order.
    seen: u64,
}

struct Client {
    tx: mpsc::UnboundedSender<ServerFrame>,
    cwd: PathBuf,
    answers: bool,
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
        next_hub: 0,
        stopping: false,
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
    let (cwd, answers) = match serde_json::from_str::<ClientFrame>(&hello) {
        Ok(ClientFrame::Hello { protocol_version, cwd, answers_approvals, .. }) if protocol_version == PROTOCOL_VERSION => (PathBuf::from(cwd), answers_approvals),
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
        s.clients.insert(id, Client { tx, cwd: cwd.clone(), answers });
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
                tokio::task::spawn_local(execute(state.clone(), id, cmd_id, command));
            }
            Ok(ClientFrame::Attach { id: cmd_id, session_id, after_event_id }) => {
                tokio::task::spawn_local(attach(state.clone(), id, cmd_id, session_id, after_event_id));
            }
            Ok(ClientFrame::Status { id: cmd_id }) => {
                let s = state.borrow();
                let f = ServerFrame::Status { id: cmd_id, status: status(&s) };
                if let Some(c) = s.clients.get(&id) {
                    let _ = c.tx.send(f);
                }
            }
            Ok(ClientFrame::Stop { id: cmd_id }) => {
                let mut s = state.borrow_mut();
                let error = (s.working > 0).then(|| ErrorInfo {
                    code: "host_busy".into(),
                    message: format!("the host daemon is running {} turn(s) — let them finish (`krowk host status` lists them), then try again", s.working),
                    http_status: None,
                    resets_at_ms: None,
                });
                if error.is_none() {
                    s.stopping = true;
                    s.wake.notify_one();
                }
                if let Some(c) = s.clients.get(&id) {
                    let _ = c.tx.send(ServerFrame::Done { id: cmd_id, result: None, error });
                }
            }
            Ok(ClientFrame::Hello { .. }) => {}
            Err(e) => eprintln!("client {id}: a line that is no frame ({e}): {}", line.chars().take(200).collect::<String>()),
        }
    }
    // The client is gone; what it started runs on — but a request only it
    // could have answered is answered now, so the turn does not wait on
    // nobody.
    {
        let mut s = state.borrow_mut();
        s.clients.remove(&id);
        for h in s.hubs.values_mut() {
            h.followers.retain(|f| *f != id);
        }
        s.unanswerable();
        s.tidy();
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
        let seen = self.next_hub;
        let hub = self.hubs.entry(session.to_string()).or_insert_with(|| Hub {
            host: host.clone(),
            followers: Vec::new(),
            turn: Vec::new(),
            running: false,
            in_flight: 0,
            head: None,
            children: HashSet::new(),
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
    /// subagent's frames included, which are the parent's turn's.
    fn publish(&mut self, root: &str, host: &Rc<Host>, client: u64, line: StreamLine) {
        let hub = self.follow(root, host, client);
        let own = line.session_id() == root;
        if !own {
            hub.children.insert(line.session_id().to_string());
        }
        if let StreamLine::Log(ev) = &line
            && own
        {
            hub.head = Some(ev.id.clone());
            match ev.body {
                LogBody::TurnStarted { .. } => {
                    hub.turn.clear();
                    hub.running = true;
                }
                LogBody::TurnCompleted { .. } => {
                    hub.running = false;
                    hub.turn.clear();
                }
                _ => {}
            }
        }
        if hub.running {
            hub.turn.push(line.clone());
        }
        let asks = matches!(&line, StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(_)));
        let followers = hub.followers.clone();
        for f in &followers {
            if let Some(c) = self.clients.get(f) {
                let _ = c.tx.send(ServerFrame::Line { line: line.clone() });
            }
        }
        // Asked with nobody left who could answer: denied at once.
        if asks && !self.hubs.get(root).is_some_and(|h| self.answered(h)) {
            host.deny_waiting(&[line.session_id().to_string()]);
        }
    }
}

async fn execute(state: Shared, client: u64, id: u64, cmd: Command) {
    let reply = |state: &Shared, f: ServerFrame| {
        if let Some(c) = state.borrow().clients.get(&client) {
            let _ = c.tx.send(f);
        }
    };
    let Some((cwd, answers)) = state.borrow().clients.get(&client).map(|c| (c.cwd.clone(), c.answers)) else { return };
    let mut root = named(&cmd).map(String::from);
    let turn = matches!(cmd, Command::Prompt { .. } | Command::Continue { .. });
    // A turn runs on the host of the client that asks for it — its
    // directory, and whether it answers approvals; the rest go to the host
    // running the session.
    let known = if turn { None } else { root.as_ref().and_then(|r| state.borrow().hubs.get(r).map(|h| h.host.clone())) };
    let host = match known.map(Ok).unwrap_or_else(|| host_for(&state, &cwd, answers)) {
        Ok(h) => h,
        Err(e) => return reply(&state, ServerFrame::Done { id, result: None, error: Some(error_info(&e)) }),
    };
    if turn {
        let mut s = state.borrow_mut();
        s.working += 1;
        s.wake.notify_one();
        if let Some(r) = &root {
            let hub = s.follow(r, &host, client);
            hub.host = host.clone();
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
        s.publish(&r, &host, client, line);
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
        if let Some(h) = root.as_ref().and_then(|r| s.hubs.get_mut(r)) {
            h.in_flight = h.in_flight.saturating_sub(1);
            h.host = host.clone();
        }
        s.wake.notify_one();
    }
    let (result, error) = match r {
        Ok(r) => (r, None),
        Err(e) => (None, Some(error_info(&e))),
    };
    reply(&state, ServerFrame::Done { id, result, error });
    state.borrow_mut().tidy();
}

/// Catches `client` up on `session` and follows it from here on.
async fn attach(state: Shared, client: u64, id: u64, session: String, after: Option<String>) {
    let refuse = |state: &Shared, e: EngineError| {
        if let Some(c) = state.borrow().clients.get(&client) {
            let _ = c.tx.send(ServerFrame::Attached { id, session_id: session.clone(), running: false, error: Some(error_info(&e)) });
        }
    };
    if !log::valid_id(&session) {
        return refuse(&state, EngineError::new("bad_session", format!("{session:?} is not a session id — the sessionId a result names")));
    }
    let Some((cwd, answers)) = state.borrow().clients.get(&client).map(|c| (c.cwd.clone(), c.answers)) else { return };
    let known = state.borrow().hubs.get(&session).map(|h| h.host.clone());
    let host = match known.map(Ok).unwrap_or_else(|| host_for(&state, &cwd, answers)) {
        Ok(h) => h,
        Err(e) => return refuse(&state, e),
    };
    let Some(dir) = state.borrow().sessions_dir.clone() else { return refuse(&state, EngineError::new("no_session", "the daemon has no sessions directory")) };
    let path = dir.join(&session).join(log::EVENTS_FILE);
    // Read off the daemon's thread, which every other stream shares; then
    // matched against the hub as it is once the read is back. While a turn
    // is under way the log is replayed only up to the last event already
    // sent live (the hub's head): what lies beyond it is on its way, and
    // comes in order with the frames around it. A head the read does not
    // reach yet — the log grew while it read — is read again.
    let mut tries = 0;
    let (events, upto, tail, running) = loop {
        tries += 1;
        let p = path.clone();
        let events = match tokio::task::spawn_blocking(move || log::read_events(&p)).await {
            Ok(Ok(e)) => e,
            _ => return refuse(&state, EngineError::new("no_session", format!("there is no session {session} — `krowk sessions` lists them"))),
        };
        let n = events.len();
        let s = state.borrow();
        let Some(hub) = s.hubs.get(&session).filter(|h| h.in_flight > 0 || h.running) else {
            break (events, n, Vec::new(), false);
        };
        let at = hub.head.as_ref().and_then(|h| events.iter().position(|e| &e.id == h));
        match (&hub.head, at) {
            // Nothing sent live yet: the turn's first frames are on their
            // way; wait for them.
            (None, _) | (Some(_), None) if tries < 100 => {
                drop(s);
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            (Some(h), Some(at)) => {
                let from = hub.turn.iter().position(|l| matches!(l, StreamLine::Log(ev) if &ev.id == h)).map_or(hub.turn.len(), |i| i + 1);
                break (events, at + 1, hub.turn[from..].to_vec(), hub.running);
            }
            _ => break (events, n, Vec::new(), hub.running),
        }
    };
    let mut s = state.borrow_mut();
    s.follow(&session, &host, client);
    let from = after.as_deref().and_then(|a| events.iter().position(|e| e.id == a)).map_or(0, |i| i + 1);
    let Some(c) = s.clients.get(&client) else { return };
    for ev in events.into_iter().take(upto).skip(from) {
        let _ = c.tx.send(ServerFrame::Line { line: StreamLine::Log(ev) });
    }
    for line in tail {
        let _ = c.tx.send(ServerFrame::Line { line });
    }
    let _ = c.tx.send(ServerFrame::Attached { id, session_id: session.clone(), running, error: None });
}
