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
use std::collections::HashMap;
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
    lines: broadcast::Sender<StreamLine>,
}

#[derive(Default)]
struct Inner {
    waiting: HashMap<u64, oneshot::Sender<ServerFrame>>,
    /// Where each session's lines go.
    sinks: Vec<Sink>,
    closed: bool,
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
        Sink { cmd, session, q, follow }
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
        self.sinks.iter().rposition(|s| s.session.as_deref() == Some(session))
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
        let stream = UnixStream::connect(path).await.map_err(|e| if absent(&e) { ConnectError::Absent } else { ConnectError::Failed(EngineError::new("host_unavailable", format!("{} cannot be reached: {e}", path.display()))) })?;
        let (r, mut w) = stream.into_split();
        let hello = ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: cwd.display().to_string(), krowk_version: version.to_string(), answers_approvals: answers, token: None };
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
        let (pid, krowk_version) = match serde_json::from_str::<ServerFrame>(&first) {
            Ok(ServerFrame::Welcome { pid, krowk_version, .. }) => (pid, krowk_version),
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
                    let Ok(f) = serde_json::from_str::<ServerFrame>(&line) else { continue };
                    match f {
                        ServerFrame::Line { line, session, cmd, .. } => {
                            let key = if session.is_empty() { line.session_id().to_string() } else { session };
                            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
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
        Ok(Client { tx, inner, next: AtomicU64::new(1), pid, krowk_version, lines: lines_tx })
    }

    /// Frames of a followed session that arrive while no turn of this
    /// client's is streaming: what `Host::watch` is in-process.
    pub fn watch(&self) -> broadcast::Receiver<StreamLine> {
        self.lines.subscribe()
    }

    fn ask(&self, frame: impl FnOnce(u64) -> ClientFrame, sink: Option<NewSink>) -> Result<oneshot::Receiver<ServerFrame>, EngineError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.closed {
                return Err(gone());
            }
            i.waiting.insert(id, tx);
            if let Some((session, tx, until)) = sink {
                i.sinks.push(Sink::open(id, session, tx, until));
            }
        }
        self.tx.send(frame(id)).map_err(|_| gone())?;
        Ok(rx)
    }

    /// `Host::execute`, over the socket: a `prompt` or `continue` streams
    /// its session's lines to `out` until it ends.
    pub async fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        let session = match &cmd {
            Command::Prompt { session_id, .. } => Some(session_id.clone()),
            Command::Continue { session_id, .. } => Some(Some(session_id.clone())),
            _ => None,
        };
        let rx = self.ask(|id| ClientFrame::Execute { id, command: cmd }, session.map(|s| (s, out, None)))?;
        match rx.await {
            Ok(ServerFrame::Done { error: Some(e), .. }) => Err(engine_error(e)),
            Ok(ServerFrame::Done { result, .. }) => Ok(result),
            _ => Err(gone()),
        }
    }

    /// Follows a session: its log after `after` (all of it when none), the
    /// running turn so far, then each frame live, to `out`. Answers whether
    /// a turn is running once caught up.
    pub async fn attach(&self, session_id: &str, after: Option<&str>, out: mpsc::Sender<StreamLine>) -> Result<bool, EngineError> {
        let rx = self.ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: None }, Some((Some(session_id.to_string()), out, None)))?;
        match rx.await {
            Ok(ServerFrame::Attached { error: Some(e), .. }) => Err(engine_error(e)),
            Ok(ServerFrame::Attached { running, .. }) => Ok(running),
            _ => Err(gone()),
        }
    }

    /// Follows a session's running turn to its end, as though this client
    /// ran it: what its log holds after `after`, the turn so far, then each
    /// frame live, to `out`, answering the turn's result — none when no
    /// turn is running, or the session settles without one. What the TUI
    /// reattaches with (R-HOST-1).
    pub async fn follow(&self, session_id: &str, after: Option<&str>, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        let (utx, urx) = oneshot::channel();
        let rx = self.ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: None }, Some((Some(session_id.to_string()), out, Some(utx))))?;
        let running = match rx.await {
            Ok(ServerFrame::Attached { error: Some(e), .. }) => return Err(engine_error(e)),
            Ok(ServerFrame::Attached { running, .. }) => running,
            _ => return Err(gone()),
        };
        if !running {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.sinks.retain(|s| !(s.follow && s.session.as_deref() == Some(session_id)));
            return Ok(None);
        }
        urx.await.map_err(|_| gone())
    }

    /// `reload`, not waited for: a client that must not stop for it (the
    /// TUI, just after `/connect`). The daemon takes frames in order, so a
    /// prompt sent after it runs on what it read.
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
