//! A client of the host daemon: the socket end of what `Host::execute` is
//! in-process. `execute` takes the same command and the same stream
//! sender and answers the same result, so a client written against the
//! in-process host runs against the daemon unchanged (`headless::Transport`).
//!
//! One reader task takes the daemon's frames in order: a session's lines go
//! to the stream of the turn this client is running or following, and
//! every command's `done` resolves that command. A turn's lines all arrive
//! before its `done`, as they do in-process.

use super::absent;
use crate::engine::EngineError;
use crate::protocol::{ClientFrame, Command, ErrorInfo, HostStatus, RunResult, ServerFrame, StreamLine, PROTOCOL_VERSION};
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
    /// Where a session's lines go: the command whose turn this client is
    /// running, or the attach it is following, by id.
    sink: Option<(u64, mpsc::Sender<StreamLine>)>,
    closed: bool,
}

fn gone() -> EngineError {
    EngineError::new("host_gone", "the host daemon closed the connection — the session goes on there if it was running; `krowk host status` says whether the daemon is up")
}

pub fn engine_error(e: ErrorInfo) -> EngineError {
    EngineError::new(&e.code, e.message).with_status(e.http_status.unwrap_or(0)).with_resets(e.resets_at_ms)
}

impl Client {
    /// Connects and says hello: `cwd` is where a new session this client
    /// starts runs.
    pub async fn connect(path: &Path, cwd: &Path, version: &str) -> Result<Client, ConnectError> {
        let stream = UnixStream::connect(path).await.map_err(|e| if absent(&e) { ConnectError::Absent } else { ConnectError::Failed(EngineError::new("host_unavailable", format!("{} cannot be reached: {e}", path.display()))) })?;
        let (r, mut w) = stream.into_split();
        let hello = ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: cwd.display().to_string(), krowk_version: version.to_string() };
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
                        ServerFrame::Line { line } => {
                            let sink = inner.lock().unwrap_or_else(|e| e.into_inner()).sink.as_ref().map(|(_, s)| s.clone());
                            match sink {
                                Some(s) => {
                                    let _ = s.send(line).await;
                                }
                                None => {
                                    let _ = lines_tx.send(line);
                                }
                            }
                        }
                        ServerFrame::Done { id, .. } | ServerFrame::Attached { id, .. } | ServerFrame::Status { id, .. } => {
                            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                            // A turn's stream ends with its command.
                            if matches!(f, ServerFrame::Done { .. }) && i.sink.as_ref().is_some_and(|(s, _)| *s == id) {
                                i.sink = None;
                            }
                            if let Some(w) = i.waiting.remove(&id) {
                                let _ = w.send(f);
                            }
                        }
                        ServerFrame::Welcome { .. } | ServerFrame::Refused { .. } => {}
                    }
                }
                let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                i.closed = true;
                i.sink = None;
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

    fn ask(&self, frame: impl FnOnce(u64) -> ClientFrame, sink: Option<mpsc::Sender<StreamLine>>) -> Result<oneshot::Receiver<ServerFrame>, EngineError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.closed {
                return Err(gone());
            }
            i.waiting.insert(id, tx);
            if let Some(s) = sink {
                i.sink = Some((id, s));
            }
        }
        self.tx.send(frame(id)).map_err(|_| gone())?;
        Ok(rx)
    }

    /// `Host::execute`, over the socket: a `prompt` or `continue` streams
    /// its session's lines to `out` until it ends.
    pub async fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        let turn = matches!(cmd, Command::Prompt { .. } | Command::Continue { .. });
        let rx = self.ask(|id| ClientFrame::Execute { id, command: cmd }, turn.then_some(out))?;
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
        let rx = self.ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: after.map(String::from) }, Some(out))?;
        match rx.await {
            Ok(ServerFrame::Attached { error: Some(e), .. }) => Err(engine_error(e)),
            Ok(ServerFrame::Attached { running, .. }) => Ok(running),
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
