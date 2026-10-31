//! A client of the host daemon: the socket end of what `Host::execute` is
//! in-process. `execute` takes the same command and the same stream
//! sender and answers the same result, so a client written against the
//! in-process host runs against the daemon unchanged (`headless::Transport`).
//!
//! One reader task takes the daemon's frames in order: a session's lines go
//! to the stream of the turn this client is running or following in that
//! session — by the session each `line` names, so a client running a turn
//! in one session while following another keeps the two apart — and every
//! command's `done` resolves that command. A turn's lines all arrive before
//! its `done`, as they do in-process. Lines of a followed session with no
//! stream of this client's go to `watch`, as `Host::watch` in-process.

use super::absent;
use crate::engine::EngineError;
use crate::protocol::{ClientFrame, Command, ErrorInfo, HostStatus, LiveEvent, RunResult, ServerFrame, StreamLine, PROTOCOL_VERSION};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};

/// How long a daemon has to answer a hello.
const HELLO_WAIT: Duration = Duration::from_secs(5);

pub enum ConnectError {
    /// Nothing listens: no socket, or one a dead daemon left.
    Absent,
    /// A daemon answered and cannot serve this client, or the socket is
    /// there and cannot be used.
    Failed(EngineError),
}

pub struct Client {
    tx: mpsc::UnboundedSender<ClientFrame>,
    inner: Arc<Mutex<Inner>>,
    next: AtomicU64,
    /// The daemon's pid and version, from its welcome.
    pub pid: u32,
    pub krowk_version: String,
    /// The protocol version the daemon announced in its welcome.
    pub protocol_version: u32,
    /// The daemon run its `seq`s are numbered within (`welcome.epoch`).
    epoch: u64,
    lines: broadcast::Sender<StreamLine>,
}

/// Where a stream this client had stood when its connection went: what it
/// re-follows from on the next (`Client::resume`), so a turn under way
/// comes on with no gap and nothing twice (R-LAG-4).
#[derive(Debug, Clone)]
pub struct Resume {
    pub session_id: String,
    /// The session's own last logged event handed on.
    pub after_event_id: Option<String>,
    /// And its last `line.seq`, within `epoch`.
    pub after_seq: u64,
    pub epoch: u64,
    /// A prompt cut before its first line named its session (`session_id`
    /// empty): its `execute.clientRef`, to find that session by.
    pub client_ref: Option<String>,
}

/// Why a command or follow did not answer.
#[derive(Debug)]
pub enum Cut {
    Failed(EngineError),
    /// The connection went while it streamed: re-followed from here, it
    /// goes on where it stopped.
    Dropped(Resume),
}

impl Cut {
    pub fn into_error(self) -> EngineError {
        match self {
            Cut::Failed(e) => e,
            Cut::Dropped(_) => gone(),
        }
    }
}

#[derive(Default)]
struct Inner {
    waiting: HashMap<u64, oneshot::Sender<ServerFrame>>,
    /// Where each session's lines go.
    sinks: Vec<Sink>,
    closed: bool,
    /// Each session's cursor: its last `seq` and own logged event handed
    /// to a stream.
    cursors: HashMap<String, (u64, Option<String>)>,
    /// The session each streaming command's lines named.
    sessions: HashMap<u64, String>,
    /// Every session a stream of this client's was opened for or named:
    /// lines of one no stream wants are not a lost prompt's to claim.
    asked: HashSet<String>,
}

/// A stream to open with a request: its session, where its lines go, and
/// whom to tell a followed turn's end.
type NewSink = (Option<String>, mpsc::Sender<StreamLine>, Option<oneshot::Sender<Option<RunResult>>>);

/// One stream of this client's: a turn it runs, or a session it follows.
/// Its lines are queued without bound and handed on by a task of its own,
/// so the reader never waits on a stream nobody drains — a command's
/// answer (an interrupt's, above all) is never stuck behind a full one.
struct Sink {
    /// The command or attach it is for.
    cmd: u64,
    /// The session: none for a prompt that starts one, until the daemon's
    /// first line of that command (`line.cmd`) names it.
    session: Option<String>,
    q: mpsc::UnboundedSender<Item>,
    /// A `follow`: it ends at the session's result, or when it settles.
    follow: bool,
    /// Following a lost prompt's session by its `clientRef`: named by the
    /// first line of a session this client never asked for.
    claims: bool,
}

/// What a stream's task hands on, in order.
enum Item {
    Line(StreamLine),
    /// A followed turn's end.
    End(Option<RunResult>),
    /// The command's `done`, told once every line before it is handed on.
    Done(oneshot::Sender<ServerFrame>, ServerFrame),
}

impl Sink {
    fn open(cmd: u64, session: Option<String>, out: mpsc::Sender<StreamLine>, until: Option<oneshot::Sender<Option<RunResult>>>) -> Sink {
        let (q, mut rx) = mpsc::unbounded_channel::<Item>();
        let follow = until.is_some();
        tokio::spawn(async move {
            let mut until = until;
            while let Some(item) = rx.recv().await {
                match item {
                    Item::Line(l) => {
                        let _ = out.send(l).await;
                    }
                    Item::End(r) => {
                        if let Some(u) = until.take() {
                            let _ = u.send(r);
                        }
                        return;
                    }
                    Item::Done(w, f) => {
                        let _ = w.send(f);
                        return;
                    }
                }
            }
        });
        Sink { cmd, session, q, follow, claims: false }
    }
}

impl Inner {
    /// The stream a line goes to: its command's, when it names one, else
    /// its session's.
    fn sink_for(&mut self, session: &str, cmd: Option<u64>) -> Option<usize> {
        if let Some(c) = cmd
            && let Some(i) = self.sinks.iter().position(|s| s.cmd == c)
        {
            self.sinks[i].session.get_or_insert_with(|| session.to_string());
            return Some(i);
        }
        if let Some(i) = self.sinks.iter().rposition(|s| s.session.as_deref() == Some(session)) {
            return Some(i);
        }
        // Only the daemon knows a lost prompt's session: the first one
        // this client never asked for is it. A line of a command of this
        // client's own is never a lost one's.
        if cmd.is_some() || self.asked.contains(session) {
            return None;
        }
        let i = self.sinks.iter().position(|s| s.claims && s.session.is_none())?;
        self.sinks[i].session = Some(session.to_string());
        self.sessions.insert(self.sinks[i].cmd, session.to_string());
        self.asked.insert(session.to_string());
        Some(i)
    }
}

fn gone() -> EngineError {
    EngineError::new("host_gone", "the host daemon closed the connection — the session goes on there if it was running; `krowk host status` says whether the daemon is up")
}

pub fn engine_error(e: ErrorInfo) -> EngineError {
    EngineError::new(&e.code, e.message).with_status(e.http_status.unwrap_or(0)).with_resets(e.resets_at_ms)
}

impl Client {
    /// Connects and says hello: `cwd` is where a new session this client
    /// starts runs, and `answers` whether it answers approval requests.
    pub async fn connect(path: &Path, cwd: &Path, version: &str, answers: bool) -> Result<Client, ConnectError> {
        Client::connect_as(path, cwd, version, answers, false).await
    }

    /// `connect`, saying whether the client is a background one (an
    /// automatic sync bridge): served like any other, but never holding the
    /// daemon up — its idle exit and `stop` do not count it.
    pub async fn connect_as(path: &Path, cwd: &Path, version: &str, answers: bool, background: bool) -> Result<Client, ConnectError> {
        let stream = UnixStream::connect(path).await.map_err(|e| if absent(&e) { ConnectError::Absent } else { ConnectError::Failed(EngineError::new("host_unavailable", format!("{} cannot be reached: {e}", path.display()))) })?;
        let (r, mut w) = stream.into_split();
        let hello = ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: cwd.display().to_string(), krowk_version: version.to_string(), answers_approvals: answers, token: None, background };
        let fail = |e: std::io::Error| ConnectError::Failed(EngineError::new("host_unavailable", format!("{} did not answer: {e}", path.display())));
        w.write_all((serde_json::to_string(&hello).expect("a frame serializes") + "\n").as_bytes()).await.map_err(fail)?;
        let mut lines = BufReader::new(r).lines();
        let first = match tokio::time::timeout(HELLO_WAIT, lines.next_line()).await {
            Ok(Ok(Some(l))) => l,
            // Closed before a word: a daemon on its way out.
            Ok(Ok(None)) => return Err(ConnectError::Absent),
            Ok(Err(e)) => return Err(fail(e)),
            Err(_) => return Err(ConnectError::Failed(EngineError::new("host_unavailable", format!("the host daemon on {} did not answer in {} s", path.display(), HELLO_WAIT.as_secs())))),
        };
        let (pid, krowk_version, epoch, protocol_version) = match serde_json::from_str::<ServerFrame>(&first) {
            Ok(ServerFrame::Welcome { pid, krowk_version, epoch, protocol_version }) => (pid, krowk_version, epoch, protocol_version),
            Ok(ServerFrame::Refused { code, message, fix }) => return Err(ConnectError::Failed(EngineError::new(&code, format!("{message} — {fix}")))),
            _ => return Err(ConnectError::Failed(EngineError::new("host_unavailable", format!("{} answered with something that is not a welcome", path.display())))),
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientFrame>();
        tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                if w.write_all((serde_json::to_string(&f).expect("a frame serializes") + "\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let inner = Arc::new(Mutex::new(Inner::default()));
        let lines_tx = broadcast::channel(256).0;
        {
            let (inner, lines_tx) = (inner.clone(), lines_tx.clone());
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    let f = match serde_json::from_str::<ServerFrame>(&line) {
                        Ok(f) => f,
                        // A frame of a newer daemon, or a line in it this
                        // client does not know: skipped alone, and its place
                        // in the stream still counted, so a resume does not
                        // ask for it again.
                        Err(_) => {
                            skipped(&inner, &line);
                            continue;
                        }
                    };
                    match f {
                        ServerFrame::Line { line, session, cmd, seq, .. } => {
                            let key = if session.is_empty() { line.session_id().to_string() } else { session };
                            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                            let c = i.cursors.entry(key.clone()).or_default();
                            if let Some(n) = seq {
                                c.0 = c.0.max(n);
                            }
                            if let StreamLine::Log(ev) = &line
                                && ev.session_id == key
                            {
                                c.1 = Some(ev.id.clone());
                            }
                            if let Some(c) = cmd {
                                i.sessions.entry(c).or_insert_with(|| key.clone());
                                i.asked.insert(key.clone());
                            }
                            match i.sink_for(&key, cmd) {
                                Some(n) => {
                                    let ends = i.sinks[n].follow && matches!(&line, StreamLine::Live(LiveEvent::Result(r)) if r.session_id == key);
                                    let result = match &line {
                                        StreamLine::Live(LiveEvent::Result(r)) if ends => Some(r.clone()),
                                        _ => None,
                                    };
                                    let _ = i.sinks[n].q.send(Item::Line(line));
                                    if ends {
                                        let _ = i.sinks.remove(n).q.send(Item::End(result));
                                    }
                                }
                                None => {
                                    let _ = lines_tx.send(line);
                                }
                            }
                        }
                        ServerFrame::Settled { session } => {
                            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                            while let Some(n) = i.sinks.iter().position(|s| s.follow && s.session.as_deref() == Some(session.as_str())) {
                                let _ = i.sinks.remove(n).q.send(Item::End(None));
                            }
                        }
                        ServerFrame::Done { id, .. } | ServerFrame::Attached { id, .. } | ServerFrame::Status { id, .. } => {
                            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                            let Some(w) = i.waiting.remove(&id) else { continue };
                            // A turn's `done` comes after its lines: handed
                            // on by its stream, once they are. Any other
                            // answer at once — never behind a stream.
                            match i.sinks.iter().position(|s| s.cmd == id).filter(|_| matches!(f, ServerFrame::Done { .. })) {
                                Some(n) => {
                                    let _ = i.sinks.remove(n).q.send(Item::Done(w, f));
                                }
                                None => {
                                    let _ = w.send(f);
                                }
                            }
                        }
                        ServerFrame::Welcome { .. } | ServerFrame::Refused { .. } => {}
                    }
                }
                let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                i.closed = true;
                i.sinks.clear();
                i.waiting.clear();
            });
        }
        Ok(Client { tx, inner, next: AtomicU64::new(1), pid, krowk_version, protocol_version, epoch, lines: lines_tx })
    }

    /// Frames of a followed session that arrive while no turn of this
    /// client's is streaming: what `Host::watch` is in-process.
    pub fn watch(&self) -> broadcast::Receiver<StreamLine> {
        self.lines.subscribe()
    }

    fn ask(&self, frame: impl FnOnce(u64) -> ClientFrame, sink: Option<NewSink>) -> Result<oneshot::Receiver<ServerFrame>, EngineError> {
        self.ask_id(frame, sink).map(|(_, rx)| rx)
    }

    /// Where `session`'s streams stood, to be re-followed from on another
    /// connection.
    fn resume_of(&self, session: &str) -> Resume {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (after_seq, after_event_id) = i.cursors.get(session).cloned().unwrap_or_default();
        Resume { session_id: session.to_string(), after_event_id, after_seq, epoch: self.epoch, client_ref: None }
    }

    fn ask_id(&self, frame: impl FnOnce(u64) -> ClientFrame, sink: Option<NewSink>) -> Result<(u64, oneshot::Receiver<ServerFrame>), EngineError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.closed {
                return Err(gone());
            }
            i.waiting.insert(id, tx);
            if let Some((session, tx, until)) = sink {
                if let Some(s) = &session {
                    i.asked.insert(s.clone());
                }
                i.sinks.push(Sink::open(id, session, tx, until));
            }
        }
        self.tx.send(frame(id)).map_err(|_| gone())?;
        Ok((id, rx))
    }

    /// `Host::execute`, over the socket: a `prompt` or `continue` streams
    /// its session's lines to `out` until it ends.
    pub async fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        self.execute_or_cut(cmd, out).await.map_err(Cut::into_error)
    }

    /// `execute`, telling a connection that went while the turn streamed
    /// (`Cut::Dropped`) — its session named, where it stood — from any
    /// other failure.
    pub async fn execute_or_cut(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, Cut> {
        let session = match &cmd {
            Command::Prompt { session_id, .. } => Some(session_id.clone()),
            Command::Continue { session_id, .. } => Some(Some(session_id.clone())),
            _ => None,
        };
        let known = session.clone().flatten();
        // A prompt that starts a session is named by the client too, so a
        // cut before its first line still finds what it started.
        let client_ref = matches!(session, Some(None)).then(krowk_store::new_id);
        let (id, rx) = self.ask_id(|id| ClientFrame::Execute { id, command: cmd, client_ref: client_ref.clone() }, session.map(|s| (s, out, None))).map_err(Cut::Failed)?;
        match rx.await {
            Ok(ServerFrame::Done { error: Some(e), .. }) => Err(Cut::Failed(engine_error(e))),
            Ok(ServerFrame::Done { result, .. }) => Ok(result),
            _ => {
                let named = self.inner.lock().unwrap_or_else(|e| e.into_inner()).sessions.get(&id).cloned().or(known);
                match named {
                    Some(s) => Err(Cut::Dropped(self.resume_of(&s))),
                    None => Err(client_ref.map_or(Cut::Failed(gone()), |r| Cut::Dropped(lost(r)))),
                }
            }
        }
    }

    /// Follows a session: its log after `after` (all of it when none), the
    /// running turn so far, then each frame live, to `out`. Answers whether
    /// a turn is running once caught up.
    pub async fn attach(&self, session_id: &str, after: Option<&str>, out: mpsc::Sender<StreamLine>) -> Result<bool, EngineError> {
        let rx = self.ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: None, epoch: None, client_ref: None }, Some((Some(session_id.to_string()), out, None)))?;
        match rx.await {
            Ok(ServerFrame::Attached { error: Some(e), .. }) => Err(engine_error(e)),
            Ok(ServerFrame::Attached { running, .. }) => Ok(running),
            _ => Err(gone()),
        }
    }

    /// Follows a session from `after`, not waited for: what its log holds
    /// after that event, then its frames as they come, all with what
    /// arrives while no turn of this client's streams (`watch`). What a TUI
    /// resuming a session asks when work of its goes on between turns — a
    /// backend's agent run in the background.
    pub fn attach_later(&self, session_id: &str, after: Option<&str>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).asked.insert(session_id.to_string());
        let _ = self.ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: None, epoch: None, client_ref: None }, None);
    }

    /// Follows a session's running turn to its end, as though this client
    /// ran it: what its log holds after `after`, the turn so far, then each
    /// frame live, to `out`, answering the turn's result — none when no
    /// turn is running, or the session settles without one. What the TUI
    /// reattaches with (R-HOST-1).
    pub async fn follow(&self, session_id: &str, after: Option<&str>, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        let from = Resume { session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: 0, epoch: 0, client_ref: None };
        self.resume(&from, out).await.map_err(Cut::into_error)
    }

    /// `follow` from where a stream stood on a connection that went
    /// (`Cut::Dropped`): the log after its last logged event, the running
    /// turn after its last `seq` — when the same daemon numbered it — then
    /// live. A turn no longer running by then answers none.
    pub async fn resume(&self, from: &Resume, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, Cut> {
        if let (true, Some(r)) = (from.session_id.is_empty(), &from.client_ref) {
            return self.refind(r, out).await;
        }
        let session_id = from.session_id.as_str();
        {
            // Where a second cut resumes from, should one come.
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if from.epoch == self.epoch {
                i.cursors.insert(session_id.to_string(), (from.after_seq, from.after_event_id.clone()));
            } else {
                i.cursors.insert(session_id.to_string(), (0, from.after_event_id.clone()));
            }
        }
        let (utx, urx) = oneshot::channel();
        let (after_seq, epoch) = if from.epoch == 0 { (None, None) } else { (Some(from.after_seq), Some(from.epoch)) };
        let rx = self
            .ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: from.after_event_id.clone(), after_seq, epoch, client_ref: None }, Some((Some(session_id.to_string()), out, Some(utx))))
            .map_err(Cut::Failed)?;
        let running = match rx.await {
            Ok(ServerFrame::Attached { error: Some(e), .. }) => return Err(Cut::Failed(engine_error(e))),
            Ok(ServerFrame::Attached { running, .. }) => running,
            _ => return Err(Cut::Dropped(self.resume_of(session_id))),
        };
        if !running {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.sinks.retain(|s| !(s.follow && s.session.as_deref() == Some(session_id)));
            return Ok(None);
        }
        urx.await.map_err(|_| Cut::Dropped(self.resume_of(session_id)))
    }

    /// `resume` of a prompt cut before its first line: its session, found
    /// by its `clientRef`, followed from the start to its turn's result.
    /// Never sent again. A daemon that has no such prompt — an older one,
    /// or another daemon than ran it — answers `host_gone`.
    async fn refind(&self, client_ref: &str, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, Cut> {
        let (utx, urx) = oneshot::channel();
        let (id, rx) = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Two lost prompts at once could not be told apart.
            if i.closed || i.sinks.iter().any(|s| s.claims && s.session.is_none()) {
                return Err(Cut::Failed(gone()));
            }
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            i.waiting.insert(id, tx);
            let mut sink = Sink::open(id, None, out, Some(utx));
            sink.claims = true;
            i.sinks.push(sink);
            (id, rx)
        };
        let cut = |c: &Client| {
            let named = c.inner.lock().unwrap_or_else(|e| e.into_inner()).sessions.get(&id).cloned();
            Cut::Dropped(named.map_or_else(|| lost(client_ref.to_string()), |s| c.resume_of(&s)))
        };
        let frame = ClientFrame::Attach { id, session_id: String::new(), after_event_id: None, after_seq: None, epoch: None, client_ref: Some(client_ref.to_string()) };
        self.tx.send(frame).map_err(|_| Cut::Failed(gone()))?;
        let running = match rx.await {
            Ok(ServerFrame::Attached { error: Some(e), .. }) => {
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).sinks.retain(|s| s.cmd != id);
                return Err(Cut::Failed(if e.code == "bad_session" { gone() } else { engine_error(e) }));
            }
            Ok(ServerFrame::Attached { running, .. }) => running,
            _ => return Err(cut(self)),
        };
        if !running {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).sinks.retain(|s| s.cmd != id);
            return Ok(None);
        }
        urx.await.map_err(|_| cut(self))
    }

    /// `reload`, not waited for: a client that must not stop for it (the
    /// TUI, just after `/connect`). A prompt sent after it runs on what it
    /// read: the daemon holds a command back until the reloads before it
    /// have settled.
    pub fn reload_later(&self, changed: Option<String>, renamed: Option<(String, String)>) {
        let (renamed_from, renamed_to) = renamed.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let _ = self.ask(|id| ClientFrame::Reload { id, changed, renamed_from, renamed_to }, None);
    }

    /// `reload` (see `ClientFrame::Reload`).
    pub async fn reload(&self, changed: Option<&str>, renamed: Option<(&str, &str)>) -> Result<(), EngineError> {
        let rx = self.ask(|id| ClientFrame::Reload { id, changed: changed.map(String::from), renamed_from: renamed.map(|r| r.0.to_string()), renamed_to: renamed.map(|r| r.1.to_string()) }, None)?;
        match rx.await {
            Ok(ServerFrame::Done { error: Some(e), .. }) => Err(engine_error(e)),
            Ok(ServerFrame::Done { .. }) => Ok(()),
            _ => Err(gone()),
        }
    }

    /// Whether the daemon has closed this connection: its next command
    /// needs another client (`daemon::remote`).
    pub fn closed(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).closed
    }

    /// Stops following `session_id` (`ClientFrame::Leave`).
    pub fn leave(&self, session_id: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sinks.retain(|s| s.session.as_deref() != Some(session_id));
        let _ = self.tx.send(ClientFrame::Leave { session_id: session_id.to_string() });
    }

    /// Asks the daemon to exit; refused (`host_busy`) while a turn runs, and
    /// (`host_in_use`) while another client is connected unless `force`.
    pub async fn stop(&self, force: bool) -> Result<(), EngineError> {
        let rx = self.ask(|id| ClientFrame::Stop { id, force }, None)?;
        match rx.await {
            Ok(ServerFrame::Done { error: Some(e), .. }) => Err(engine_error(e)),
            Ok(ServerFrame::Done { .. }) => Ok(()),
            _ => Err(gone()),
        }
    }

    pub async fn status(&self) -> Result<HostStatus, EngineError> {
        let rx = self.ask(|id| ClientFrame::Status { id }, None)?;
        match rx.await {
            Ok(ServerFrame::Status { status, .. }) => Ok(status),
            _ => Err(gone()),
        }
    }
}

/// Where a prompt cut before its first line is followed again from.
fn lost(client_ref: String) -> Resume {
    Resume { session_id: String::new(), after_event_id: None, after_seq: 0, epoch: 0, client_ref: Some(client_ref) }
}

/// Counts the `seq` of a `line` this client could not read in its session's
/// cursor.
fn skipped(inner: &Mutex<Inner>, raw: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else { return };
    let (Some("line"), Some(seq), Some(session)) = (v["type"].as_str(), v["seq"].as_u64(), v["session"].as_str().filter(|s| !s.is_empty())) else { return };
    let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
    let c = i.cursors.entry(session.to_string()).or_default();
    c.0 = c.0.max(seq);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::UnixListener;

    /// R-SUB-9: a line from a newer daemon that this client cannot read is
    /// skipped alone — the lines around it reach the stream, and the cursor
    /// a resume asks from counts it.
    #[tokio::test]
    async fn r_sub_9_a_line_the_client_does_not_know_is_skipped_and_the_rest_reach_its_stream() {
        let dir = std::env::temp_dir().join(format!("krowk-client-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("host.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let daemon = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            lines.next_line().await.unwrap();
            let mut send = async |v: serde_json::Value| w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
            send(json!({"type": "welcome", "protocolVersion": PROTOCOL_VERSION, "krowkVersion": "test", "pid": 1, "epoch": 7})).await;
            let attach: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let notice = |text: &str, seq: u64| json!({"type": "line", "session": "s", "seq": seq, "line": {"type": "notice", "sessionId": "s", "turnId": "t", "text": text}});
            send(notice("before", 1)).await;
            send(json!({"type": "line", "session": "s", "seq": 2, "line": {"type": "subagent.someday", "sessionId": "kid"}})).await;
            send(notice("after", 3)).await;
            send(json!({"type": "line", "session": "s", "seq": 4, "line": {"type": "subagent.someday", "sessionId": "kid"}})).await;
            send(json!({"type": "attached", "id": attach["id"], "sessionId": "s", "running": true})).await;
            lines.next_line().await.ok();
        });
        let c = Client::connect(&sock, &dir, "test", false).await.ok().expect("connected");
        let (tx, mut rx) = mpsc::channel(8);
        assert!(c.attach("s", None, tx).await.unwrap());
        let mut texts = Vec::new();
        while texts.len() < 2 {
            match rx.recv().await.unwrap() {
                StreamLine::Live(LiveEvent::Notice { text, .. }) => texts.push(text),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(texts, ["before", "after"]);
        assert_eq!(c.resume_of("s").after_seq, 4, "the last line, unread, is counted all the same");
        drop(c);
        daemon.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A daemon older than `clientRef`, its connection cut before a new
    /// session's first line: the client asks the next one after the prompt
    /// by its ref, is answered as an older daemon answers an empty session
    /// id, and says `host_gone` — the prompt is never sent again.
    #[tokio::test]
    async fn a_prompt_lost_on_a_daemon_that_ignores_its_client_ref_is_host_gone_not_sent_twice() {
        let dir = std::env::temp_dir().join(format!("krowk-client-ref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("host.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let daemon = tokio::spawn(async move {
            let mut frames = Vec::new();
            for _ in 0..2 {
                let (s, _) = listener.accept().await.unwrap();
                let (r, mut w) = s.into_split();
                let mut lines = BufReader::new(r).lines();
                lines.next_line().await.unwrap();
                w.write_all(format!("{}\n", json!({"type": "welcome", "protocolVersion": PROTOCOL_VERSION, "krowkVersion": "old", "pid": 1, "epoch": 7})).as_bytes()).await.unwrap();
                let f: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if f["type"] == "attach" {
                    // What a daemon that does not know `clientRef` answers.
                    let a = json!({"type": "attached", "id": f["id"], "sessionId": "", "running": false, "error": {"code": "bad_session", "message": "\"\" is not a session id"}});
                    w.write_all(format!("{a}\n").as_bytes()).await.unwrap();
                    lines.next_line().await.ok();
                }
                frames.push(f);
            }
            frames
        });
        let cmd = Command::Prompt { session_id: None, text: "hi".into(), images: Vec::new(), model: None, permission_mode: Default::default(), toolset: None, effort: None, budget: None };
        let first = Client::connect(&sock, &dir, "test", false).await.ok().expect("connected");
        let (tx, _rx) = mpsc::channel(8);
        let Err(Cut::Dropped(from)) = first.execute_or_cut(cmd, tx.clone()).await else { panic!("a cut prompt is to be found again") };
        assert!(from.session_id.is_empty() && from.client_ref.is_some(), "{from:?}");
        let next = Client::connect(&sock, &dir, "test", false).await.ok().expect("connected");
        let Err(Cut::Failed(e)) = next.resume(&from, tx).await else { panic!("an older daemon finds nothing") };
        assert_eq!(e.code, "host_gone");
        drop((first, next));
        let frames = daemon.await.unwrap();
        assert_eq!(frames[0]["type"], "execute");
        assert_eq!(frames[0]["clientRef"], frames[1]["clientRef"]);
        assert_eq!((frames[1]["type"].as_str(), frames[1]["sessionId"].as_str()), (Some("attach"), Some("")), "asked after, never sent again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
