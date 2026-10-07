//! The `Engine` trait: what produces a session's turns.
//!
//! An engine is handed one turn — the branch so far, the prompt, the model —
//! and runs it to the end, reporting what happens as `EngineEvent`s on a
//! channel. It owns nothing durable: the host assigns log ids, appends the
//! log, forwards live frames and adds up the cost, so every engine's turns
//! land in the same log in the same shape (R-BACK-5).
//!
//! Two families implement it:
//!
//! - The native loop (`native::NativeEngine`), where krowk owns the prompt,
//!   the tools and the loop, and a `ModelClient` speaks one provider's wire
//!   API. The Anthropic Messages client is the first; OpenAI Responses and
//!   Chat Completions are more `ModelClient`s under the same loop.
//! - Backends (a vendor harness such as `claude -p` or `codex app-server`
//!   driving its own loop), which implement `Engine` directly and translate
//!   the vendor's stream into these events. `claude::ClaudeEngine` is the
//!   first: one long-lived `claude` process per session, kept by the host
//!   between turns; `codex::CodexEngine` drives `codex app-server` the same
//!   way.
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
use crate::toolset::Preset;
use crate::catalog::ModelInfo;
use crate::protocol::{ApprovalDecision, ApprovalRequest, BackendAgent, Billing, Delta, Effort, ErrorInfo, HandoffKind, ImageRef, Item, ItemKind, LimitStatus, ModelRef, PermissionMode, Todo, ToolDefinition, Usage, WireApi};
use crate::subagent::{AgentRun, Subagents};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

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
    /// `todo_write` replaced the session's todo list: the host logs it.
    Todos { todos: Vec<Todo> },
    /// A subagent started, as the child session `session_id`, answering
    /// the tool call `call_id`: the host logs the link.
    SubagentStarted { call_id: String, session_id: String, description: String, agent: Option<String>, model: ModelRef },
    /// How close the instance is to its rate or usage limit, as its
    /// provider just said (R-INST-6).
    Limits(LimitStatus),
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
    /// own, in the sandbox too: a krowk worktree's `KROWK_PORT_BASE`.
    pub env: Vec<(String, String)>,
    /// The session's own directory, beside its log: where the images its
    /// prompts carry are kept (`crate::images`).
    pub session_dir: PathBuf,
    pub permission_mode: PermissionMode,
    /// The toolset preset the host chose for this turn's model.
    pub preset: &'static Preset,
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
    /// session, and for one that has only run natively.
    pub backend_session: Option<String>,
    /// What the session has spent and may spend: the engine asks it before
    /// every model call it makes (R-BUDGET-1).
    pub budget: Budget,
    /// Where `publish` sends files; none when this host publishes nothing.
    pub evidence: Option<Evidence>,
    /// The build slots bash's heavy commands wait for (`crate::builds`).
    pub builds: crate::builds::Builds,
    /// The turn's permissions: every call the engine runs or is asked about
    /// is judged here, and asked about through it.
    pub gate: crate::permissions::Gate,
    /// What the repository and the person's settings bring to a native
    /// turn: instructions, skills, hooks.
    pub compat: std::sync::Arc<crate::compat::Compat>,
    /// What this turn may start subagents with; none in a subagent, which
    /// starts none of its own, and for an engine that does not offer them.
    pub subagents: Option<Subagents>,
    /// For a subagent: the definition it runs — its instructions and its
    /// tool allowlist.
    pub agent: Option<AgentRun>,
    /// For a backend: how to bring its vendor session up to date with
    /// turns it did not run (`crate::handoff`). None when there is nothing
    /// it has not seen; the native loop reads the whole branch instead.
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
#[derive(Debug, Clone, Default)]
pub struct Steers(Arc<Mutex<SteerQueue>>);

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
        Ok(())
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
    /// Whether the engine asks `TurnContext::budget` before each model call
    /// itself. The native loop does; a backend's calls are the vendor's to
    /// make, so the host checks before its turn and interrupts it when a
    /// metered call has gone over.
    fn checks_budget(&self) -> bool {
        false
    }
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
}
