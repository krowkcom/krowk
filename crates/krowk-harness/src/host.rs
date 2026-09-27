//! The host: where commands are executed and the log is written. Clients
//! talk to it only through `Command` and `StreamLine`, so the in-process
//! client `krowk -p` uses today and the daemon's socket clients later speak
//! the same protocol. The in-process transport is a function call and two
//! channels.
//!
//! The host is the one writer of a session's log (R-SYNC-2): an engine
//! reports events, and the host gives each logged one its id and parent,
//! appends it, and only then forwards it — a client never sees an event the
//! log does not have.

use crate::anthropic::AnthropicClient;
use crate::catalog::ModelInfo;
use crate::chat::{ChatClient, Credential};
use crate::budget::Budget;
use crate::agents;
use crate::engine::{BoxFuture, Engine, EngineError, EngineEvent, Events, HistoryItem, Steers, TurnContext, TurnEnd};
use crate::subagent::{AgentRun, AgentsConfig, ParentTurn, Spawn, Subagents};
use crate::evidence::{Evidence, Publisher};
use crate::instances::{Auth, Registry, Resolved};
use crate::oauth;
use crate::openai::ResponsesClient;
use crate::log::{self, LogError, SessionLog};
use crate::native::{self, NativeEngine};
use crate::toolset;
use crate::handoff::{self, TurnSpan};
use crate::instances::Rollover;
use crate::protocol::{
    Billing, BudgetLimits, Command, ContextRecord, Effort, Item, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine, SwitchOffer, SwitchReason, TurnStatus, Usage,
    WireApi,
};
use crate::claude::ClaudeEngine;
use crate::codex::CodexEngine;
use crate::trust;
use crate::{compat, permissions, readiness};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{mpsc, watch, Semaphore};

/// Prices a model call: (provider, model, usage) to USD, or none when the
/// model has no price. Supplied by the caller, which owns the price cache.
pub type Pricer = Arc<dyn Fn(&str, &str, &Usage) -> Option<f64> + Send + Sync>;

/// What the catalog knows of a model: (provider, model) to its family,
/// limits, efforts and wire API, or none when the catalog does not know it.
/// The family picks the toolset preset, the wire API the client, and the
/// efforts what the ladder maps onto. Supplied by the caller, which owns the
/// models.dev cache.
pub type Catalog = Arc<dyn Fn(&str, &str) -> Option<ModelInfo> + Send + Sync>;

pub struct HostConfig {
    /// Where session logs live: `log::sessions_dir`.
    pub sessions_dir: PathBuf,
    /// The working directory a new session starts in. A resumed session
    /// keeps the one it started in, so its system prompt stays byte-identical.
    pub cwd: PathBuf,
    pub registry: Registry,
    pub krowk_version: String,
    pub pricer: Pricer,
    pub catalog: Catalog,
    /// krowk's provider credentials file, where OAuth logins live.
    pub credentials: PathBuf,
    /// Asked before a backend is spawned in a repository (R-BACK-6).
    pub trust: trust::Gate,
    /// Pushes what `publish` is handed to krowk's registry (R-EVID-1);
    /// none, and the tool says it cannot run.
    pub publisher: Option<Publisher>,
    /// The person's own settings and where krowk keeps its own: what the
    /// permission rules, instructions, skills and hooks are read from, and
    /// whether a client answers approval requests (R-PERM-1, R-PERM-2).
    pub permissions: permissions::Config,
    /// The person's agent definitions and the model listing a subagent's
    /// model is chosen from (R-SUB-1, R-SUB-5).
    pub agents: AgentsConfig,
}

/// What executes commands. Cheap to share: its state is behind one `Arc`,
/// which a turn's subagents hold too, since each is a turn of this host.
pub struct Host {
    shared: Arc<Shared>,
}

pub(crate) struct Shared {
    pub(crate) cfg: HostConfig,
    /// Each session with a turn running — a subagent's too: its cancel
    /// switch and the queue its steering waits in.
    running: Mutex<HashMap<String, Running>>,
    /// Each session's backend engine, with the instance it runs on: its
    /// process outlives a turn and serves the session's next one.
    backends: Mutex<HashMap<String, Backend>>,
    /// How long a session's backend process is kept without a turn.
    backend_idle: std::time::Duration,
    /// Approval requests waiting for a client's answer, every session's —
    /// a subagent's included.
    approvals: permissions::Approvals,
    /// What a person allowed for the rest of each session. A subagent
    /// shares its parent's: a grant for the session holds in its subagents.
    grants: Mutex<HashMap<String, permissions::SessionGrants>>,
    /// The sessions this host has run a turn of: a session's first turn
    /// here is its SessionStart.
    started: Mutex<std::collections::HashSet<String>>,
}

struct Running {
    cancel: Arc<watch::Sender<bool>>,
    steers: Steers,
    /// A `switchModel` that arrived while the turn ran: logged once it is
    /// over, since the turn holds the log.
    switch: Option<ModelRef>,
}

/// A session's backend engine, the instance it was made for, and when a
/// turn last finished on it.
struct Backend {
    instance: String,
    engine: Arc<dyn Engine>,
    used: Instant,
}

/// A backend process kept this long without a turn is let go: a long-lived
/// host (the daemon, the TUI) holds many sessions, and an idle `claude` or
/// `codex app-server` is a few hundred megabytes. The next turn starts it
/// again on the vendor's resume.
pub const BACKEND_IDLE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// How long letting go of one backend may take: its own polite stop (stdin
/// closed, five seconds, then its group stopped) and a margin.
pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

fn log_failure(e: LogError) -> EngineError {
    match e {
        LogError::NotFound(m) => EngineError::new("no_session", m),
        LogError::Busy(m) => EngineError::new("session_busy", m),
        LogError::Io(m) => EngineError::new("session_log_failed", m),
    }
}

impl Host {
    pub fn new(cfg: HostConfig) -> Host {
        Host {
            shared: Arc::new(Shared {
                cfg,
                running: Mutex::new(HashMap::new()),
                backends: Mutex::new(HashMap::new()),
                backend_idle: BACKEND_IDLE,
                approvals: permissions::Approvals::default(),
                grants: Mutex::new(HashMap::new()),
                started: Mutex::new(std::collections::HashSet::new()),
            }),
        }
    }

    /// Keeps an idle session's backend process this long instead of
    /// `BACKEND_IDLE`.
    pub fn with_backend_idle(mut self, idle: std::time::Duration) -> Host {
        Arc::get_mut(&mut self.shared).expect("a new host is not shared yet").backend_idle = idle;
        self
    }

    /// Lets every backend process go cleanly. A host dropped without this
    /// still stops them (they are killed with their handles), just less
    /// politely.
    ///
    /// Bounded: an engine that cannot be let go of in `SHUTDOWN_GRACE` — its
    /// lock held by a turn nobody polls any more — has its process group
    /// killed instead, so a host going away never waits on one.
    pub async fn shutdown(&self) {
        let engines: Vec<Arc<dyn Engine>> = self.shared.backends.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, b)| b.engine).collect();
        let mut stuck = false;
        for e in engines {
            stuck |= tokio::time::timeout(SHUTDOWN_GRACE, e.shutdown()).await.is_err();
        }
        if stuck {
            crate::group::kill_all();
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.shared.cfg.registry
    }

    /// Executes one command. A `prompt` streams its events to `out` and
    /// answers with its result — a turn that failed is still a result, with
    /// `isError`. An error here means no turn ran.
    pub async fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        let shared = &self.shared;
        match cmd {
            Command::Prompt { session_id, text, model, permission_mode, toolset, effort, budget } => {
                shared.prompt(session_id.as_deref(), text, model, permission_mode, toolset.as_deref(), effort, budget.unwrap_or_default(), out).await.map(Some)
            }
            // A subagent's session is a running turn like any other, so it
            // is interrupted alone, by its own id (R-SUB-2).
            Command::Interrupt { session_id } => {
                let running = shared.running.lock().unwrap_or_else(|e| e.into_inner());
                match running.get(&session_id) {
                    Some(r) => {
                        let _ = r.cancel.send(true);
                        Ok(None)
                    }
                    None => Err(EngineError::new("no_running_turn", format!("session {session_id} has no turn running, so there is nothing to interrupt"))),
                }
            }
            // Queued for the engine's next step; it comes back in the log as
            // a `userText` item where the turn took it.
            Command::Steer { session_id, text } => {
                if text.trim().is_empty() {
                    return Err(EngineError::new("empty_prompt", "the steering text is empty"));
                }
                let running = shared.running.lock().unwrap_or_else(|e| e.into_inner());
                match running.get(&session_id) {
                    Some(r) if r.steers.push(text).is_ok() => Ok(None),
                    // The turn has taken its last input and is ending.
                    Some(_) => Err(EngineError::new("turn_ending", format!("the turn in session {session_id} is finishing and reads no more input — send it as the next prompt"))),
                    None => Err(EngineError::new("no_running_turn", format!("session {session_id} has no turn running to steer — send it as a prompt instead"))),
                }
            }
            // Whichever client answers first decides; the turn that asked
            // tells every client it was answered (`approval.resolved`). A
            // subagent's request is answered under the subagent's session.
            Command::Approve { session_id, request_id, decision } => {
                shared.approvals.answer(&session_id, &request_id, decision).map_err(|e| EngineError::new("no_approval_request", e))?;
                Ok(None)
            }
            Command::SwitchModel { session_id, model } => shared.switch_model(session_id.as_deref(), model, out).await.map(|()| None),
            Command::Fork { .. } => Err(EngineError::new("not_implemented", "this command is part of the protocol but not served by this build yet")),
        }
    }
}

/// One turn, settled: the log it appends to, what the branch so far says,
/// and everything it runs with.
struct TurnPlan {
    log: SessionLog,
    past: Past,
    text: String,
    model: ModelRef,
    /// The instance's provider: what the turn's calls are priced under.
    provider: String,
    engine: Arc<dyn Engine>,
    preset: &'static toolset::Preset,
    wire: WireApi,
    info: Option<ModelInfo>,
    permission_mode: PermissionMode,
    effort: Option<Effort>,
    cwd: PathBuf,
    backend_session: Option<String>,
    budget: Budget,
    evidence: Option<Evidence>,
    /// The rules the turn is judged by, its instructions, skills and hooks,
    /// and the session's grants: a subagent's are its parent's.
    policy: permissions::Policy,
    compat: compat::Compat,
    grants: permissions::SessionGrants,
    /// A subagent's definition.
    agent: Option<AgentRun>,
    /// Whether the turn may start subagents: a session's own native turn,
    /// never a subagent's.
    spawns: bool,
    /// A subagent's parent turn: where its spend is also reported.
    parent: Option<ParentLink>,
    /// Flips when the parent's turn is interrupted.
    parent_cancel: Option<watch::Receiver<bool>>,
    /// Whether the result goes to the client as a `result` frame: a
    /// subagent's goes back to the tool call instead. Only such a turn — a
    /// session's own, never a subagent's, which stays on the model it was
    /// started on — follows up a failure with a switch.
    announce: bool,
    started: Instant,
    /// How a backend's thread is brought up to date (`crate::handoff`).
    handoff: Option<handoff::Handoff>,
    /// The session held in this host from before its log was opened: the
    /// turn's cancel switch, its steering, a switch for after it.
    here: Registration,
    /// A rollover this turn is the move of: logged once it has passed its
    /// checks, as it starts (R-INST-8).
    rolling: Option<Rolling>,
    /// The instances this prompt has tried already.
    tried: Vec<String>,
}

/// The parent turn a subagent's spend is reported to.
#[derive(Clone)]
struct ParentLink {
    budget: Budget,
    session_id: String,
    turn_id: String,
}

impl Shared {
    /// Lets go of every backend process idle for longer than the host keeps
    /// one, except a session's with a turn running. Swept when a prompt
    /// arrives, so an idle host costs nothing to keep tidy.
    async fn evict_idle(&self) {
        let running: Vec<String> = self.running.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
        let idle: Vec<Arc<dyn Engine>> = {
            let mut backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
            let stale: Vec<String> = backends.iter().filter(|(id, b)| b.used.elapsed() >= self.backend_idle && !running.contains(id)).map(|(id, _)| id.clone()).collect();
            stale.iter().filter_map(|id| backends.remove(id)).map(|b| b.engine).collect()
        };
        for e in idle {
            e.shutdown().await;
        }
    }

    /// The session's backend engine on `instance`: the one already running
    /// it, else a new one (and a process started by its first turn).
    fn backend_for(&self, session_id: &str, instance: &Resolved) -> Result<Arc<dyn Engine>, EngineError> {
        let mut backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = backends.get(session_id)
            && b.instance == instance.name
        {
            return Ok(b.engine.clone());
        }
        // A session moved to another instance: the old engine's process is
        // stopped with it when the last handle goes (its drop kills it).
        let e: Arc<dyn Engine> = match instance.wire_api {
            WireApi::CodexAppServer => Arc::new(CodexEngine::new(instance.clone(), &self.cfg.krowk_version)?),
            _ => Arc::new(ClaudeEngine::new(instance.clone(), &self.cfg.krowk_version)?),
        };
        backends.insert(session_id.to_string(), Backend { instance: instance.name.clone(), engine: e.clone(), used: Instant::now() });
        Ok(e)
    }

    /// A `prompt`: one turn — and, with `rollover` at `auto`, another on the
    /// next instance each time one ends on its instance's limit (R-INST-8),
    /// every instance tried once. The move is logged only once the next
    /// turn has passed every check and starts: a candidate that fails one
    /// is skipped for the next, and with none left the limited turn's
    /// result stands, the session where it was. The result is the last
    /// turn's.
    #[allow(clippy::too_many_arguments)]
    async fn prompt(
        self: &Arc<Self>,
        session_id: Option<&str>,
        text: String,
        model: Option<ModelRef>,
        permission_mode: PermissionMode,
        toolset: Option<&str>,
        effort: Option<Effort>,
        limits: BudgetLimits,
        out: mpsc::Sender<StreamLine>,
    ) -> Result<RunResult, EngineError> {
        let mut session = session_id.map(String::from);
        let mut model = model;
        let mut tried: Vec<String> = Vec::new();
        let mut rolling: Option<Rolling> = None;
        let mut limited: Option<RunResult> = None;
        loop {
            let r = match self.settle(session.as_deref(), text.clone(), model.clone(), permission_mode, toolset, effort, limits, &out, &tried, rolling.clone()).await {
                Ok(plan) => self.turn(plan, out.clone()).await,
                Err(e) => Err(e),
            };
            match (r, rolling.take()) {
                (Ok((result, Some(next))), _) => {
                    tried.push(result.model.instance.clone());
                    session = Some(result.session_id.clone());
                    model = Some(next.to.clone());
                    limited = Some(result);
                    rolling = Some(next);
                }
                (Ok((result, None)), _) => return Ok(result),
                // The instance rolled over to could not start its turn: the
                // move was never logged, so the session is where it was.
                // The next candidate, or the limit's own result.
                (Err(e), Some(r)) => {
                    tried.push(r.to.instance.clone());
                    match self.next_instance(&r.from, &tried, &r.cwd, true).await {
                        Some(to) => {
                            model = Some(to.clone());
                            rolling = Some(Rolling { to, ..r });
                        }
                        None => {
                            let mut result = limited.take().expect("a rollover follows a limited turn");
                            if let Some(err) = result.error.as_mut() {
                                err.message.push_str(&format!(" (rollover to {} could not start: {})", r.to, e.message));
                            }
                            return Ok(result);
                        }
                    }
                }
                (Err(e), None) => return Err(e),
            }
        }
    }

    /// Checks that `model` can run a turn here before a session is moved to
    /// it (R-SWITCH-4): the instance, the readiness check — its key, login
    /// or binary, a vendor's login asked of the vendor — and for a backend
    /// whether the repository is trusted to run one. `stays` is the model
    /// the session is on, named in the refusal's words.
    async fn check_model(&self, model: &ModelRef, cwd: &std::path::Path, stays: Option<&ModelRef>) -> Result<(), EngineError> {
        let staying = |e: EngineError| match stays.filter(|s| *s != model) {
            Some(s) => EngineError { message: format!("{} — the session stays on {s}", e.message), ..e },
            None => e,
        };
        let instance = self.cfg.registry.get(&model.instance).map_err(|e| staying(EngineError::new("no_instance", e)))?;
        self.ready(instance, cwd, false).await.map_err(staying)?;
        if instance.backend.is_none() {
            let info = (self.cfg.catalog)(&instance.provider, &model.model);
            let wire = instance.wire_for(info.as_ref().and_then(|i| i.wire_api));
            engine_for(instance, wire, &self.cfg.credentials, &self.cfg.krowk_version).map(drop).map_err(staying)?;
        }
        Ok(())
    }

    /// The readiness check before a turn runs on `instance` (`readiness`):
    /// what needs no process first, then for a backend the repository's
    /// trust, then the vendor's own login — so nothing at all is spawned
    /// for a repository nobody trusted. The vendor is asked in the
    /// session's own working directory — inside the root just trusted, and
    /// where the turn will start it: Claude Code reads the project settings
    /// of its working directory alone, not of its parents, so its answer
    /// there (Bedrock, an `apiKeyHelper`, a Codex model provider) is the
    /// one the turn will get. `known_good` skips the vendor: a session whose process
    /// is up and serving it has shown its login.
    async fn ready(&self, instance: &Resolved, cwd: &std::path::Path, known_good: bool) -> Result<(), EngineError> {
        let creds = &self.cfg.credentials;
        if let Some(r) = readiness::local(instance, creds)
            && let Some(e) = readiness::report(instance, r, creds).refusal(instance)
        {
            return Err(e);
        }
        if instance.backend.is_none() {
            return Ok(());
        }
        (self.cfg.trust)(&trust::root(cwd))?;
        if known_good {
            return Ok(());
        }
        let at = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        readiness::check_async(instance, creds, &readiness::Probe::at(at)).await.refusal(instance).map_or(Ok(()), Err)
    }

    /// Where a vendor is asked outside any repository: krowk's own `0700`
    /// directory beside the sessions (`readiness::neutral_dir`).
    fn neutral_probe(&self) -> Result<readiness::Probe, String> {
        let data = self.cfg.sessions_dir.parent().unwrap_or(&self.cfg.sessions_dir);
        readiness::neutral_dir(data).map(readiness::Probe::at)
    }

    /// Whether `session_id` has a backend process for `instance` up now.
    fn backend_up(&self, session_id: Option<&str>, instance: &str) -> bool {
        let Some(id) = session_id else { return false };
        self.backends.lock().unwrap_or_else(|e| e.into_inner()).get(id).is_some_and(|b| b.instance == instance)
    }

    /// `switchModel`: checked, then logged — now, or when the running turn
    /// is over — so the session's next turn runs there.
    async fn switch_model(self: &Arc<Self>, session_id: Option<&str>, model: ModelRef, out: mpsc::Sender<StreamLine>) -> Result<(), EngineError> {
        let Some(id) = session_id else {
            return self.check_model(&model, &self.cfg.cwd, None).await;
        };
        if !log::valid_id(id) {
            return Err(EngineError::new("no_session", format!("{id:?} is not a krowk session id")));
        }
        let events = log::read_events(&self.cfg.sessions_dir.join(id).join(log::EVENTS_FILE)).map_err(log_failure)?;
        let past = replay(&log::branch(&events, events.last().map(|e| e.id.as_str()).unwrap_or_default()));
        // A subagent runs one turn, on the model its definition (or
        // `subagents.model`) chose: there is no next turn to switch.
        if let Some(parent) = &past.parent {
            return Err(EngineError::new("subagent_session", format!("session {id} is a subagent of {parent}, and runs on the model it was started on — switch {parent}, whose next turn and subagents follow it")));
        }
        self.check_model(&model, past.cwd.as_deref().unwrap_or(&self.cfg.cwd), past.model.as_ref()).await?;
        {
            let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = running.get_mut(id) {
                r.switch = Some(model);
                return Ok(());
            }
        }
        if past.model.as_ref() == Some(&model) {
            return Ok(());
        }
        // A turn of this host letting go of the log right now holds it a
        // moment longer than its registration: asked again, briefly.
        let mut tries = 0;
        let (mut log, _) = loop {
            match SessionLog::open(&self.cfg.sessions_dir, id) {
                Err(LogError::Busy(_)) if tries < 40 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(r) = running.get_mut(id) {
                        r.switch = Some(model);
                        return Ok(());
                    }
                }
                r => break r.map_err(log_failure)?,
            }
        };
        let ev = log.append(LogBody::ModelSwitched { turn_id: None, from: past.model, to: model, reason: SwitchReason::Requested, detail: None }).map_err(log_failure)?;
        log.sync().map_err(log_failure)?;
        let _ = out.send(StreamLine::Log(ev)).await;
        Ok(())
    }

    /// Where a session limited on `from` may continue: the first candidate
    /// that can run here (R-INST-7, R-INST-8). A rollover (`full`) checks
    /// each as `switchModel` does: the checks that need no process, trust,
    /// then the vendor in the repository. An offer never asks about trust —
    /// that is asked when the person takes it and the turn runs there — so
    /// it asks the readiness check as `krowk status` does: the checks that
    /// need no process, then the vendor in krowk's own directory, never the
    /// repository nobody has trusted yet. A signed-out account is never
    /// offered; one whose login only a project's settings provide
    /// (Bedrock, an `apiKeyHelper`) is not offered either, and is reached
    /// by naming it.
    async fn next_instance(&self, from: &ModelRef, tried: &[String], cwd: &std::path::Path, full: bool) -> Option<ModelRef> {
        for m in self.cfg.registry.rollover_candidates(from, tried) {
            let Ok(i) = self.cfg.registry.get(&m.instance) else { continue };
            let ok = match &i.backend {
                Some(_) if !full => match self.neutral_probe() {
                    Ok(probe) => readiness::check_async(i, &self.cfg.credentials, &probe).await.refusal(i).is_none(),
                    Err(_) => false,
                },
                _ => self.check_model(&m, cwd, None).await.is_ok(),
            };
            if ok {
                return Some(m);
            }
        }
        None
    }

    /// One turn of a `prompt`, settled: everything that can refuse it
    /// checked before a new session is created, so a refusal leaves no
    /// empty session behind — and, for a turn that names another model,
    /// says the session stays where it was (R-SWITCH-4).
    #[allow(clippy::too_many_arguments)]
    async fn settle(
        self: &Arc<Self>,
        session_id: Option<&str>,
        text: String,
        model: Option<ModelRef>,
        permission_mode: PermissionMode,
        toolset: Option<&str>,
        effort: Option<Effort>,
        limits: BudgetLimits,
        out: &mpsc::Sender<StreamLine>,
        tried: &[String],
        rolling: Option<Rolling>,
    ) -> Result<TurnPlan, EngineError> {
        let started = Instant::now();
        self.evict_idle().await;
        if text.trim().is_empty() {
            return Err(EngineError::new("empty_prompt", "the prompt is empty — pass it as an argument, or on stdin"));
        }
        // The turn is this host's from before its log is opened until it
        // lets go, so a `switchModel` in between is kept for it rather than
        // meeting the log's lock.
        let mut here = Registration::new(self.clone());
        if let Some(id) = session_id {
            here.register(id)?;
        }
        let opened = match session_id {
            Some(id) => Some(SessionLog::open(&self.cfg.sessions_dir, id).map_err(log_failure)?),
            None => None,
        };
        let past = opened.as_ref().map(|(log, events)| replay(&log::branch(events, log.head().unwrap_or_default()))).unwrap_or_default();
        let model = match model {
            Some(m) => m,
            None => match past.model.clone() {
                Some(m) => m,
                None => self.cfg.registry.default_model().map_err(|e| EngineError::new("bad_config", e))?,
            },
        };
        let staying = |e: EngineError| match past.model.as_ref().filter(|p| **p != model) {
            Some(p) => EngineError { message: format!("{} — the session stays on {p}", e.message), ..e },
            None => e,
        };
        let instance = self.cfg.registry.get(&model.instance).map_err(|e| staying(EngineError::new("no_instance", e)))?.clone();
        let info = (self.cfg.catalog)(&instance.provider, &model.model);
        let family = info.as_ref().and_then(|i| i.family.clone());
        let (preset, _) = toolset::choose(toolset, self.cfg.registry.toolset.as_deref(), family.as_deref(), &model.model).map_err(|e| EngineError::new("bad_toolset", e))?;
        let wire = instance.wire_for(info.as_ref().and_then(|i| i.wire_api));
        let cwd = past.cwd.clone().unwrap_or_else(|| self.cfg.cwd.clone());
        let native = match &instance.backend {
            None => Some(engine_for(&instance, wire, &self.cfg.credentials, &self.cfg.krowk_version).map_err(staying)?),
            // A backend runs the repository's own hooks and MCP servers, so
            // it is not started in one nobody trusted; and a binary that is
            // not there, or a login that is not, is named before a session
            // exists for it.
            Some(_) => {
                self.ready(&instance, &cwd, self.backend_up(session_id, &instance.name)).await.map_err(staying)?;
                None
            }
        };
        let effort = effort.or(instance.effort);
        // The rules, instructions, skills and hooks for where the session
        // runs. A settings file that does not parse refuses the prompt: a
        // deny rule it held would otherwise silently stop holding.
        let mut policy = permissions::Policy::load(&self.cfg.permissions, &cwd).map_err(|e| EngineError::new("bad_settings", format!("{e} — fix the file, then send the prompt again")))?;
        let mut compat = compat::Compat::load(&self.cfg.permissions, &cwd, policy.loaded.hooks.clone());
        policy.read_dirs = compat.skills.iter().map(|k| k.dir.clone()).collect();
        let (log, events) = match opened {
            Some(opened) => opened,
            None => {
                let (log, root) = SessionLog::create(&self.cfg.sessions_dir, &self.cfg.cwd, &self.cfg.krowk_version).map_err(log_failure)?;
                here.register(&log.session_id)?;
                let _ = out.send(StreamLine::Log(root.clone())).await;
                (log, vec![root])
            }
        };
        let session_id = log.session_id.clone();
        let spawns = native.is_some();
        let engine: Arc<dyn Engine> = match native {
            Some(e) => Arc::from(e),
            None => self.backend_for(&session_id, &instance)?,
        };
        // A backend keeps its own thread and reads nothing of the log: the
        // thread it resumes — its own, or another account's of the same
        // vendor copied over — and what to tell it of the turns it did not
        // run (`handoff`). The native loop reads the whole branch.
        let (backend_session, handoff) = match &instance.backend {
            Some(b) => self.carry_over(&past, &instance, b, &text),
            None => (None, None),
        };
        // Everything the session and its subagents have spent, from their
        // logs; this turn's calls are added as they are metered.
        let budget = Budget::new(limits, &session_id, &self.cfg.sessions_dir, self.cfg.pricer.clone(), &instance.provider, &model.model, &events);
        drop(events);
        let evidence = self.cfg.publisher.clone().map(|p| Evidence::new(p, &session_id, past.run.clone()));
        let first_here = self.started.lock().unwrap_or_else(|e| e.into_inner()).insert(session_id.clone());
        compat.session_start = first_here.then_some(if past.items.is_empty() { "startup" } else { "resume" });
        compat.transcript = self.cfg.sessions_dir.join(&session_id).join(log::EVENTS_FILE).display().to_string();
        let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner()).entry(session_id.clone()).or_default().clone();
        let plan = TurnPlan {
            log,
            past,
            text,
            model,
            provider: instance.provider.clone(),
            engine,
            preset,
            wire,
            info,
            permission_mode,
            effort,
            cwd,
            backend_session,
            budget,
            evidence,
            policy,
            compat,
            grants,
            agent: None,
            spawns,
            parent: None,
            parent_cancel: None,
            announce: true,
            started,
            handoff,
            here,
            rolling,
            tried: tried.to_vec(),
        };
        Ok(plan)
    }

    /// A subagent (R-SUB-1): a child session of the parent turn `spawn`
    /// describes, answering its tool call `call_id` with one turn on
    /// `model`. Its lines go to the parent's client; its result comes back
    /// here, for the tool call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn subagent(self: &Arc<Self>, spawn: &Spawn, call_id: &str, description: &str, prompt: &str, model: ModelRef, run: AgentRun, events: &Events) -> Result<RunResult, EngineError> {
        let instance = self.cfg.registry.get(&model.instance).map_err(|e| EngineError::new("no_instance", e))?.clone();
        // A vendor runs its own agents, with its own tools: it could not be
        // held to the allowlist, so a subagent is always krowk's own loop.
        if instance.backend.is_some() {
            return Err(EngineError::new(
                "bad_subagent_model",
                format!("{model} runs on a vendor's own agent, and subagents run on krowk's own loop — name an API model in the agent definition or in `subagents.model`"),
            ));
        }
        let info = (self.cfg.catalog)(&instance.provider, &model.model);
        let family = info.as_ref().and_then(|i| i.family.clone());
        let (preset, _) = toolset::choose(None, self.cfg.registry.toolset.as_deref(), family.as_deref(), &model.model).map_err(|e| EngineError::new("bad_toolset", e))?;
        let wire = instance.wire_for(info.as_ref().and_then(|i| i.wire_api));
        let engine = engine_for(&instance, wire, &self.cfg.credentials, &self.cfg.krowk_version)?;
        let p = &spawn.parent;
        let (log, root) = SessionLog::create_child(&self.cfg.sessions_dir, &p.cwd, &self.cfg.krowk_version, Some(&p.session_id), run.name.as_deref()).map_err(log_failure)?;
        let _ = spawn.out.send(StreamLine::Log(root.clone())).await;
        let child = log.session_id.clone();
        let _ = events.send(EngineEvent::SubagentStarted { call_id: call_id.into(), session_id: child.clone(), description: description.into(), agent: run.name.clone(), model: model.clone() }).await;
        let budget = Budget::for_subagent(&p.budget, &child, &instance.provider, &model.model, std::slice::from_ref(&root));
        let plan = TurnPlan {
            log,
            past: Past { cwd: Some(p.cwd.clone()), ..Past::default() },
            text: prompt.into(),
            model,
            provider: instance.provider.clone(),
            engine: Arc::from(engine),
            preset,
            wire,
            info,
            permission_mode: p.permission_mode,
            effort: instance.effort,
            cwd: p.cwd.clone(),
            backend_session: None,
            budget,
            evidence: p.evidence.as_ref().map(|e| e.for_subagent(events.clone())),
            // The parent's rules, instructions, skills and hooks, and its
            // session's grants: a subagent is judged as its parent would be,
            // in its parent's mode, and asks under its own session id.
            policy: p.gate.policy().clone(),
            compat: compat::Compat { session_start: None, transcript: self.cfg.sessions_dir.join(&child).join(log::EVENTS_FILE).display().to_string(), ..(*p.compat).clone() },
            grants: p.grants.clone(),
            agent: Some(run),
            spawns: false,
            parent: Some(ParentLink { budget: p.budget.clone(), session_id: p.session_id.clone(), turn_id: p.turn_id.clone() }),
            parent_cancel: Some(p.cancel.clone()),
            announce: false,
            started: Instant::now(),
            handoff: None,
            here: Registration::new(self.clone()),
            rolling: None,
            tried: Vec::new(),
        };
        // Boxed: a subagent's turn is a turn of this host, inside the
        // parent's.
        let turn: BoxFuture<'_, Result<(RunResult, Option<Rolling>), EngineError>> = Box::pin(self.turn(plan, spawn.out.clone()));
        turn.await.map(|(r, _)| r)
    }

    /// Runs one settled turn to its end and logs it whole, with what
    /// follows from how it ended for a session's own turn: a switch that
    /// could not run going back (R-SWITCH-4), a limit's offer, or where
    /// `rollover = "auto"` goes next (R-INST-7, R-INST-8), and a
    /// `switchModel` that came while it ran.
    async fn turn(self: &Arc<Self>, mut plan: TurnPlan, out: mpsc::Sender<StreamLine>) -> Result<(RunResult, Option<Rolling>), EngineError> {
        let session_id = plan.log.session_id.clone();
        if plan.here.id.is_none() {
            plan.here.register(&session_id)?;
        }
        let turn_id = krowk_store::new_id();
        let engine = plan.engine.clone();
        let model = plan.model.clone();
        let mut w = Writer {
            log: &mut plan.log,
            out: &out,
            turn_id: turn_id.clone(),
            preset: plan.preset,
            wire: plan.wire,
            provider: plan.provider.clone(),
            backend: plan.past.backend.clone(),
            parent: plan.parent.clone(),
            handoff: None,
        };
        // A rollover is on the record only now that its turn has passed
        // every check, and every client is told: never silent (R-INST-8).
        if let Some(r) = &plan.rolling {
            let detail = r.detail();
            w.log(LogBody::ModelSwitched { turn_id: None, from: Some(r.from.clone()), to: model.clone(), reason: SwitchReason::RateLimited, detail: Some(detail.clone()) }).await?;
            w.live(LiveEvent::Notice { session_id: session_id.clone(), turn_id: turn_id.clone(), text: format!("{detail} (rollover = \"auto\")") }).await;
        }
        w.log(LogBody::TurnStarted { turn_id: turn_id.clone(), model: model.clone(), provider: engine.provider().into(), wire_api: engine.wire_api(), permission_mode: plan.permission_mode, effort: plan.effort }).await?;
        let prompt_item = Item::UserText { text: std::mem::take(&mut plan.text) };
        w.log(LogBody::ItemCompleted { turn_id: turn_id.clone(), item_id: krowk_store::new_id(), item: prompt_item.clone() }).await?;
        let mut history = std::mem::take(&mut plan.past.items);
        // `/name` for a skill, on krowk's own loop: its instructions come
        // right after the prompt. A vendor's agent expands its own.
        let asked = match &prompt_item {
            Item::UserText { text } if plan.spawns => compat::skills::invoked(&plan.compat.skills, text),
            _ => None,
        };
        history.push(HistoryItem { item: prompt_item, response: None });
        if let Some(text) = asked {
            let item = Item::UserText { text };
            w.log(LogBody::ItemCompleted { turn_id: turn_id.clone(), item_id: krowk_store::new_id(), item: item.clone() }).await?;
            history.push(HistoryItem { item, response: None });
        }

        let gate = permissions::Gate::new(
            plan.policy.clone(),
            plan.permission_mode,
            plan.grants.clone(),
            self.cfg.permissions.approvals.then(|| self.approvals.clone()),
            self.cfg.permissions.grants_file(),
            &session_id,
            &turn_id,
        );
        let compat = Arc::new(std::mem::take(&mut plan.compat));
        let (cancel_tx, cancel, steers) = (plan.here.cancel.clone(), plan.here.cancel_rx.clone(), plan.here.steers.clone());
        // The agent definitions a subagent can be started from, read afresh
        // each turn from the repository and the person's own.
        let subagents = plan.spawns.then(|| {
            let (defs, problems) = agents::discover(&trust::root(&plan.cwd), &self.cfg.agents.user_dirs);
            let spawn = Spawn {
                host: self.clone(),
                parent: ParentTurn {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    model: model.clone(),
                    provider: plan.provider.clone(),
                    cwd: plan.cwd.clone(),
                    permission_mode: plan.permission_mode,
                    budget: plan.budget.clone(),
                    evidence: plan.evidence.clone(),
                    gate: gate.clone(),
                    compat: compat.clone(),
                    grants: plan.grants.clone(),
                    cancel: cancel.clone(),
                },
                out: out.clone(),
                gate: Arc::new(Semaphore::new(self.cfg.registry.subagents.max_parallel())),
                defs,
                spent: std::sync::Mutex::new((0.0, false)),
            };
            (Subagents(Arc::new(spawn)), problems)
        });
        let subagents = match subagents {
            Some((s, problems)) => {
                for why in problems {
                    w.live(LiveEvent::Notice { session_id: session_id.clone(), turn_id: turn_id.clone(), text: format!("an agent definition was skipped — {why}") }).await;
                }
                Some(s)
            }
            None => None,
        };
        let budget = plan.budget.clone();
        let spawned = subagents.clone();
        let ctx = TurnContext {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            model: model.clone(),
            history,
            cwd: plan.cwd.clone(),
            permission_mode: plan.permission_mode,
            preset: plan.preset,
            effort: plan.effort,
            model_info: plan.info.clone(),
            cancel,
            steers: steers.clone(),
            backend_session: plan.backend_session.clone(),
            budget: budget.clone(),
            evidence: plan.evidence.clone(),
            gate,
            compat,
            subagents,
            agent: plan.agent.clone(),
            handoff: plan.handoff.take(),
        };
        let mut tally = Tally::default();
        // A backend's calls are the vendor's to make: its turn is not begun
        // when the session is already at its budget, and is interrupted when
        // a metered call takes it past (`Writer::handle`). The native loop
        // asks before each call itself.
        let watch = (!engine.checks_budget()).then(|| (budget.clone(), cancel_tx.clone()));
        let before = match &watch {
            Some((b, _)) => b.admit_turn().await,
            None => Ok(()),
        };
        // A subagent stops when its parent's turn is interrupted, the way
        // it stops for its own interrupt: keeping what it made.
        let parent_cancel = plan.parent_cancel.as_mut();
        let follow = async {
            if let Some(pc) = parent_cancel {
                crate::engine::cancelled(pc).await;
                let _ = cancel_tx.send(true);
            }
            std::future::pending::<()>().await
        };
        let outcome = match before {
            Ok(()) => tokio::select! {
                biased;
                o = w.drive(engine.as_ref(), ctx, &mut tally, &model, &budget, watch.as_ref()) => o,
                _ = follow => unreachable!("following the parent never ends"),
            },
            Err(e) => Err(e),
        };
        // A backend turn the budget interrupted failed on it, whatever the
        // vendor made of the interrupt.
        let outcome = match tally.tripped.take() {
            Some(e) => Err(e),
            None => outcome,
        };
        // Refused from here on, not queued for a turn that is over; what an
        // interrupted or failed turn never took goes back on its result.
        let unread_steers = steers.close();
        self.approvals.forget_session(&session_id);
        if let Some(b) = self.backends.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&session_id) {
            b.used = Instant::now();
        }

        let (status, mut error) = match &outcome {
            Ok(TurnEnd::Completed) => (TurnStatus::Completed, None),
            Ok(TurnEnd::Interrupted) => (TurnStatus::Interrupted, None),
            Err(e) => (TurnStatus::Failed, Some(e.info())),
        };
        let (after, offer, next) = match &outcome {
            Err(e) if plan.announce => self.after_failure(e, (plan.past.reached.as_ref(), &plan.tried, &plan.cwd), &model, &session_id, tally.calls, error.as_mut()).await,
            _ => (None, None, None),
        };
        let duration_ms = plan.started.elapsed().as_millis() as u64;
        // The turn's cost is its subagents' too, as a backend's own
        // subagents' are part of its turn (R-SUB-4).
        let (children_usd, children_unpriced) = spawned.as_ref().map_or((0.0, false), Subagents::spent);
        w.log(LogBody::TurnCompleted { turn_id: turn_id.clone(), status, usage: tally.usage, duration_ms, error: error.clone(), reported_cost_usd: tally.reported }).await?;
        if let Some((to, reason, detail)) = &after {
            w.log(LogBody::ModelSwitched { turn_id: Some(turn_id.clone()), from: Some(model.clone()), to: to.clone(), reason: *reason, detail: Some(detail.clone()) }).await?;
        }
        // Asked for while the turn ran: the person's word is the last. Taken
        // as the turn lets go of the session, so none comes too late for it.
        let mut next = next;
        if let Some(to) = plan.here.finish() {
            let from = after.as_ref().map(|(m, ..)| m.clone()).unwrap_or_else(|| model.clone());
            if from != to {
                w.log(LogBody::ModelSwitched { turn_id: Some(turn_id.clone()), from: Some(from), to, reason: SwitchReason::Requested, detail: None }).await?;
                next = None;
            }
        }
        plan.log.sync().map_err(log_failure)?;
        let result = RunResult {
            session_id,
            turn_id,
            status,
            is_error: status == TurnStatus::Failed,
            result: tally.last_text,
            model,
            usage: tally.usage,
            cost_usd: if tally.unpriced || children_unpriced { None } else { Some(tally.cost + children_usd) },
            duration_ms,
            num_model_calls: tally.calls,
            error,
            unread_steers,
            switch_offer: offer,
        };
        if plan.announce {
            let _ = out.send(StreamLine::Live(LiveEvent::Result(result.clone()))).await;
        }
        Ok((result, next))
    }

    /// What follows a session's own turn that failed: a switch that could
    /// not run goes back to the model of the last turn that reached its
    /// model, however the switch was made (R-SWITCH-4); a limit offers the
    /// next instance, or with `rollover = "auto"` names where to go next —
    /// logged when that turn starts (R-INST-7, R-INST-8). `error` gets the
    /// words that say so.
    #[allow(clippy::type_complexity)]
    async fn after_failure(&self, e: &EngineError, (reached, tried, cwd): (Option<&ModelRef>, &[String], &PathBuf), model: &ModelRef, session_id: &str, calls: u32, error: Option<&mut crate::protocol::ErrorInfo>) -> (Option<(ModelRef, SwitchReason, String)>, Option<SwitchOffer>, Option<Rolling>) {
        let previous = reached.filter(|p| *p != model).cloned();
        if e.limited() {
            let until = e.resets_at_ms.map(|ms| format!(" until {}", clock(ms))).unwrap_or_default();
            let mut tried = tried.to_vec();
            tried.push(model.instance.clone());
            let auto = self.cfg.registry.rollover == Rollover::Auto;
            return match self.next_instance(model, &tried, cwd, auto).await {
                Some(to) if auto => (None, None, Some(Rolling { from: model.clone(), to, until, cwd: cwd.clone() })),
                Some(to) => {
                    if let Some(err) = error {
                        err.message.push_str(&format!(" — continue on {to} with `krowk -p --resume {session_id} --model {to}`, or say yes when krowk offers it"));
                    }
                    (None, Some(SwitchOffer { from: model.clone(), to, resets_at_ms: e.resets_at_ms }), None)
                }
                None => (None, None, None),
            };
        }
        match previous {
            Some(prev) if cannot_run_here(&e.code, calls) => {
                if let Some(err) = error {
                    err.message.push_str(&format!(" — the session continues on {prev}"));
                }
                (Some((prev, SwitchReason::SwitchFailed, format!("{model} could not run the turn: {}", e.code))), None, None)
            }
            _ => (None, None, None),
        }
    }

    /// The vendor thread a backend turn on `instance` resumes, and how it
    /// is brought up to date (R-SWITCH-2, R-INST-4). The thread that has
    /// run the most of the session among this vendor's instances is the
    /// one to continue: this instance's own, resumed; another account's,
    /// its transcript copied into this one's config directory first; none,
    /// and a new thread is seeded with krowk's handoff.
    fn carry_over(&self, past: &Past, instance: &Resolved, b: &crate::instances::Backend, prompt: &str) -> (Option<String>, Option<handoff::Handoff>) {
        let vendor = match instance.wire_api {
            WireApi::CodexAppServer => crate::codex::BACKEND,
            _ => crate::claude::BACKEND,
        };
        let own = past.vendors.iter().find(|v| v.instance == instance.name);
        let best = past.vendors.iter().filter(|v| v.backend == vendor).max_by_key(|v| v.seen);
        let mut not_carried = None;
        let mut carried: Option<(&Vendor, PathBuf)> = None;
        if let Some(best) = best.filter(|v| v.instance != instance.name && own.is_none_or(|o| v.seen > o.seen)) {
            let from_home = self.cfg.registry.get(&best.instance).ok().and_then(|i| i.backend.as_ref()).and_then(|fb| fb.home.clone());
            let r = match (&best.transcript, from_home, &b.home) {
                (Some(t), Some(from), Some(to)) => handoff::carry(std::path::Path::new(t), &from, to, if vendor == crate::codex::BACKEND { handoff::Vendor::Codex } else { handoff::Vendor::ClaudeCode }, &best.session_id),
                (None, _, _) => Err(format!("{} did not say where it keeps the transcript", best.instance)),
                (_, None, _) => Err(format!("{}'s config directory is not known", best.instance)),
                (_, _, None) => Err(format!("{}'s config directory is not known", instance.name)),
            };
            match r {
                Ok(path) => carried = Some((best, path)),
                Err(e) => not_carried = Some(format!("the transcript on {} could not be carried over: {e}", best.instance)),
            }
        }
        let (resume, seen, from, path) = match (carried, own) {
            (Some((v, path)), _) => (Some(v.session_id.clone()), Some(v.seen), Some(v.instance.clone()), Some(path)),
            (None, Some(o)) => (Some(o.session_id.clone()), Some(o.seen), None, None),
            (None, None) => (None, None, None, None),
        };
        let plan = handoff::plan(&past.items, &past.turns, seen, prompt, from, not_carried).map(|h| handoff::Handoff { carried_to: path, ..h });
        (resume, plan)
    }

}

/// A move `rollover = "auto"` makes: from the limited model to the next
/// candidate, and why, in words that name both whole (R-INST-8).
#[derive(Debug, Clone)]
struct Rolling {
    from: ModelRef,
    to: ModelRef,
    /// ` until 14:00`, when the limit said.
    until: String,
    cwd: PathBuf,
}

impl Rolling {
    fn detail(&self) -> String {
        let mut d = format!("{} is limited{}; rollover is auto, so the session continues on {}", self.from, self.until, self.to);
        if self.to.model != self.from.model {
            d.push_str(&format!(" — another model: {} instead of {}", self.to.model, self.from.model));
        }
        d
    }
}

/// A turn's hold on its session in this host: its cancel switch, its
/// steering, and a `switchModel` for after it — from before the log is
/// opened until the turn lets go. Let go of on drop, however it ends.
struct Registration {
    shared: Arc<Shared>,
    id: Option<String>,
    cancel: Arc<watch::Sender<bool>>,
    cancel_rx: watch::Receiver<bool>,
    steers: Steers,
}

impl Registration {
    fn new(shared: Arc<Shared>) -> Registration {
        let (tx, rx) = watch::channel(false);
        Registration { shared, id: None, cancel: Arc::new(tx), cancel_rx: rx, steers: Steers::default() }
    }

    fn register(&mut self, id: &str) -> Result<(), EngineError> {
        let mut running = self.shared.running.lock().unwrap_or_else(|e| e.into_inner());
        if running.contains_key(id) {
            return Err(EngineError::new("session_busy", format!("session {id} has a turn running here already — wait for it, or steer it")));
        }
        running.insert(id.to_string(), Running { cancel: self.cancel.clone(), steers: self.steers.clone(), switch: None });
        self.id = Some(id.to_string());
        Ok(())
    }

    /// Lets go, and returns the switch that arrived for after the turn.
    fn finish(&mut self) -> Option<ModelRef> {
        let id = self.id.take()?;
        self.shared.running.lock().unwrap_or_else(|e| e.into_inner()).remove(&id).and_then(|r| r.switch)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Whether a turn's failure, on a model the session had just switched to,
/// says that model cannot run turns here at all — as against a failure any
/// turn can have. No login, no key, no access, no such model, a backend in
/// the wrong mode: never, however far it got (a vendor answers a missing
/// login with a message of its own). A backend that would not start or
/// answer, a history the provider refused: only when nothing reached the
/// model — after that, the switch worked and the turn failed.
fn cannot_run_here(code: &str, calls: u32) -> bool {
    match code {
        "not_authenticated" | "provider_auth" | "provider_forbidden" | "model_not_found" | "backend_not_found" | "backend_permission_mode" => true,
        "provider_invalid_request" | "backend_unresponsive" | "backend_exited" | "backend_failed" => calls == 0,
        _ => false,
    }
}

/// A moment as the person's clock shows it: `14:00` today, else with its
/// date — for when a limit lifts.
pub fn clock(ms: i64) -> String {
    #[cfg(unix)]
    {
        let at = (ms / 1000) as libc::time_t;
        let now = krowk_store::now_ms() / 1000;
        // SAFETY: localtime_r writes only into the struct it is handed.
        let tm = |t: libc::time_t| unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            tm
        };
        let (a, n) = (tm(at), tm(now as libc::time_t));
        if (a.tm_year, a.tm_yday) == (n.tm_year, n.tm_yday) {
            return format!("{:02}:{:02}", a.tm_hour, a.tm_min);
        }
        format!("{:04}-{:02}-{:02} {:02}:{:02}", a.tm_year + 1900, a.tm_mon + 1, a.tm_mday, a.tm_hour, a.tm_min)
    }
    #[cfg(not(unix))]
    {
        let s = ms / 1000;
        format!("{:02}:{:02} UTC", (s / 3600) % 24, (s / 60) % 60)
    }
}

/// The engine an instance runs a model on, over the wire API chosen for
/// it. A credential that is missing is refused here, before a session is
/// created for a turn that could not run.
fn engine_for(instance: &Resolved, wire: WireApi, credentials: &std::path::Path, krowk_version: &str) -> Result<Box<dyn Engine>, EngineError> {
    // A native instance's readiness needs no process: a key, or a login in
    // krowk's own file that is there and not expired past refreshing.
    if let Some(r) = readiness::local(instance, credentials)
        && let Some(e) = readiness::report(instance, r, credentials).refusal(instance)
    {
        return Err(e);
    }
    let credential = match &instance.auth {
        Auth::ApiKey => Credential::Key(instance.api_key.clone()),
        Auth::Keyless | Auth::Vendor => Credential::None,
        Auth::OAuth { .. } => Credential::OAuth(Arc::new(oauth::Tokens::open(oauth::Store::new(credentials.to_path_buf()), &instance.name)?)),
    };
    match wire {
        WireApi::AnthropicMessages => Ok(Box::new(NativeEngine { client: AnthropicClient::new(instance.clone(), krowk_version)? })),
        WireApi::OpenaiResponses => Ok(Box::new(NativeEngine { client: ResponsesClient::new(instance.clone(), krowk_version)? })),
        WireApi::ChatCompletions => Ok(Box::new(NativeEngine { client: ChatClient::new(instance.clone(), credential, krowk_version)? })),
        WireApi::ClaudeCode => Err(EngineError::new("bad_config", format!("{} runs Claude Code as a backend, not a native wire API", instance.name))),
        WireApi::CodexAppServer => Err(EngineError::new("bad_config", format!("{} runs Codex as a backend, not a native wire API", instance.name))),
    }
}

/// What the branch so far says: its items, grouped the way they were on the
/// wire, its turns, the model it is on, where it runs, and the vendor
/// threads behind it.
#[derive(Default)]
struct Past {
    items: Vec<HistoryItem>,
    turns: Vec<TurnSpan>,
    /// The last turn's model, or the one a later `model.switched` moved to.
    model: Option<ModelRef>,
    /// The model of the last turn that reached its model: where a switch
    /// that could not run goes back to.
    reached: Option<ModelRef>,
    cwd: Option<PathBuf>,
    /// The last `backend.session` logged, whichever instance it was on.
    backend: Option<BackendRecord>,
    /// Each backend instance's thread, and how much of the session it holds.
    vendors: Vec<Vendor>,
    /// The krowk run its evidence goes under, once `publish` opened one.
    run: Option<String>,
    /// For a subagent: the session that started it.
    parent: Option<String>,
}

/// The last `backend.session` of a branch, and the instance it ran on.
#[derive(Debug, Clone, PartialEq)]
struct BackendRecord {
    instance: String,
    session_id: String,
    transcript: Option<String>,
    billing: Option<Billing>,
}

/// A backend instance's thread of this session: the vendor session it
/// resumes, where the vendor keeps it, and how many of the branch's turns it
/// holds — every turn up to the last one it ran, since each turn it ran
/// began by bringing it up to date.
#[derive(Debug, Clone, PartialEq)]
struct Vendor {
    instance: String,
    backend: String,
    session_id: String,
    transcript: Option<String>,
    seen: usize,
}

fn replay(branch: &[&LogEvent]) -> Past {
    let mut past = Past::default();
    let mut at: HashMap<&str, usize> = HashMap::new();
    let mut responses = 0usize;
    // Whether the running turn reached its backend: a thread that never
    // got the prompt has not seen it.
    let mut reached = false;
    let mut answered = false;
    for ev in branch {
        match &ev.body {
            LogBody::SessionStarted { cwd, parent_session_id, .. } => {
                past.cwd = Some(PathBuf::from(cwd));
                past.parent = parent_session_id.clone();
            }
            LogBody::TurnStarted { model, .. } => {
                past.model = Some(model.clone());
                past.turns.push(TurnSpan { model: model.clone(), items: past.items.len()..past.items.len() });
                reached = false;
                answered = false;
            }
            LogBody::BackendSession { backend, vendor_session_id, transcript_path, billing, .. } => {
                let instance = past.turns.last().map(|t| t.model.instance.clone()).unwrap_or_default();
                past.backend = Some(BackendRecord { instance: instance.clone(), session_id: vendor_session_id.clone(), transcript: transcript_path.clone(), billing: *billing });
                past.vendors.retain(|v| v.instance != instance);
                past.vendors.push(Vendor { instance, backend: backend.clone(), session_id: vendor_session_id.clone(), transcript: transcript_path.clone(), seen: past.turns.len().saturating_sub(1) });
                reached = true;
            }
            LogBody::ItemCompleted { item_id, item, .. } => {
                at.insert(item_id, past.items.len());
                past.items.push(HistoryItem { item: item.clone(), response: None });
                if let Some(t) = past.turns.last_mut() {
                    t.items.end = past.items.len();
                }
            }
            LogBody::ResponseCompleted { item_ids, .. } => {
                for id in item_ids {
                    if let Some(&i) = at.get(id.as_str()) {
                        past.items[i].response = Some(responses);
                    }
                }
                responses += 1;
                reached = true;
                answered = true;
            }
            LogBody::TurnCompleted { status, error, .. } => {
                // It reached its model when a model answered, and did not
                // then fail on one of the ways a model cannot run here.
                let ran = match status {
                    TurnStatus::Failed => answered && !error.as_ref().is_some_and(|e| cannot_run_here(&e.code, 1)),
                    _ => true,
                };
                if ran {
                    past.reached = past.turns.last().map(|t| t.model.clone());
                }
                let instance = past.turns.last().map(|t| t.model.instance.as_str()).unwrap_or_default();
                if reached && let Some(v) = past.vendors.iter_mut().find(|v| v.instance == instance) {
                    v.seen = past.turns.len();
                }
            }
            LogBody::ModelSwitched { to, .. } => past.model = Some(to.clone()),
            LogBody::RunOpened { run, .. } => past.run = Some(run.clone()),
            // The subagents' own logs hold their conversations; the todo
            // list is read back from the calls that set it.
            LogBody::SubagentResponse { .. } | LogBody::BackendHandoff { .. } | LogBody::SubagentStarted { .. } | LogBody::TodosUpdated { .. } => {}
        }
    }
    past
}

/// A turn's running totals.
#[derive(Default)]
struct Tally {
    usage: Usage,
    cost: f64,
    unpriced: bool,
    calls: u32,
    last_text: String,
    /// Why the host interrupted a backend's turn: the budget it went past.
    tripped: Option<EngineError>,
    /// What the backend said the turn cost.
    reported: Option<f64>,
}

/// Appends and forwards, in that order.
struct Writer<'a> {
    log: &'a mut SessionLog,
    out: &'a mpsc::Sender<StreamLine>,
    turn_id: String,
    preset: &'static toolset::Preset,
    wire: WireApi,
    /// The provider the turn's calls go to, for its context record.
    provider: String,
    /// The backend session last logged: one that did not change is not
    /// logged again.
    backend: Option<BackendRecord>,
    /// A subagent's parent turn, whose spend the subagent's calls add to.
    parent: Option<ParentLink>,
    /// What a backend was sent to bring it up to date, for the turn's
    /// context record.
    handoff: Option<String>,
}

impl Writer<'_> {
    async fn log(&mut self, body: LogBody) -> Result<LogEvent, EngineError> {
        let ev = self.log.append(body).map_err(log_failure)?;
        let _ = self.out.send(StreamLine::Log(ev.clone())).await;
        Ok(ev)
    }

    async fn live(&self, ev: LiveEvent) {
        let _ = self.out.send(StreamLine::Live(ev)).await;
    }

    /// Where a metered call left the turn and the session: the result's
    /// cost, and R-BUDGET-2's frame for the status bar.
    async fn spent(&self, session_id: &str, turn_id: String, tally: &mut Tally, spent: &crate::budget::Snapshot) {
        tally.cost = spent.turn.known_usd;
        tally.unpriced = !spent.turn.unpriced.is_empty();
        self.live(LiveEvent::Cost {
            session_id: session_id.into(),
            turn_id,
            cost_usd: spent.total.cost(),
            turn_cost_usd: spent.turn.cost(),
            generated_tokens: spent.total.generated(),
        })
        .await;
        // A subagent spends during its parent's turn: the parent's figure,
        // its whole tree counted again, moves with it (R-SUB-4).
        if let Some(p) = &self.parent {
            let tree = p.budget.refreshed().await;
            self.live(LiveEvent::Cost {
                session_id: p.session_id.clone(),
                turn_id: p.turn_id.clone(),
                cost_usd: tree.total.cost(),
                turn_cost_usd: tree.turn.cost(),
                generated_tokens: tree.total.generated(),
            })
            .await;
        }
    }

    /// Runs the engine and handles its events as they come. A log that
    /// cannot be written stops the turn: the log is the session.
    async fn drive(&mut self, engine: &dyn Engine, ctx: TurnContext, tally: &mut Tally, model: &ModelRef, budget: &Budget, watch: Option<&(Budget, Arc<watch::Sender<bool>>)>) -> Result<TurnEnd, EngineError> {
        let (tx, mut rx) = mpsc::channel(256);
        let session_id = ctx.session_id.clone();
        let run = engine.run_turn(ctx, tx);
        tokio::pin!(run);
        let mut outcome: Option<Result<TurnEnd, EngineError>> = None;
        let mut texts: HashMap<String, String> = HashMap::new();
        loop {
            tokio::select! {
                biased;
                Some(ev) = rx.recv() => {
                    self.handle(ev, &session_id, tally, &mut texts, model, budget).await?;
                    // A backend past its budget is stopped the way a person
                    // stops it, keeping what it made.
                    if let Some((b, cancel)) = watch
                        && tally.tripped.is_none()
                        && let Some(e) = b.over()
                    {
                        tally.tripped = Some(e);
                        let _ = cancel.send(true);
                    }
                }
                r = &mut run, if outcome.is_none() => outcome = Some(r),
                else => break,
            }
        }
        outcome.expect("the loop ends only after the engine does")
    }

    async fn handle(&mut self, ev: EngineEvent, session_id: &str, tally: &mut Tally, texts: &mut HashMap<String, String>, model: &ModelRef, budget: &Budget) -> Result<(), EngineError> {
        let turn_id = self.turn_id.clone();
        match ev {
            EngineEvent::Context { system, tools } => {
                let rec = ContextRecord {
                    turn_id,
                    time_ms: krowk_store::now_ms(),
                    model: model.clone(),
                    provider: self.provider.clone(),
                    wire_api: self.wire,
                    // A backend brings its own tools; the preset is krowk's.
                    toolset: match self.wire {
                        WireApi::ClaudeCode => crate::claude::BACKEND.into(),
                        WireApi::CodexAppServer => crate::codex::BACKEND.into(),
                        _ => self.preset.name.into(),
                    },
                    system_tokens: native::estimate_tokens(&system),
                    tools_tokens: native::tools_tokens(&tools),
                    system,
                    tools,
                    handoff: self.handoff.take(),
                };
                self.log.record_context(&rec).map_err(log_failure)?;
            }
            EngineEvent::ItemStarted { item_id, kind } => {
                self.live(LiveEvent::ItemStarted { session_id: session_id.into(), turn_id, item_id, item: kind }).await;
            }
            EngineEvent::ItemDelta { item_id, delta } => {
                self.live(LiveEvent::ItemDelta { session_id: session_id.into(), turn_id, item_id, delta }).await;
            }
            EngineEvent::ItemCompleted { item_id, item } => {
                if let Item::AssistantText { text } = &item {
                    texts.insert(item_id.clone(), text.clone());
                }
                self.log(LogBody::ItemCompleted { turn_id, item_id, item }).await?;
            }
            EngineEvent::ResponseCompleted { response_id, model: answered, usage, stop_reason, item_ids } => {
                tally.calls += 1;
                tally.usage += usage;
                // Priced by the id the request named; the answering model's
                // id is the fallback, since a provider may name a snapshot.
                let spent = budget.record(&answered, &usage);
                let said: Vec<&str> = item_ids.iter().filter_map(|id| texts.get(id).map(String::as_str)).collect();
                if !said.is_empty() {
                    tally.last_text = said.join("\n\n");
                }
                self.log(LogBody::ResponseCompleted { turn_id: turn_id.clone(), response_id, model: answered, usage, stop_reason, item_ids }).await?;
                self.spent(session_id, turn_id, tally, &spent).await;
            }
            EngineEvent::BackendSession { backend, session_id: vendor, transcript, billing } => {
                let rec = BackendRecord { instance: model.instance.clone(), session_id: vendor.clone(), transcript: transcript.clone(), billing };
                if self.backend.as_ref() != Some(&rec) {
                    self.log(LogBody::BackendSession { turn_id, backend, vendor_session_id: vendor, transcript_path: transcript, billing }).await?;
                    self.backend = Some(rec);
                }
            }
            EngineEvent::RunOpened { run } => {
                self.log(LogBody::RunOpened { turn_id, run }).await?;
            }
            // Metered, logged apart from the conversation, and counted.
            EngineEvent::SubagentResponse { response_id, model: answered, usage } => {
                tally.usage += usage;
                let spent = budget.record_subagent(&answered, &usage);
                self.log(LogBody::SubagentResponse { turn_id: turn_id.clone(), response_id, model: answered, usage }).await?;
                self.spent(session_id, turn_id, tally, &spent).await;
            }
            EngineEvent::ReportedCost { usd } => {
                tally.reported = Some(usd);
                let spent = budget.reported(usd);
                self.spent(session_id, turn_id, tally, &spent).await;
            }
            EngineEvent::Notice { text } => {
                self.live(LiveEvent::Notice { session_id: session_id.into(), turn_id, text }).await;
            }
            EngineEvent::Approval(req) => self.live(LiveEvent::ApprovalRequested(req)).await,
            EngineEvent::ApprovalResolved { request_id, decision } => {
                self.live(LiveEvent::ApprovalResolved { session_id: session_id.into(), turn_id, request_id, decision }).await;
            }
            EngineEvent::Todos { todos } => {
                self.log(LogBody::TodosUpdated { turn_id, todos }).await?;
            }
            EngineEvent::SubagentStarted { call_id, session_id: child, description, agent, model } => {
                self.log(LogBody::SubagentStarted { turn_id, call_id, subagent_session_id: child, description, agent, model }).await?;
            }
            EngineEvent::Limits(limit) => {
                self.live(LiveEvent::Limits { session_id: session_id.into(), turn_id, instance: model.instance.clone(), limit }).await;
            }
            EngineEvent::Handoff { how, from_instance, summarized_turns, recent_turns, fell_back, text } => {
                self.log(LogBody::BackendHandoff { turn_id, how, from_instance, summarized_turns, recent_turns, fell_back }).await?;
                if !text.is_empty() {
                    self.handoff = Some(text);
                }
            }
        }
        Ok(())
    }
}
