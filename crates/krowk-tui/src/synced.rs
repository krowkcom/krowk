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
    /// The log says the turn completed: its `result` follows (after a
    /// model switch it logged, maybe), and a next turn starting ends it
    /// without one.
    completed: bool,
}

impl Sink {
    fn new(out: mpsc::Sender<StreamLine>, done: oneshot::Sender<Result<Option<RunResult>, EngineError>>, taken: bool) -> Sink {
        Sink { out, done, taken, completed: false }
    }
}

/// A command handed to the viewer, waiting for its ack.
struct Waiter {
    ack: oneshot::Sender<Result<(), String>>,
    /// The prompt behind the running turn: its ack is the turn's taking.
    prompt: bool,
}

#[derive(Default)]
struct State {
    /// Commands handed to the viewer, in order, waiting for the id it gives
    /// them (`Update::Sent`, in the same order).
    unsent: std::collections::VecDeque<Waiter>,
    /// By the viewer's id, waiting for the host's ack.
    sent: std::collections::HashMap<String, Waiter>,
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
    let pump = Pump { state: state.clone(), watch: watch.clone(), events: events_tx, session: id.clone() };
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

/// The running turn, ended.
fn end(s: &mut State, r: Result<Option<RunResult>, EngineError>) {
    if let Some(k) = s.sink.take() {
        let _ = k.done.send(r);
    }
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
                s.sink = Some(Sink::new(out, done_tx, false));
            }
            s.unsent.push_back(Waiter { ack, prompt });
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
    /// The synced session: a subagent's approval names its own, which the
    /// host refuses a viewer's answer to.
    session: String,
}

impl Pump {
    async fn update(&self, u: krowk_harness::sync::viewer::Update) {
        use krowk_harness::sync::viewer::Update;
        match u {
            Update::Line(l) => self.line(l, true).await,
            // Caught up after a gap or a reconnect: what the log holds,
            // finished turns among it. It feeds a turn already followed,
            // and never opens one.
            Update::Attached { events, .. } | Update::CaughtUp(events) => {
                for e in events.into_iter().filter_map(|e| serde_json::from_value::<LogEvent>(e).ok()) {
                    self.line(StreamLine::Log(e), false).await;
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
                // A control command the TUI stopped waiting for
                // (`COMMAND_WAIT`) is not kept for an ack that may never come.
                s.sent.retain(|_, w| !w.ack.is_closed());
                if let Some(w) = s.unsent.pop_front() {
                    if w.prompt {
                        s.prompt = Some(id.clone());
                    }
                    s.sent.insert(id, w);
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
            let _ = w.ack.send(match error {
                Some(e) => Err(e),
                None => Ok(()),
            });
        }
    }

    /// A line of the session: the running turn's, or — none running — one
    /// between turns, unless it starts a turn begun elsewhere (`live` only),
    /// which the TUI is handed to follow. A turn ends on its `result`; on
    /// its logged end when that comes caught up rather than live; or, its
    /// `result` never sent, when the next turn starts or the host goes.
    async fn line(&self, l: StreamLine, live: bool) {
        if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = &l
            && r.session_id != self.session
        {
            let _ = self.events.send(Event::Note(format!("a subagent asks to {} — it is answered on the host", r.summary)));
            return;
        }
        let to = {
            let mut s = lock(&self.state);
            let starts = matches!(&l, StreamLine::Log(LogEvent { body: LogBody::TurnStarted { .. }, .. }));
            // The next turn starting is the last one's end, its `result`
            // lost: never a turn followed forever.
            if starts && s.sink.as_ref().is_some_and(|k| k.completed) {
                end(&mut s, Ok(None));
            }
            if live && s.sink.is_none() && starts {
                let (out, rx) = mpsc::channel(1024);
                let (done, done_rx) = oneshot::channel();
                s.sink = Some(Sink::new(out, done, true));
                let _ = self.events.send(Event::Turn(rx, done_rx));
            }
            s.sink.as_ref().map(|k| k.out.clone())
        };
        let ends = match &l {
            StreamLine::Live(LiveEvent::Result(r)) => Some(r.clone()),
            _ => None,
        };
        let completes = matches!(&l, StreamLine::Log(LogEvent { body: LogBody::TurnCompleted { .. }, .. }));
        match to {
            Some(out) => {
                let _ = out.send(l).await;
            }
            None => {
                let _ = self.watch.send(l);
            }
        }
        let mut s = lock(&self.state);
        if let Some(r) = ends {
            end(&mut s, Ok(Some(r)));
        } else if completes && let Some(k) = s.sink.as_mut().filter(|k| k.taken) {
            // Caught up, its `result` was never sent live: it ends now.
            k.completed = true;
            if !live {
                end(&mut s, Ok(None));
            }
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
            let _ = w.ack.send(Err(why.to_string()));
        }
        let _ = self.events.send(Event::Note(why.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, Pump, State};
    use krowk_harness::protocol::{ApprovalRequest, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, StreamLine, TurnStatus, Usage, WireApi};
    use krowk_harness::sync::viewer::Update;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{broadcast, mpsc, oneshot};

    fn pump() -> (Pump, mpsc::UnboundedReceiver<Event>, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State::default()));
        let (events, rx) = mpsc::unbounded_channel();
        (Pump { state: state.clone(), watch: broadcast::channel(16).0, events, session: "s".into() }, rx, state)
    }

    fn ev(id: &str, body: LogBody) -> serde_json::Value {
        serde_json::to_value(LogEvent { id: id.into(), parent_id: None, session_id: "s".into(), time_ms: 0, body }).unwrap()
    }

    fn started(id: &str) -> serde_json::Value {
        ev(id, LogBody::TurnStarted { turn_id: "t".into(), model: ModelRef { instance: "claude".into(), model: "m".into() }, provider: "anthropic".into(), wire_api: WireApi::AnthropicMessages, permission_mode: PermissionMode::Default, effort: None })
    }

    fn completed(id: &str) -> serde_json::Value {
        ev(id, LogBody::TurnCompleted { turn_id: "t".into(), status: TurnStatus::Completed, usage: Usage::default(), duration_ms: 1, error: None, reported_cost_usd: None })
    }

    fn turns(rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<(mpsc::Receiver<StreamLine>, oneshot::Receiver<Result<Option<krowk_harness::protocol::RunResult>, krowk_harness::engine::EngineError>>)> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let Event::Turn(l, d) = e {
                out.push((l, d));
            }
        }
        out
    }

    /// D11: a reconnect's catch-up replays a turn that finished: drawn as
    /// history, never opened as a running turn that no `result` will close.
    #[tokio::test]
    async fn d11_a_finished_turn_caught_up_after_a_reconnect_stays_closed() {
        let (p, mut rx, state) = pump();
        p.update(Update::CaughtUp(vec![started("e1"), completed("e2")])).await;
        assert!(turns(&mut rx).is_empty(), "no turn opened from the catch-up");
        assert!(state.lock().unwrap().sink.is_none());
    }

    /// D11: a turn followed live whose `result` was lost in a gap ends on
    /// its logged end, caught up — or, live, on the next turn's start.
    #[tokio::test]
    async fn d11_a_followed_turn_ends_on_its_logged_end_when_its_result_is_lost() {
        let (p, mut rx, state) = pump();
        p.update(Update::Line(StreamLine::Log(serde_json::from_value(started("e1")).unwrap()))).await;
        let (_lines, mut done) = turns(&mut rx).pop().expect("a live turn begun elsewhere is followed");
        p.update(Update::CaughtUp(vec![completed("e2")])).await;
        assert!(matches!(done.try_recv(), Ok(Ok(None))), "ended by the caught-up end");
        assert!(state.lock().unwrap().sink.is_none());

        p.update(Update::Line(StreamLine::Log(serde_json::from_value(started("e3")).unwrap()))).await;
        let (_lines, mut done) = turns(&mut rx).pop().expect("the next");
        p.update(Update::Line(StreamLine::Log(serde_json::from_value(completed("e4")).unwrap()))).await;
        assert!(done.try_recv().is_err(), "live, its result is waited for");
        p.update(Update::Line(StreamLine::Log(serde_json::from_value(started("e5")).unwrap()))).await;
        assert!(matches!(done.try_recv(), Ok(Ok(None))), "and the next turn's start ends it");
        assert_eq!(turns(&mut rx).len(), 1, "which is followed in its turn");
    }

    /// D11: an approval's ack never reads as a queued prompt's taking: acks
    /// are matched by the command they answer.
    #[tokio::test]
    async fn d11_an_approvals_ack_is_not_the_queued_prompts() {
        let (p, _rx, state) = pump();
        let (approve, mut approve_rx) = oneshot::channel();
        let (prompt, _prompt_rx) = oneshot::channel();
        let (out, _lines) = mpsc::channel(4);
        let (done, mut done_rx) = oneshot::channel();
        {
            let mut s = state.lock().unwrap();
            s.unsent.push_back(super::Waiter { ack: approve, prompt: false });
            s.unsent.push_back(super::Waiter { ack: prompt, prompt: true });
            s.sink = Some(super::Sink::new(out, done, false));
        }
        p.update(Update::Sent { id: "a".into(), queued: true }).await;
        p.update(Update::Sent { id: "p".into(), queued: true }).await;
        p.update(Update::Acked { id: "a".into(), error: None }).await;
        assert!(matches!(approve_rx.try_recv(), Ok(Ok(()))));
        p.update(Update::Host(false)).await;
        assert!(done_rx.try_recv().is_err(), "the prompt is still queued, not ended as taken");
    }

    /// D11: a subagent's approval names its own session, whose answer the
    /// host refuses a viewer: it is said, not offered.
    #[tokio::test]
    async fn d11_a_subagents_approval_is_not_offered_to_a_viewer() {
        let (p, mut rx, _) = pump();
        let mut watch = p.watch.subscribe();
        let req = ApprovalRequest { session_id: "sub".into(), turn_id: "t".into(), request_id: "r".into(), tool: "bash".into(), input: serde_json::json!({}), summary: "run ls".into(), reason: String::new(), remember: Vec::new() };
        p.update(Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(req)))).await;
        assert!(watch.try_recv().is_err(), "no dialog");
        assert!(matches!(rx.try_recv(), Ok(Event::Note(n)) if n.contains("answered on the host")));
    }
}
