//! A synced session as the TUI's sessions are (D11): another machine's
//! session, followed through `krowk_harness::sync::viewer`, behind the same
//! `Link` seam as the daemon. Nothing here draws: the viewer's updates
//! become the `StreamLine`s the TUI already draws, and the TUI's commands
//! become the viewer's.
//!
//! - The session as the chunks hold it is the history, replayed as a
//!   resumed session's log is.
//! - A prompt sent here is a turn of the TUI's own: its lines go to the
//!   turn, which ends on its `result`. A turn begun elsewhere — the host's
//!   person typing, another viewer — is handed to the TUI as one too
//!   (`Event::Turn`), so Esc interrupts it and its approvals are answered
//!   from here.
//! - Approvals, interrupts and steering are the viewer's commands,
//!   answered by the host's ack. With no host there a command is queued,
//!   and the TUI's own wait on it (`COMMAND_WAIT`) counts it as sent.
//! - Whether a host is there, and the path the session comes by, are
//!   `Event`s for the status line.

use krowk_harness::engine::EngineError;
use krowk_harness::protocol::{ApprovalDecision, Command, LiveEvent, LogBody, LogEvent, RunResult, StreamLine};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};

pub use krowk_harness::sync::viewer::Options;

/// What the status line and the loop hear from the session besides its
/// lines.
pub enum Event {
    /// A host is on the session, or none is: prompts are queued meanwhile.
    Host(bool),
    /// The path the session comes by: `relay`, or `direct over …`.
    Path(String),
    /// Something to tell the person: why the relay cannot be joined, a
    /// command the host refused.
    Note(String),
    /// A turn begun elsewhere, for the TUI to follow as its own: its lines,
    /// then its result.
    Turn(mpsc::Receiver<StreamLine>, oneshot::Receiver<Result<Option<RunResult>, EngineError>>),
}

/// The running turn's lines go here, and its result to `done`.
struct Sink {
    out: mpsc::Sender<StreamLine>,
    done: oneshot::Sender<Result<Option<RunResult>, EngineError>>,
    /// The host took the prompt: a host leaving now leaves the turn
    /// without an end to wait for. One still queued waits for the next.
    taken: bool,
}

#[derive(Default)]
struct State {
    /// Commands handed to the viewer, in order, waiting for the id it gives
    /// them (`Update::Sent`, in the same order).
    unsent: std::collections::VecDeque<oneshot::Sender<Result<(), String>>>,
    /// By the viewer's id, waiting for the host's ack.
    sent: std::collections::HashMap<String, oneshot::Sender<Result<(), String>>>,
    /// The id of the prompt behind the running turn, until it is acked.
    prompt: Option<String>,
    sink: Option<Sink>,
}

pub struct SyncLink {
    commands: mpsc::UnboundedSender<Command>,
    state: Arc<Mutex<State>>,
    watch: broadcast::Sender<StreamLine>,
    events: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
}

/// The session as it stood on attaching, and the link to follow it on.
pub struct Opened {
    pub link: SyncLink,
    pub id: String,
    pub history: Vec<LogEvent>,
}

/// Attaches to the session: its history read from the chunks, then the
/// relay, followed by a task of its own until the link goes.
pub async fn open(o: Options) -> Result<Opened, String> {
    use krowk_harness::sync::viewer::{self, Update};
    let id = o.session.clone();
    let mut v = viewer::attach(o).await?;
    let mut history = Vec::new();
    // The first frame is the session as stored, sent as the attach answers.
    if let Some(first) = v.updates.recv().await {
        for u in first {
            if let Update::Attached { events, .. } = u {
                history.extend(events.into_iter().filter_map(|e| serde_json::from_value(e).ok()));
            }
        }
    }
    let state = Arc::new(Mutex::new(State::default()));
    let watch = broadcast::channel(256).0;
    let (events_tx, events) = mpsc::unbounded_channel();
    let pump = Pump { state: state.clone(), watch: watch.clone(), events: events_tx };
    tokio::spawn(async move {
        while let Some(batch) = v.updates.recv().await {
            for u in batch {
                pump.update(u).await;
            }
        }
        pump.gone("the link to the synced session ended");
    });
    Ok(Opened { link: SyncLink { commands: v.commands, state, watch, events: Mutex::new(Some(events)) }, id, history })
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the host said no to, as the TUI shows an error.
fn refused(e: String) -> EngineError {
    EngineError::new("host_refused", e)
}

impl SyncLink {
    /// A prompt, an approval, an interrupt or steering, sent through the
    /// viewer. A prompt answers with its turn's result; the rest with the
    /// host's ack. Anything else is the host's to decide, not a viewer's.
    pub async fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        match &cmd {
            // A rule for the project is written on the host, into its
            // repository's settings: not a viewer's to make.
            Command::Approve { decision: ApprovalDecision::AllowProject, .. } => return Err(EngineError::new("not_on_sync", "a viewer allows a call once or for the session; a rule for the project is made on the host")),
            Command::Prompt { .. } | Command::Approve { .. } | Command::Interrupt { .. } | Command::Steer { .. } => {}
            // A turn a backend began by itself is the host's to run.
            Command::Continue { .. } => return Err(EngineError::new("nothing_pending", "the host runs the turns it begins")),
            _ => return Err(EngineError::new("not_on_sync", "this session runs on another machine: a viewer prompts, steers, interrupts and answers approvals, and nothing else")),
        }
        let prompt = matches!(cmd, Command::Prompt { .. });
        let (ack, acked) = oneshot::channel();
        let (done_tx, done) = oneshot::channel();
        {
            let mut s = lock(&self.state);
            if prompt {
                s.sink = Some(Sink { out, done: done_tx, taken: false });
            }
            s.unsent.push_back(ack);
            // Under the lock, so the order handed is the order `unsent` holds.
            if self.commands.send(cmd).is_err() {
                return Err(EngineError::new("host_gone", "the link to the synced session ended"));
            }
        }
        match acked.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if prompt {
                    lock(&self.state).sink = None;
                }
                return Err(refused(e));
            }
            Err(_) => return Err(EngineError::new("host_gone", "the link to the synced session ended")),
        }
        if !prompt {
            return Ok(None);
        }
        done.await.unwrap_or_else(|_| Err(EngineError::new("host_gone", "the link to the synced session ended")))
    }

    pub fn watch(&self) -> broadcast::Receiver<StreamLine> {
        self.watch.subscribe()
    }

    /// The session's other news, taken once by the loop.
    pub fn events(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Turns the viewer's updates into lines, acks and events.
struct Pump {
    state: Arc<Mutex<State>>,
    watch: broadcast::Sender<StreamLine>,
    events: mpsc::UnboundedSender<Event>,
}

impl Pump {
    async fn update(&self, u: krowk_harness::sync::viewer::Update) {
        use krowk_harness::sync::viewer::Update;
        match u {
            Update::Line(l) => self.line(l).await,
            Update::Attached { events, .. } | Update::CaughtUp(events) => {
                for e in events.into_iter().filter_map(|e| serde_json::from_value::<LogEvent>(e).ok()) {
                    self.line(StreamLine::Log(e)).await;
                }
            }
            Update::Host(present) => {
                if !present {
                    self.host_left();
                }
                let _ = self.events.send(Event::Host(present));
            }
            Update::Path { path, via } => {
                let _ = self.events.send(Event::Path(match via {
                    Some(v) => format!("{path} {v}"),
                    None => path,
                }));
            }
            Update::Sent { id, .. } => {
                let mut s = lock(&self.state);
                if let Some(w) = s.unsent.pop_front() {
                    s.sent.insert(id.clone(), w);
                }
                if s.sink.as_ref().is_some_and(|k| !k.taken) && s.prompt.is_none() {
                    s.prompt = Some(id);
                }
            }
            Update::Acked { id, error } => self.acked(id, error),
            Update::Gap => {}
            Update::Note(n) => {
                let _ = self.events.send(Event::Note(n));
            }
            Update::Failed(e) => self.gone(&e),
        }
    }

    fn acked(&self, id: String, error: Option<String>) {
        let mut s = lock(&self.state);
        if s.prompt.as_deref() == Some(id.as_str()) {
            s.prompt = None;
            if let Some(k) = &mut s.sink {
                k.taken = true;
            }
        }
        if let Some(w) = s.sent.remove(&id) {
            let _ = w.send(match error {
                Some(e) => Err(e),
                None => Ok(()),
            });
        }
    }

    /// A line of the session: the running turn's, or — none running — one
    /// between turns, unless it starts a turn begun elsewhere, which the
    /// TUI is handed to follow.
    async fn line(&self, l: StreamLine) {
        let to = {
            let mut s = lock(&self.state);
            if s.sink.is_none() && matches!(&l, StreamLine::Log(LogEvent { body: LogBody::TurnStarted { .. }, .. })) {
                let (out, rx) = mpsc::channel(1024);
                let (done, done_rx) = oneshot::channel();
                s.sink = Some(Sink { out, done, taken: true });
                let _ = self.events.send(Event::Turn(rx, done_rx));
            }
            s.sink.as_ref().map(|k| k.out.clone())
        };
        let end = match &l {
            StreamLine::Live(LiveEvent::Result(r)) => Some(r.clone()),
            _ => None,
        };
        match to {
            Some(out) => {
                let _ = out.send(l).await;
            }
            None => {
                let _ = self.watch.send(l);
            }
        }
        if let Some(r) = end
            && let Some(k) = lock(&self.state).sink.take()
        {
            let _ = k.done.send(Ok(Some(r)));
        }
    }

    /// The host went: a turn it took ends here with no result to wait for
    /// (its end shows once a host is back, caught up); a prompt still
    /// queued waits on.
    fn host_left(&self) {
        let mut s = lock(&self.state);
        if s.sink.as_ref().is_some_and(|k| k.taken)
            && let Some(k) = s.sink.take()
        {
            let _ = k.done.send(Err(EngineError::new("host_away", "the host went away during the turn; what it does next shows once it is back")));
        }
    }

    /// The viewer ended: whatever waits on it is told, and the person too.
    fn gone(&self, why: &str) {
        let mut s = lock(&self.state);
        if let Some(k) = s.sink.take() {
            let _ = k.done.send(Err(EngineError::new("host_gone", why)));
        }
        let s = &mut *s;
        let waiting: Vec<_> = s.unsent.drain(..).chain(s.sent.drain().map(|(_, w)| w)).collect();
        for w in waiting {
            let _ = w.send(Err(why.to_string()));
        }
        let _ = self.events.send(Event::Note(why.to_string()));
    }
}
