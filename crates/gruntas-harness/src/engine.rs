//! The `Engine` trait: what produces a session's turns.
//!
//! An engine is handed one turn — the branch so far, the prompt, the model —
//! and runs it to the end, reporting what happens as `EngineEvent`s on a
//! channel. It owns nothing durable: the host assigns log ids, appends the
//! log, forwards live frames and adds up the cost, so every engine's turns
//! land in the same log in the same shape (R-BACK-5).
//!
//! Backends implement it: a vendor harness (`claude -p` or `codex
//! app-server`) driving its own loop, its stream translated into these
//! events. `claude::ClaudeEngine` keeps one long-lived `claude` process per
//! session, kept by the host between turns; `codex::CodexEngine` drives
//! `codex app-server` the same way.
//!
//! The rules every engine keeps:
//!
//! - Every item is announced with `ItemStarted` before any `ItemDelta`, and
//!   finished with exactly one `ItemCompleted` under the same id — or never
//!   completed at all, when a turn is interrupted mid-item and what arrived
//!   cannot stand on its own (half a tool call's input, reasoning with no
//!   signature).
//! - `ResponseCompleted` follows the items of one model call, naming them in
//!   order: they are what was one message on the provider's wire, and are
//!   replayed as one.
//! - `Context` is sent before the first model call of a turn, with the exact
//!   system prompt and tools that call carries (R-LOG-4).
//! - Interruption is cooperative: `TurnContext::cancel` flips, the engine
//!   stops at the next point it can, and returns `TurnEnd::Interrupted`.
//! - Steering is cooperative too: what a client adds with `Command::Steer`
//!   waits in `TurnContext::steers`, and the engine takes it at its next
//!   step — before its next model call — reporting each as a `UserText`
//!   item, so the log shows where in the turn it landed. A turn does not end
//!   with steering left untaken.

use crate::budget::Budget;
use crate::evidence::Evidence;
use crate::catalog::ModelInfo;
use crate::protocol::{ApprovalDecision, ApprovalRequest, BackendAgent, Billing, Delta, Effort, ErrorInfo, HandoffKind, ImageRef, Item, ItemKind, LimitStatus, ModelRef, PermissionMode, ToolDefinition, Usage, WireApi};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch, Notify};

/// A boxed future that can move between threads: what a dyn-compatible
/// async trait method returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What an engine reports while it runs a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The system prompt and tools the turn's model calls carry.
    Context { system: String, tools: Vec<ToolDefinition> },
    ItemStarted { item_id: String, kind: ItemKind },
    ItemDelta { item_id: String, delta: Delta },
    ItemCompleted { item_id: String, item: Item },
    ResponseCompleted { response_id: Option<String>, model: String, usage: Usage, stop_reason: Option<String>, item_ids: Vec<String> },
    /// A backend's own session: its id, which a later turn resumes, where
    /// the vendor keeps its transcript, and what it is billed to. The host
    /// logs it when it is new or has changed.
    BackendSession { backend: String, session_id: String, transcript: Option<String>, billing: Option<Billing> },
    /// `publish` opened the session's krowk run: the host logs it, so every
    /// later publish — this session's next turn's too — attaches to it.
    RunOpened { run: String },
    /// A call a backend's own subagent made: metered for the budget and the
    /// session's cost, and kept out of this conversation.
    SubagentResponse { response_id: Option<String>, model: String, usage: Usage },
    /// What a backend said the whole turn cost (Claude Code's
    /// `total_cost_usd`), which counts calls krowk may not have seen.
    ReportedCost { usd: f64 },
    /// Something for the person and nobody else — never logged, never sent
    /// to a model: an anonymous upload's claim command.
    Notice { text: String },
    /// A call waits for a person's say: the host sends it to the session's
    /// clients as `approval.requested`.
    Approval(ApprovalRequest),
    /// It was answered.
    ApprovalResolved { request_id: String, decision: ApprovalDecision },
    /// A subagent started, as the child session `session_id`, answering
    /// the tool call `call_id`: the host logs the link, with who runs it
    /// and the backend's own id for it (R-SUB-7).
    SubagentStarted { call_id: String, session_id: String, description: String, agent: Option<String>, model: ModelRef, ran_by: crate::protocol::RanBy, backend_id: Option<String> },
    /// How close the instance is to its rate or usage limit, as its
    /// provider just said (R-INST-6).
    Limits(LimitStatus),
    /// Who a backend's instance is signed in as, and the models it listed
    /// when it started: the host saves both beside the usage (R-ROLL-2).
    Account { login: Option<String>, models: Vec<String> },
    /// A backend was brought up to date with turns it did not run
    /// (R-SWITCH-2, R-INST-4): how, and exactly what it was sent. Reported
    /// before the turn's `Context`, so the context record carries it.
    Handoff { how: HandoffKind, from_instance: Option<String>, summarized_turns: u32, recent_turns: u32, fell_back: Option<String>, text: String },
    /// The agents a backend runs by itself, whole, when the list changed:
    /// the host sends them on as `backend.agents`.
    BackendAgents { agents: Vec<BackendAgent> },
    /// Between turns only, through `Idle`: the backend began a turn by
    /// itself, and it waits for `Command::Continue` (`turn.unprompted`).
    Unprompted { reason: String },
    /// Through `Idle` only: a turn took the process from between turns.
    /// What was said through `Idle` before it came before the turn's
    /// events; what is said after it comes after them, the turn over.
    Woke,
    /// One of the backend's own agents started (R-SUB-7): Claude Code's
    /// task `task_id`, answering the conversation's `Agent` call `call_id`,
    /// with what the parent asked of it (R-SUB-8) and whether it runs in the
    /// background, outliving the turn. Its `BackendChild` events follow,
    /// the prompt first, then its `BackendChildEnded`; in a turn's stream
    /// or, between turns, through `Idle`.
    BackendChildStarted { task_id: String, call_id: String, description: String, agent: Option<String>, prompt: String, background: bool },
    /// One event of that agent's own conversation, in order: its items,
    /// whole (a `ToolCall` completing is when its tool starts, the
    /// `ToolResult` when it ends; each item's started and completed come
    /// together), and its responses, which carry no usage — its calls are
    /// metered once, as the parent's `SubagentResponse`. Its `Approval` and
    /// `ApprovalResolved` too: a call it asks about is its own, sent under
    /// its session (R-SUB-9), and its `Progress`.
    BackendChild { task_id: String, event: Box<EngineEvent> },
    /// Only inside `BackendChild`: the tokens the agent has used so far, as
    /// the backend counts them (Claude Code's `task_progress`), which its
    /// responses, carrying no usage, do not say.
    Progress { tokens: i64 },
    /// Only inside `BackendChild`, right before its `Approval`: the call
    /// the request `request_id` asks about (Claude Code's `tool_use_id`),
    /// whose clock starts again once it is answered.
    AskedFor { request_id: String, call_id: String },
    /// It ended, and nothing more of it follows: how, and why when the
    /// backend or krowk said.
    BackendChildEnded { task_id: String, status: crate::protocol::ChildState, error: Option<String> },
}

/// A turn a backend began by itself, waiting for `Command::Continue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Why it began, in words: `background agent “x” completed`.
    pub reason: String,
    /// The mode its process is in, which krowk accepted: the last turn's,
    /// or default after that plan turn's `ExitPlanMode` was approved. The
    /// turn runs in it — never looser than what was asked but for that.
    pub mode: Option<PermissionMode>,
}

/// Where a backend reports what happens between turns, when no turn's
/// channel is there to take it — its agents, a turn it began by itself.
/// The host hands one to each backend engine and sends what it is told to
/// its watchers (`Host::watch`).
pub type Idle = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Where an engine sends its events. Bounded, so a slow client slows the
/// stream rather than growing a buffer.
pub type Events = mpsc::Sender<EngineEvent>;

/// One turn, as the engine is handed it.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub session_id: String,
    pub turn_id: String,
    pub model: ModelRef,
    /// The branch so far, oldest first, ending with this turn's prompt.
    pub history: Vec<HistoryItem>,
    /// Where the session runs: tools resolve paths against it.
    pub cwd: PathBuf,
    /// What the turn's commands get in their environment beyond krowk's
    /// own, in the sandbox too: a krowk worktree's `GRUNTAS_PORT_BASE`.
    pub env: Vec<(String, String)>,
    /// The session's own directory, beside its log: where the images its
    /// prompts carry are kept (`crate::images`).
    pub session_dir: PathBuf,
    pub permission_mode: PermissionMode,
    /// The effort asked for, on krowk's ladder; the engine maps it onto the
    /// rungs the model takes.
    pub effort: Option<Effort>,
    /// What the catalog knows of the model, when it knows it.
    pub model_info: Option<ModelInfo>,
    /// Flips to true when the turn is to stop.
    pub cancel: watch::Receiver<bool>,
    /// Input added while the turn runs, oldest first.
    pub steers: Steers,
    /// The backend session the branch last ran in, from its
    /// `backend.session` event: what a backend resumes. None for a new
    /// session.
    pub backend_session: Option<String>,
    /// What the session has spent and may spend (R-BUDGET-1): the host
    /// checks it before the turn and as each call is metered.
    pub budget: Budget,
    /// Where `publish` sends files; none when this host publishes nothing.
    pub evidence: Option<Evidence>,
    /// The turn's permissions: every call the engine runs or is asked about
    /// is judged here, and asked about through it.
    pub gate: crate::permissions::Gate,
    /// What the repository and the person's settings bring to the turn:
    /// its MCP servers, skills and hooks.
    pub compat: std::sync::Arc<crate::compat::Compat>,
    /// For a backend: how to bring its vendor session up to date with
    /// turns it did not run (`crate::handoff`). None when there is nothing
    /// it has not seen.
    pub handoff: Option<crate::handoff::Handoff>,
    /// The turn a backend began by itself (`Command::Continue`): nothing is
    /// sent, and the engine reads the turn already under way. Its prompt
    /// item is the host's note of why, never the person's words.
    pub unprompted: bool,
}

/// The steering a running turn has been sent and not yet taken: a queue
/// the host pushes onto and the engine drains between model calls. Shared,
/// because the host's `execute(Steer)` and the engine's loop run
/// concurrently; a plain mutex, because neither holds it across an await.
///
/// Closing is what makes a steer either read or refused, never lost: the
/// engine closes the queue in the same locked step as its last check that
/// it is empty, and the host closes it when the turn is over, so a push that
/// comes after either is told so instead of being queued for nobody.
///
/// A running turn waits for steering on `arrived` rather than a timer, so a
/// backend that has gone quiet leaves krowk asleep (R-PERF-2).
#[derive(Debug, Clone, Default)]
pub struct Steers(Arc<Mutex<SteerQueue>>, Arc<Notify>);

#[derive(Debug, Default)]
struct SteerQueue {
    waiting: Vec<Steer>,
    closed: bool,
}

/// One steer: its text, and the images it carries, already kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Steer {
    pub text: String,
    pub images: Vec<ImageRef>,
}

impl From<&str> for Steer {
    fn from(text: &str) -> Steer {
        Steer { text: text.to_string(), images: Vec::new() }
    }
}

impl Steer {
    pub fn item(self) -> Item {
        Item::UserText { text: self.text, images: self.images }
    }
}

impl Steers {
    fn lock(&self) -> std::sync::MutexGuard<'_, SteerQueue> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `steer`, or hands it back when the turn takes no more.
    pub fn push(&self, steer: Steer) -> Result<(), Steer> {
        let mut q = self.lock();
        if q.closed {
            return Err(steer);
        }
        q.waiting.push(steer);
        drop(q);
        self.1.notify_one();
        Ok(())
    }

    /// Resolves once a steer has been pushed since the last time this
    /// resolved — at once if one was pushed while nobody waited, so a push
    /// between the engine's `take` and its next wait is never missed. It may
    /// resolve with the queue already drained: `take` then finds nothing.
    pub async fn arrived(&self) {
        self.1.notified().await
    }

    /// Whether nothing waits.
    pub fn is_empty(&self) -> bool {
        self.lock().waiting.is_empty()
    }

    /// Everything waiting, oldest first, leaving the queue empty.
    pub fn take(&self) -> Vec<Steer> {
        std::mem::take(&mut self.lock().waiting)
    }

    /// Closes the queue if nothing is waiting, and says whether it did: the
    /// engine's "may this turn end" — false means there is steering to read.
    pub fn close_if_empty(&self) -> bool {
        let mut q = self.lock();
        if q.waiting.is_empty() {
            q.closed = true;
        }
        q.closed
    }

    /// Returns steering to the front of the queue, oldest first, whether or
    /// not it is closed: steering a backend took but the model never read
    /// (cancelled on an interrupt).
    pub fn put_back(&self, steers: Vec<Steer>) {
        self.lock().waiting.splice(0..0, steers);
    }

    /// Closes the queue whatever it holds, and returns what was never taken.
    pub fn close(&self) -> Vec<Steer> {
        let mut q = self.lock();
        q.closed = true;
        std::mem::take(&mut q.waiting)
    }
}

/// An item of the branch, with the model call that produced it. Whether a
/// reasoning item's blob may replay is the blob's own to say — it names its
/// provider and wire API — so an item whose response never completed
/// replays as faithfully as any other.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryItem {
    pub item: Item,
    /// The index of the response this item came out of, unique within the
    /// history; none for items the host or a tool produced, and for items of
    /// a response that failed before it completed.
    pub response: Option<usize>,
}

/// How a turn that did not fail ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    Completed,
    Interrupted,
}

/// Why a turn failed: a code and a sentence naming the next action, and the
/// HTTP status the provider answered with, when one did (0 otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub code: String,
    pub message: String,
    pub status: u16,
    /// For a rate or usage limit: when the provider said it lifts, in
    /// milliseconds since the Unix epoch.
    pub resets_at_ms: Option<i64>,
}

impl EngineError {
    pub fn new(code: &str, message: impl Into<String>) -> EngineError {
        EngineError { code: code.into(), message: message.into(), status: 0, resets_at_ms: None }
    }

    pub fn with_status(self, status: u16) -> EngineError {
        EngineError { status, ..self }
    }

    pub fn with_resets(self, resets_at_ms: Option<i64>) -> EngineError {
        EngineError { resets_at_ms, ..self }
    }

    /// The instance hit its rate or usage limit: the failure a switch to
    /// another instance answers (R-INST-7), whichever engine reported it.
    pub fn limited(&self) -> bool {
        matches!(self.code.as_str(), "rate_limited" | "usage_limit") || self.status == 429
    }

    pub fn info(&self) -> ErrorInfo {
        ErrorInfo { code: self.code.clone(), message: self.message.clone(), http_status: (self.status != 0).then_some(self.status), resets_at_ms: self.resets_at_ms }
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// What produces a session's turns. Object-safe, so the host picks one per
/// instance kind at run time.
pub trait Engine: Send + Sync {
    /// The provider this engine's model calls go to, e.g. `anthropic`.
    fn provider(&self) -> &str;
    fn wire_api(&self) -> WireApi;
    /// Runs one turn to its end. Items produced before a failure were
    /// already sent and stay in the log.
    fn run_turn<'a>(&'a self, ctx: TurnContext, events: Events) -> BoxFuture<'a, Result<TurnEnd, EngineError>>;
    /// Lets go of whatever outlives a turn — a backend's process — cleanly,
    /// before the host goes away. Nothing, for an engine that keeps nothing.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    /// A turn the backend began by itself and krowk has not run yet; none
    /// for an engine that never begins one.
    fn pending(&self) -> Option<Pending> {
        None
    }
    /// Whether the session still has work under way between turns — a
    /// backend's own agents, a turn it began — so its process is not let go
    /// as idle.
    fn busy(&self) -> bool {
        false
    }
    /// Stops one of the backend's own agents alone, by the backend's id for
    /// it (R-SUB-10): once the backend has taken the stop, the agent ends
    /// as its events say. A backend that cannot answers `not_stoppable`,
    /// with why.
    fn stop_task<'a>(&'a self, task_id: &'a str) -> BoxFuture<'a, Result<(), EngineError>> {
        Box::pin(async move { Err(EngineError::new("not_stoppable", format!("this backend cannot stop one of its agents ({task_id}) alone — interrupt the session that started it to stop it with that turn"))) })
    }
}

/// Resolves once `cancel` flips to true. A switch whose owner is gone never
/// flips, so it never resolves.
pub async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    if cancel.wait_for(|c| *c).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves at `deadline`; with none, never.
pub async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Tokens in a text, estimated at four bytes a token — the ratio providers
/// quote for English and code. No tokenizer ships with krowk; what the
/// estimate is for is noticing growth, which it does whatever the true
/// ratio is.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// The tool definitions' estimated tokens, over the JSON a provider is sent
/// for them.
pub fn tools_tokens(tools: &[ToolDefinition]) -> u64 {
    tools.iter().map(|t| estimate_tokens(&serde_json::to_string(t).expect("a definition serializes"))).sum()
}

/// Runs every future to its end at once, and answers in their order: a
/// fan-out of subagents, which borrow the turn and so cannot be spawned.
pub(crate) async fn join_all<T>(mut futs: Vec<BoxFuture<'_, T>>) -> Vec<T> {
    let mut done: Vec<Option<T>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (f, slot) in futs.iter_mut().zip(done.iter_mut()) {
            if slot.is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => *slot = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending { std::task::Poll::Pending } else { std::task::Poll::Ready(()) }
    })
    .await;
    done.into_iter().map(|v| v.expect("every future finished")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race, forced: the engine makes its last "anything to read?" check
    /// and a steer arrives right after. It must be refused, back to the
    /// sender, never queued for a turn that has decided to end.
    #[test]
    fn r_proto_1_a_steer_after_the_last_check_is_refused_not_lost() {
        let texts = |v: Vec<Steer>| v.into_iter().map(|s| s.text).collect::<Vec<_>>();
        let s = Steers::default();
        s.push("first".into()).unwrap();
        assert!(!s.close_if_empty(), "steering waiting: the turn goes on");
        assert_eq!(texts(s.take()), ["first"]);
        assert!(s.close_if_empty(), "nothing waiting: the turn may end");
        assert_eq!(s.push("too late".into()), Err("too late".into()), "refused, and handed back");
        assert!(s.take().is_empty());
        let t = Steers::default();
        t.push("never read".into()).unwrap();
        assert_eq!(texts(t.close()), ["never read"], "a turn that stops early returns what it never took");
        assert!(t.push("x".into()).is_err());
    }

    /// The wake a waiting turn gets: a steer pushed between the engine's
    /// `take` and its next wait still wakes it, and a turn with nothing
    /// pushed stays asleep (R-PERF-2).
    #[tokio::test]
    async fn r_steer_1_a_steer_pushed_before_the_wait_still_wakes_it() {
        let soon = std::time::Duration::from_millis(50);
        let s = Steers::default();
        assert!(tokio::time::timeout(soon, s.arrived()).await.is_err(), "nothing pushed: no wake");
        s.push("between".into()).unwrap();
        tokio::time::timeout(soon, s.arrived()).await.expect("pushed before the wait: woken at once");
        assert!(tokio::time::timeout(soon, s.arrived()).await.is_err(), "one push, one wake");
        let waiter = tokio::spawn({
            let s = s.clone();
            async move { s.arrived().await }
        });
        tokio::task::yield_now().await;
        s.push("during".into()).unwrap();
        tokio::time::timeout(soon * 20, waiter).await.expect("pushed during the wait: woken").unwrap();
        assert_eq!(s.take().len(), 2);
    }
}
