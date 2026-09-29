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
use crate::protocol::{ClientFrame, Command, ErrorInfo, HostStatus, LiveEvent, LogBody, ModelRef, RunResult, ServerFrame, StreamLine, TurnStatus, Usage, PROTOCOL_VERSION};
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
    /// What its log said of the turn under way: carried to the next
    /// connection, whose catch-up starts after it.
    pub turn: Option<TurnSoFar>,
}

/// A session's latest turn as its logged events tell it: enough to answer
/// its result from the log when the turn ended while the connection was
/// down, and the `done` that carried the result went with it.
#[derive(Debug, Clone, Default)]
pub struct TurnSoFar {
    turn_id: String,
    model: Option<ModelRef>,
    /// Its last assistant text: the result's answer.
    text: String,
    calls: u32,
    ended: Option<(TurnStatus, Usage, u64, Option<ErrorInfo>)>,
}

impl TurnSoFar {
    fn saw(&mut self, body: &LogBody) {
        match body {
            LogBody::TurnStarted { turn_id, model, .. } => *self = TurnSoFar { turn_id: turn_id.clone(), model: Some(model.clone()), ..TurnSoFar::default() },
            LogBody::ItemCompleted { turn_id, item: crate::protocol::Item::AssistantText { text }, .. } if *turn_id == self.turn_id => self.text.clone_from(text),
            LogBody::ResponseCompleted { turn_id, .. } if *turn_id == self.turn_id => self.calls += 1,
            LogBody::TurnCompleted { turn_id, status, usage, duration_ms, error, .. } if *turn_id == self.turn_id => self.ended = Some((*status, *usage, *duration_ms, error.clone())),
            _ => {}
        }
    }

    /// The result of a turn the log says has ended. Its cost is not in the
    /// log, so it is unknown (null) rather than guessed.
    fn result(&self, session_id: &str) -> Option<RunResult> {
        let (status, usage, duration_ms, error) = self.ended.clone()?;
        Some(RunResult {
            session_id: session_id.to_string(),
            turn_id: self.turn_id.clone(),
            status,
            is_error: status == TurnStatus::Failed,
            result: self.text.clone(),
            model: self.model.clone()?,
            usage,
            cost_usd: None,
            duration_ms,
            num_model_calls: self.calls,
            error,
            unread_steers: Vec::new(),
            switch_offer: None,
        })
    }
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
    /// Each session's latest turn, as its logged events handed on tell it.
    turns: HashMap<String, TurnSoFar>,
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
        let (pid, krowk_version, epoch) = match serde_json::from_str::<ServerFrame>(&first) {
            Ok(ServerFrame::Welcome { pid, krowk_version, epoch, .. }) => (pid, krowk_version, epoch),
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
                                i.turns.entry(key.clone()).or_default().saw(&ev.body);
                            }
                            if let Some(c) = cmd {
                                i.sessions.entry(c).or_insert_with(|| key.clone());
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
        Ok(Client { tx, inner, next: AtomicU64::new(1), pid, krowk_version, epoch, lines: lines_tx })
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
        Resume { session_id: session.to_string(), after_event_id, after_seq, epoch: self.epoch, turn: i.turns.get(session).cloned() }
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
        let (id, rx) = self.ask_id(|id| ClientFrame::Execute { id, command: cmd }, session.map(|s| (s, out, None))).map_err(Cut::Failed)?;
        match rx.await {
            Ok(ServerFrame::Done { error: Some(e), .. }) => Err(Cut::Failed(engine_error(e))),
            Ok(ServerFrame::Done { result, .. }) => Ok(result),
            _ => {
                let named = self.inner.lock().unwrap_or_else(|e| e.into_inner()).sessions.get(&id).cloned().or(known);
                Err(named.map_or(Cut::Failed(gone()), |s| Cut::Dropped(self.resume_of(&s))))
            }
        }
    }

    /// Follows a session: its log after `after` (all of it when none), the
    /// running turn so far, then each frame live, to `out`. Answers whether
    /// a turn is running once caught up.
    pub async fn attach(&self, session_id: &str, after: Option<&str>, out: mpsc::Sender<StreamLine>) -> Result<bool, EngineError> {
        let rx = self.ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: None, epoch: None }, Some((Some(session_id.to_string()), out, None)))?;
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
        let from = Resume { session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: 0, epoch: 0, turn: None };
        self.resume(&from, out).await.map_err(Cut::into_error)
    }

    /// `follow` from where a stream stood on a connection that went
    /// (`Cut::Dropped`): the log after its last logged event, the running
    /// turn after its last `seq` — when the same daemon numbered it — then
    /// live. A turn no longer running by then answers none.
    pub async fn resume(&self, from: &Resume, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, Cut> {
        self.resume_as(from, out, false).await
    }

    /// `resume` of a turn this client's side ran and was cut from: one that
    /// ended while the connection was down answers the result its log
    /// gives — what the `done` lost with the connection would have said —
    /// once the lines caught up before it are handed on.
    pub async fn resume_turn(&self, from: &Resume, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, Cut> {
        self.resume_as(from, out, true).await
    }

    async fn resume_as(&self, from: &Resume, out: mpsc::Sender<StreamLine>, turn: bool) -> Result<Option<RunResult>, Cut> {
        let session_id = from.session_id.as_str();
        {
            // Where a second cut resumes from, should one come.
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if from.epoch == self.epoch {
                i.cursors.insert(session_id.to_string(), (from.after_seq, from.after_event_id.clone()));
            } else {
                i.cursors.insert(session_id.to_string(), (0, from.after_event_id.clone()));
            }
            // What the last connection's log said of the turn, which this
            // one's catch-up goes on from.
            if let Some(t) = &from.turn {
                i.turns.insert(session_id.to_string(), t.clone());
            }
            // A turn that had ended before the cut is an earlier one: the
            // cut one had not logged its start yet, and only an end caught
            // up from here is its.
            if turn && i.turns.get(session_id).is_some_and(|t| t.ended.is_some()) {
                i.turns.remove(session_id);
            }
        }
        let (utx, urx) = oneshot::channel();
        let (after_seq, epoch) = if from.epoch == 0 { (None, None) } else { (Some(from.after_seq), Some(from.epoch)) };
        let rx = self
            .ask(|id| ClientFrame::Attach { id, session_id: session_id.to_string(), after_event_id: from.after_event_id.clone(), after_seq, epoch }, Some((Some(session_id.to_string()), out, Some(utx))))
            .map_err(Cut::Failed)?;
        let running = match rx.await {
            Ok(ServerFrame::Attached { error: Some(e), .. }) => return Err(Cut::Failed(engine_error(e))),
            Ok(ServerFrame::Attached { running, .. }) => running,
            _ => return Err(Cut::Dropped(self.resume_of(session_id))),
        };
        if !running {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // The turn ended while the connection was down: its result, as
            // the log caught up gives it, behind the lines that brought it.
            let ended = if turn { i.turns.get(session_id).and_then(|t| t.result(session_id)) } else { None };
            let Some(r) = ended else {
                i.sinks.retain(|s| !(s.follow && s.session.as_deref() == Some(session_id)));
                return Ok(None);
            };
            match i.sinks.iter().position(|s| s.follow && s.session.as_deref() == Some(session_id)) {
                Some(n) => {
                    let _ = i.sinks.remove(n).q.send(Item::End(Some(r)));
                    i.sinks.retain(|s| !(s.follow && s.session.as_deref() == Some(session_id)));
                }
                None => return Ok(Some(r)),
            }
        }
        urx.await.map_err(|_| Cut::Dropped(self.resume_of(session_id)))
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
