//! The Claude Code backend (R-BACK-1): krowk drives the user's own,
//! unmodified `claude` binary — the one compliant way to run a turn on a
//! Claude subscription rather than API tokens — as one long-lived process
//! per session:
//!
//! ```text
//! claude -p --input-format stream-json --output-format stream-json --verbose
//!        --include-partial-messages --permission-prompt-tool stdio
//!        --mcp-config '{"mcpServers":{"krowk":{"type":"sdk","name":"krowk"}}}' --strict-mcp-config
//!        --model <model> --permission-mode default|plan
//!        [--resume <claude session>] [--effort <level>]
//! ```
//!
//! Each prompt is one `user` line on the process's stdin, and its turn is
//! everything up to the `result` line; the stream in between is translated
//! into krowk's events by `stream`, so a backend turn lands in the same log
//! in the same shape as a native one (R-BACK-5). The process stays up
//! between turns of the session, kept by the host; a new krowk process
//! starts a new one on `--resume` with the Claude session id the log holds.
//!
//! **The control protocol** rides the same pipes, as `control_request` /
//! `control_response` lines. It is not publicly documented; the reference is
//! Anthropic's open-source `claude-agent-sdk-python` (`_internal/query.py`),
//! and what this build speaks was checked against `claude` 2.1.280:
//!
//! | subtype | direction | what krowk does |
//! |---|---|---|
//! | `initialize` | krowk → claude | first, on every process; its answer lists the `models`, which is how in-place model switching is detected |
//! | `can_use_tool` | claude → krowk | judged by krowk's permission evaluator (`crate::permissions`), asking the person when a client is attached |
//! | `mcp_message` | claude → krowk | a JSON-RPC message for the `krowk` MCP server, answered by `crate::bridge` |
//! | `interrupt` | krowk → claude | a `Command::Interrupt`; the turn ends at the next `result` and the process lives on |
//! | `set_model` | krowk → claude | a turn on another model of the same instance, when `initialize` listed models; otherwise a new process on `--resume` |
//!
//! **Between turns** the process is not silent: an agent the model started
//! with Claude Code's `Agent` tool (`run_in_background`) runs on, asks
//! `can_use_tool`, and when it finishes Claude Code begins a turn by itself
//! to answer it — a `system`/`init` with no prompt, whose `result` carries
//! `origin`. One task reads stdout for the life of the process, and between
//! turns an idle loop holds it: it answers what Claude Code asks under the
//! last turn's permissions (nobody is there to be asked), keeps the list of
//! agents (`system` `task_started`, `task_updated`, `task_notification`,
//! `background_tasks_changed`), meters their calls, and stops at the first
//! line of a turn Claude Code began, which then waits — `pending`, and
//! `turn.unprompted` to the host's watchers — until the host runs it as a
//! turn of the session (`Command::Continue`). A prompt never ends at such a
//! turn's `result`: the host runs a waiting one first, and one that begins
//! after the prompt was sent is folded into the prompt's turn.
//!
//! Features are detected, never assumed from a version number: the
//! capabilities `system`/`init` announces (`interrupt_receipt_v1` — an
//! interrupt is acknowledged, so an unacknowledged one is given up on
//! sooner) and the `initialize` answer.
//!
//! **What krowk is asked, and what it is not.** Claude Code decides some
//! tool calls itself before it asks anyone: an allow rule in the user's or
//! the project's settings (`permissions.allow`), a `PreToolUse` hook that
//! approves, an agent's own `permissionMode`, and the read-only calls its
//! mode never asks about. Those run without krowk's `can_use_tool` — that
//! is Claude Code behaving as its user configured it, from the same
//! `.claude/settings.json` files krowk's own rules read. Every deny rule
//! krowk holds is handed to it as `--disallowedTools`, so what Claude Code
//! would allow by itself still meets them: deny wins here too. What krowk
//! also holds is the mode: the process is started `--permission-mode default` (or `plan`),
//! so a `defaultMode` in settings cannot loosen it, and a turn whose
//! `system`/`init` reports a mode looser than krowk's is stopped before it
//! runs anything. Every call Claude Code does ask about is judged by krowk's
//! permission evaluator (`crate::permissions`) under Claude Code's own tool
//! names, and asked of the person when a client is attached.
//!
//! **Compliance** (R-BACK-2) is structural: krowk starts the binary as the
//! user installed it, sends it no system prompt and no headers, sets only
//! `CLAUDE_CONFIG_DIR` and the instance's own environment, and never opens
//! Claude's credentials — the login is Claude Code's, made with `claude auth
//! login` and reported by `claude auth status` (`auth`). A compliance test
//! holds every source file and every run to that.

pub mod auth;
pub mod stream;

use crate::bridge::{self, BridgeEnv};
use crate::engine::{cancelled, BoxFuture, Engine, EngineError, EngineEvent, Events, Idle, Pending, TurnContext, TurnEnd};
use crate::instances::{Backend, Resolved};
use crate::permissions::{self, Access, Call, Gate, Verdict};
use crate::protocol::{BackendAgent, Billing, Effort, Item, ItemKind, ModelRef, PermissionMode, ToolDefinition, WireApi};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stream::{Init, Meter, Translator};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};

/// The binary a `claude-code` instance runs when its definition names none.
pub const BINARY: &str = "claude";
/// The backend's name in the log.
pub const BACKEND: &str = "claude-code";

/// How long the process has to answer `initialize`: long enough for a cold
/// start with plugins and hooks, short enough that a binary that is not
/// Claude Code is named rather than waited on.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// After an interrupt, how long the turn waits for Claude Code's `result`
/// before the process is stopped; the session continues on `--resume`.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);
/// With `interrupt_receipt_v1`, how long an interrupt may go unacknowledged.
const RECEIPT_GRACE: Duration = Duration::from_secs(5);
/// How long a process that was asked to exit (stdin closed) gets.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// The end of stderr kept for a failure's words.
const STDERR_TAIL: usize = 4096;

/// What a turn's context record says of the system prompt: it is Claude
/// Code's, and krowk neither sees nor changes it.
pub const SYSTEM_NOTE: &str = "(Claude Code's own system prompt: krowk sends none and does not see it)";

/// What the prompt item of a turn Claude Code began by itself starts
/// with: clients show it as krowk's note, not the person's words.
pub const UNPROMPTED: &str = "<unprompted>";

/// The prompt item of a turn Claude Code began by itself: why it began.
pub fn unprompted(reason: &str) -> String {
    format!("{UNPROMPTED}Claude Code began this turn by itself: {reason}. krowk sent no prompt.</unprompted>")
}

/// A turn Claude Code began by itself that ended inside a prompted one: the
/// note the log holds where it ended, before the prompt's own answer.
pub fn folded(reason: &str) -> String {
    format!("{UNPROMPTED}What came before this was a turn Claude Code began by itself: {reason}. The prompt's own answer follows.</unprompted>")
}

/// Why a turn began by itself, when no agent's end says so.
const BEGAN: &str = "it did not say why";

/// The launch settings a process is bound to. A turn that needs others —
/// plan mode, another effort, or another model where `set_model` is not
/// available — gets a new process on `--resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub model: String,
    pub resume: Option<String>,
    pub effort: Option<&'static str>,
    pub plan: bool,
    /// Every deny rule that applies, in Claude Code's spelling, as
    /// `--disallowedTools`: what Claude Code would allow by itself still
    /// meets them, so deny wins on this backend too.
    pub disallowed: Vec<String>,
}

/// The `--mcp-config` that injects krowk's tools: one in-process (`sdk`)
/// server, answered over the control protocol.
pub fn mcp_config() -> String {
    json!({"mcpServers": {bridge::SERVER: {"type": "sdk", "name": bridge::SERVER}}}).to_string()
}

/// The `--settings` every process starts with: no `Co-Authored-By: Claude`
/// trailer on commits, no Claude Code line on pull requests. `attribution`
/// is the current key, `includeCoAuthoredBy` the one older versions read;
/// an instance's own `--settings` in its `args` comes later and wins.
pub const SETTINGS: &str = r#"{"attribution":{"commit":"","pr":""},"includeCoAuthoredBy":false}"#;

/// The whole argument list, krowk's own first and the instance's after.
pub fn args(l: &Launch, extra: &[String]) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--mcp-config",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(mcp_config());
    a.push("--strict-mcp-config".into());
    a.extend(["--settings".into(), SETTINGS.into()]);
    a.extend(["--model".into(), l.model.clone()]);
    if let Some(r) = &l.resume {
        a.extend(["--resume".into(), r.clone()]);
    }
    if let Some(e) = l.effort {
        a.extend(["--effort".into(), e.into()]);
    }
    // The mode is always named: left out, a `defaultMode` in the user's or
    // the project's settings (acceptEdits, bypassPermissions) would decide
    // it. Plan changes what Claude Code does, not only what it may do, so it
    // is Claude Code's to know; every looser krowk mode stays krowk's, and
    // what Claude Code asks is judged by krowk's permission evaluator.
    if !l.disallowed.is_empty() {
        a.push("--disallowedTools".into());
        a.extend(l.disallowed.iter().cloned());
    }
    a.extend(["--permission-mode".into(), if l.plan { "plan" } else { "default" }.into()]);
    a.extend(extra.iter().cloned());
    a
}

/// krowk's deny rules, as Claude Code is started with them: none under
/// unhinged, which no rule of krowk's holds. The deny rules of Claude Code's
/// own settings files Claude Code applies itself, before it asks krowk.
fn disallowed(gate: &Gate) -> Vec<String> {
    if gate.mode() == PermissionMode::Unhinged { Vec::new() } else { gate.policy().deny_list() }
}

/// krowk's ladder onto Claude Code's `--effort` levels (low … max): below
/// low is low, since Claude Code has no switch for thinking off.
pub fn effort_level(e: Effort) -> &'static str {
    match e {
        Effort::None | Effort::Minimal | Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}

/// Where Claude Code keeps a session's transcript: `projects/<cwd with
/// every character but ASCII letters and digits made a dash>/<id>.jsonl` in
/// its config directory — else wherever a transcript of that id already is
/// (a long path is shortened in newer versions).
pub fn transcript_path(config_dir: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let projects = config_dir.join("projects");
    let slug: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let file = format!("{session_id}.jsonl");
    let direct = projects.join(&slug).join(&file);
    if direct.exists() {
        return direct;
    }
    std::fs::read_dir(&projects)
        .ok()
        .and_then(|dirs| dirs.flatten().map(|d| d.path().join(&file)).find(|p| p.is_file()))
        .unwrap_or(direct)
}

/// A tool call Claude Code asks about, as krowk's permission evaluator
/// judges it (`crate::permissions`): Claude Code's own tool names are the
/// names rules are written in. Calls Claude Code allows itself — its
/// settings' allow rules, its hooks — never reach here (see the module's
/// notes); what does is judged like a native call. krowk's own bridged
/// tools need no permission; a tool this build does not know is asked
/// about.
pub fn call_of(tool: &str, input: &Value, cwd: &Path) -> Call {
    let at = |p: &str| {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
    };
    let path = ["file_path", "notebook_path", "path"].iter().find_map(|k| input.get(*k).and_then(Value::as_str)).map(at);
    let access = if tool.starts_with(&format!("mcp__{}__", bridge::SERVER)) {
        Access::Free
    } else if let Some(rest) = tool.strip_prefix("mcp__") {
        let (server, t) = rest.split_once("__").unwrap_or((rest, ""));
        Access::Mcp { server: server.into(), tool: t.into() }
    } else {
        match tool {
            "Read" | "NotebookRead" => Access::Read(vec![path.unwrap_or_else(|| cwd.to_path_buf())]),
            "Grep" | "Glob" | "LS" => {
                // A glob can name where it searches as much as a path can:
                // an absolute `pattern` (Glob) or `glob` (Grep) is judged by
                // its fixed part.
                let key = if tool == "Glob" { "pattern" } else { "glob" };
                let globbed = input.get(key).and_then(Value::as_str).filter(|g| Path::new(g).is_absolute()).map(|g| PathBuf::from(glob_root(g)));
                let mut ps = vec![path.unwrap_or_else(|| cwd.to_path_buf())];
                ps.extend(globbed);
                Access::Read(ps)
            }
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Access::Edit(path.into_iter().collect()),
            "Bash" => Access::Bash(input.get("command").and_then(Value::as_str).unwrap_or_default().to_string()),
            "Skill" => Access::Skill(None),
            "WebFetch" => Access::Fetch(input.get("url").and_then(Value::as_str).unwrap_or_default().to_string()),
            "TodoWrite" | "ToolSearch" | "Task" | "Agent" | "EnterPlanMode" | "ExitPlanMode" | "BashOutput" | "KillShell" | "KillBash" => Access::Free,
            _ => Access::Other,
        }
    };
    Call { tool: tool.to_string(), access, subject: input.get("subagent_type").or_else(|| input.get("skill")).and_then(Value::as_str).map(String::from) }
}

/// The fixed part of an absolute glob: the directories before the first
/// component that holds a wildcard.
fn glob_root(g: &str) -> String {
    let mut root = PathBuf::new();
    for c in Path::new(g).components() {
        if c.as_os_str().to_string_lossy().contains(['*', '?', '[', '{']) {
            break;
        }
        root.push(c);
    }
    root.display().to_string()
}

/// How loose a Claude Code permission mode is, against krowk's: plan asks
/// before everything, default asks before edits and commands, acceptEdits
/// only before commands; auto, bypassPermissions and any mode this build
/// does not know ask before nothing krowk can count on.
fn looser_than(claude: &str, krowk: PermissionMode) -> bool {
    let rank = match claude {
        "plan" => 0,
        "default" | "manual" | "dontAsk" => 1,
        "acceptEdits" => 2,
        _ => 3,
    };
    let allowed = match krowk {
        PermissionMode::Plan => 0,
        PermissionMode::Default => 1,
        PermissionMode::AcceptEdits => 2,
        PermissionMode::BypassPermissions | PermissionMode::Unhinged => 3,
    };
    rank > allowed
}

/// The ambient variables a Claude Code process does not inherit unless its
/// instance sets them: they are the native `anthropic` instance's key and
/// base URL, and inherited they would move a subscription account onto an
/// API key or another server without anyone asking for it. An instance
/// sets a base URL in its `env` and a key through `apiKeyEnv`, which krowk
/// reads and hands the process itself.
pub const NOT_INHERITED: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"];

/// What of `NOT_INHERITED` to remove for this instance.
pub fn cleared(b: &Backend) -> impl Iterator<Item = &'static str> + '_ {
    NOT_INHERITED.iter().copied().filter(|k| !b.env.contains_key(*k))
}

/// The environment a Claude Code command gets on top of krowk's own: the
/// inherited key and base URL taken away, telemetry off, the config
/// directory, the instance's `env`, and its key under the name Claude Code
/// reads.
///
/// Telemetry off because Claude Code flushes it on exit: ~0.7 s that every
/// quit of the TUI would wait on, against ~30 ms without. An instance that
/// wants it sets `DISABLE_TELEMETRY` in its `env`, which comes after.
pub fn environment(b: &Backend) -> (Vec<&'static str>, Vec<(String, String)>) {
    let mut set: Vec<(String, String)> = vec![("DISABLE_TELEMETRY".into(), "1".into())];
    if let Some(dir) = &b.config_dir {
        set.push(("CLAUDE_CONFIG_DIR".into(), dir.display().to_string()));
    }
    set.extend(b.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    if let Some((to, key)) = &b.key {
        set.push((to.clone(), key.clone()));
    }
    (cleared(b).collect(), set)
}

/// The engine for one session on one `claude-code` instance. The host keeps
/// it between turns, so the process it starts serves the whole session.
pub struct ClaudeEngine {
    instance: Resolved,
    krowk_version: String,
    /// The process: a turn's while one runs, the idle loop's between turns.
    proc: Arc<tokio::sync::Mutex<Option<Proc>>>,
    /// The idle loop reading the process between turns, and its stop.
    idle: Mutex<Option<Idling>>,
    /// Its agents and a turn it began, readable without the process's lock.
    status: Arc<Status>,
    /// Where between-turn news goes; none, and nobody is told.
    tell: Option<Idle>,
}

struct Idling {
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// What the process is doing that no turn is: the agents it runs, and the
/// turn it began by itself, waiting. And its subagents' calls, metered: the
/// engine's rather than one process's, so a process replaced — another
/// launch, a crash — hands what it had not reported yet to the next.
#[derive(Default)]
struct Status {
    agents: Mutex<Vec<Tracked>>,
    pending: Mutex<Option<Pending>>,
    meter: Mutex<Meter>,
}

#[derive(Debug, Clone)]
struct Tracked {
    agent: BackendAgent,
    /// Run in the background: it outlives the turn that started it.
    background: bool,
}

impl Status {
    fn agents(&self) -> std::sync::MutexGuard<'_, Vec<Tracked>> {
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn meter(&self) -> std::sync::MutexGuard<'_, Meter> {
        self.meter.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The process is gone, and all of it with it: the list, when it had
    /// any, to report empty.
    fn clear(&self) -> Option<Vec<BackendAgent>> {
        *self.pending() = None;
        let mut a = self.agents();
        (!a.is_empty()).then(|| {
            a.clear();
            Vec::new()
        })
    }
}

impl ClaudeEngine {
    pub fn new(instance: Resolved, krowk_version: &str) -> Result<ClaudeEngine, EngineError> {
        if instance.backend.is_none() {
            return Err(EngineError::new("bad_config", format!("{} is not a Claude Code instance", instance.name)));
        }
        Ok(ClaudeEngine { instance, krowk_version: krowk_version.into(), proc: Arc::new(tokio::sync::Mutex::new(None)), idle: Mutex::new(None), status: Arc::default(), tell: None })
    }

    /// Reports what happens between turns to `tell`.
    pub fn watched(mut self, tell: Idle) -> ClaudeEngine {
        self.tell = Some(tell);
        self
    }

    fn backend(&self) -> &Backend {
        self.instance.backend.as_ref().expect("checked in new")
    }

    /// Stops the idle loop and waits for it to let go of the process.
    async fn wake(&self) {
        let idling = self.idle.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(i) = idling {
            let _ = i.stop.send(());
            let _ = i.task.await;
        }
    }

    /// Hands the process to the idle loop until the next turn.
    fn rest(&self) {
        let (stop, rx) = oneshot::channel();
        let task = tokio::spawn(idle(self.proc.clone(), rx, self.tell.clone()));
        *self.idle.lock().unwrap_or_else(|e| e.into_inner()) = Some(Idling { stop, task });
    }
}

impl Drop for ClaudeEngine {
    /// The idle loop holds the process too: it goes, so the process is let
    /// go with the engine's last handle — and the agents it ran, and a turn
    /// it began, with it: said, so no client goes on showing them.
    fn drop(&mut self) {
        if let Some(i) = self.idle.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            i.task.abort();
        }
        let waiting = self.status.pending().is_some();
        let stopped = self.status.clear();
        if let Some(tell) = &self.tell {
            if let Some(agents) = stopped {
                tell(EngineEvent::BackendAgents { agents });
                tell(EngineEvent::Notice { text: format!("Claude Code on {} was let go, and the background agents it ran stopped with it", self.instance.name) });
            } else if waiting {
                tell(EngineEvent::Notice { text: format!("Claude Code on {} was let go before the turn it began by itself ran", self.instance.name) });
            }
        }
    }
}

impl Engine for ClaudeEngine {
    fn provider(&self) -> &str {
        &self.instance.provider
    }

    fn wire_api(&self) -> WireApi {
        WireApi::ClaudeCode
    }

    fn run_turn<'a>(&'a self, mut ctx: TurnContext, events: Events) -> BoxFuture<'a, Result<TurnEnd, EngineError>> {
        Box::pin(async move {
            self.wake().await;
            let mut slot = self.proc.lock().await;
            let want = Launch {
                model: ctx.model.model.clone(),
                resume: ctx.backend_session.clone(),
                effort: ctx.effort.map(effort_level),
                plan: ctx.permission_mode == PermissionMode::Plan,
                disallowed: disallowed(&ctx.gate),
            };
            let ask = Answers {
                session_id: ctx.session_id.clone(),
                turn_id: ctx.turn_id.clone(),
                model: ctx.model.clone(),
                cwd: ctx.cwd.clone(),
                mode: ctx.permission_mode,
                krowk_version: self.krowk_version.clone(),
                gate: ctx.gate.protecting(self.backend().home.iter().cloned()),
                evidence: ctx.evidence.clone(),
            };
            let prompt = match ctx.history.last().map(|h| &h.item) {
                Some(Item::UserText { text }) => text.clone(),
                _ => return Err(EngineError::new("empty_prompt", "a backend turn needs the prompt as its last item")),
            };
            // A turn Claude Code began runs where it began, whatever the
            // launch: a new process would not have it.
            if ctx.unprompted {
                let Some(p) = slot.as_mut().filter(|p| !p.begun.is_empty()) else {
                    return Err(EngineError::new("nothing_pending", format!("Claude Code on {} has no turn of its own waiting", self.instance.name)));
                };
                let outcome = p.turn(None, &mut ctx, &ask, &events, self.backend(), &self.instance.name).await;
                return self.settle(slot, outcome, ask, &events).await;
            }
            // The running process serves this turn when its launch still
            // fits; a model it can switch to in place is switched to.
            let mut resumed = false;
            if let Some(p) = slot.as_mut() {
                // The mode it is in counts too: a plan turn that approved
                // ExitPlanMode leaves Claude Code in default, which the next
                // plan turn must not inherit (nor a default turn an
                // EnterPlanMode's plan). It is launched in one of two modes,
                // and serves only a turn that wants the one it is in.
                let mode_fits = p.mode.is_empty() || (want.plan == (p.mode == "plan") && !looser_than(&p.mode, ctx.permission_mode));
                let fits = p.alive() && mode_fits && p.launch.plan == want.plan && p.launch.effort == want.effort && p.launch.disallowed == want.disallowed;
                let switched = fits && (p.launch.model == want.model || (p.set_model && p.set_model(&want.model, &ask).await));
                if !switched && let Some(old) = slot.take() {
                    // Its agents, and a turn it began, go with it; the
                    // thread keeps what they did.
                    if let Some(agents) = self.status.clear() {
                        let _ = events.send(EngineEvent::BackendAgents { agents }).await;
                        let _ = events.send(EngineEvent::Notice { text: "Claude Code was started again for this turn's settings, and the background agents it ran stopped".into() }).await;
                    }
                    old.shutdown().await;
                }
                // Kept: it holds the session's thread as far as it has run.
                resumed = slot.is_some();
            }
            let mut fell_back = None;
            if slot.is_none() {
                let resume = want.resume.clone();
                *slot = Some(match Proc::spawn(self.backend(), &ctx.cwd, want.clone(), &ask, self.status.clone()).await {
                    Ok(p) => {
                        resumed = resume.is_some();
                        p
                    }
                    // A thread Claude Code cannot resume — its transcript
                    // gone, or a copy it would not take — is no reason to
                    // lose the session: the log has it, and a new thread is
                    // seeded from it (R-SWITCH-4, R-INST-4).
                    Err(e) if resume.is_some() && ctx.handoff.is_some() && e.code != "backend_not_found" => {
                        fell_back = Some(format!("Claude Code on {} could not resume its session {} ({}: {})", self.instance.name, resume.unwrap_or_default(), e.code, e.message));
                        Proc::spawn(self.backend(), &ctx.cwd, Launch { resume: None, ..want }, &ask, self.status.clone()).await?
                    }
                    Err(e) => return Err(e),
                });
            }
            let p = slot.as_mut().expect("spawned above");
            // What the thread did not run goes ahead of the prompt, in the
            // same user message (R-SWITCH-2); what was sent is reported.
            let (prompt, handoff) = crate::handoff::Handoff::opening(ctx.handoff.as_ref(), &prompt, resumed, fell_back);
            if let Some(ev) = handoff {
                let _ = events.send(ev).await;
            }
            let outcome = p.turn(Some(&prompt), &mut ctx, &ask, &events, self.backend(), &self.instance.name).await;
            self.settle(slot, outcome, ask, &events).await
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.wake().await;
            if let Some(p) = self.proc.lock().await.take() {
                p.shutdown().await;
            }
            // Asked for: nothing of it is news to anyone.
            let _ = self.status.clear();
        })
    }

    fn pending(&self) -> Option<Pending> {
        self.status.pending().clone()
    }

    fn busy(&self) -> bool {
        self.status.pending().is_some() || !self.status.agents().is_empty()
    }
}

impl ClaudeEngine {
    /// After a turn: a process that died, or a turn that failed partway, is
    /// not trusted with the next turn, which starts clean on --resume; one
    /// that lives on is read between turns, under this turn's permissions.
    async fn settle(&self, mut slot: tokio::sync::MutexGuard<'_, Option<Proc>>, outcome: Result<TurnEnd, EngineError>, ask: Answers, events: &Events) -> Result<TurnEnd, EngineError> {
        let Some(p) = slot.as_mut() else { return outcome };
        if outcome.is_err() || !p.alive() {
            if let Some(agents) = self.status.clear() {
                let _ = events.send(EngineEvent::BackendAgents { agents }).await;
            }
            if let Some(old) = slot.take() {
                old.kill().await;
            }
            return outcome;
        }
        p.answers = Some(Arc::new(ask));
        drop(slot);
        self.rest();
        outcome
    }
}

/// Between turns: the process read until the next turn wants it — what
/// Claude Code asks answered under the last turn's permissions, its agents
/// kept and metered — or until Claude Code begins a turn by itself, which
/// then waits for the host.
async fn idle(proc: Arc<tokio::sync::Mutex<Option<Proc>>>, mut stop: oneshot::Receiver<()>, tell: Option<Idle>) {
    let say = |ev: EngineEvent| {
        if let Some(t) = &tell {
            t(ev);
        }
    };
    let mut slot = proc.lock().await;
    let Some(p) = slot.as_mut().filter(|p| p.begun.is_empty()) else { return };
    loop {
        let msg = tokio::select! {
            biased;
            _ = &mut stop => return,
            m = p.out.recv() => m,
        };
        let Some(msg) = msg else {
            // It exited with nobody asking: the next turn starts another.
            p.exited = true;
            if let Some(agents) = p.status.clear() {
                say(EngineEvent::BackendAgents { agents });
            }
            return;
        };
        match msg["type"].as_str() {
            Some("control_request") => {
                let id = msg["request_id"].as_str().unwrap_or_default();
                let answer = match p.answers.clone() {
                    Some(ask) => ask.answer(id, &msg["request"], None).await,
                    None => json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": "krowk has no turn to answer this under"}}),
                };
                if p.send(&answer).await.is_err() {
                    return;
                }
            }
            Some("control_response") => {}
            _ => {
                if let Some(agents) = p.track(&msg) {
                    say(EngineEvent::BackendAgents { agents });
                }
                p.idle_line(msg);
                if !p.begun.is_empty() {
                    break;
                }
            }
        }
    }
    p.armed = false;
    let reason = p.finished.take().unwrap_or_else(|| BEGAN.into());
    // The mode it runs in is the one the process is in, which krowk
    // accepted: the last turn's, or default once that plan turn's
    // ExitPlanMode was approved — never looser than asked but for that.
    let asked = p.answers.as_ref().map(|a| a.mode);
    let mode = match (p.mode.as_str(), asked) {
        ("plan", _) => Some(PermissionMode::Plan),
        ("default", Some(PermissionMode::Plan)) => Some(PermissionMode::Default),
        _ => asked,
    };
    *p.status.pending() = Some(Pending { reason: reason.clone(), mode });
    say(EngineEvent::Unprompted { reason });
}

/// What a control request is answered with: the turn it arrived in.
struct Answers {
    session_id: String,
    turn_id: String,
    model: ModelRef,
    cwd: PathBuf,
    mode: PermissionMode,
    krowk_version: String,
    /// The turn's permissions, with the instance's config directory kept
    /// from edits too.
    gate: Gate,
    /// Where a bridged `publish` sends files.
    evidence: Option<crate::evidence::Evidence>,
}

/// Where a question for a person goes while a turn runs: the turn's events,
/// and its interrupt. Outside a turn (a request krowk made, such as
/// `initialize`) nobody is asked.
type Asking<'a> = Option<(&'a Events, &'a tokio::sync::watch::Receiver<bool>)>;

impl Answers {
    /// Judges a call, asking a person when the verdict says to and a turn
    /// is there to ask through.
    async fn permit(&self, tool: &str, input: &Value, asking: Asking<'_>) -> Result<(), String> {
        if tool == "AskUserQuestion" && !self.mode.asks_nothing() {
            return Err("krowk is running this turn without a person to ask: decide, say what you assumed, and carry on.".into());
        }
        let call = call_of(tool, input, &self.cwd);
        match asking {
            Some((events, cancel)) => self.gate.check(&call, tool, input, None, events, cancel).await.map(drop),
            None => match self.gate.verdict(&call, None) {
                Verdict::Allow(_) => Ok(()),
                Verdict::Deny(m) => Err(m),
                Verdict::Ask { reason, .. } => Err(format!("{} needs approval — {reason} — and Claude Code asked outside a turn, where nobody can be asked", permissions::summary(&call))),
            },
        }
    }

    /// The answer to one of Claude Code's control requests.
    async fn answer(&self, request_id: &str, req: &Value, asking: Asking<'_>) -> Value {
        let subtype = req.get("subtype").and_then(Value::as_str).unwrap_or_default();
        let success = |response: Value| json!({"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": response}});
        match subtype {
            "can_use_tool" => {
                let tool = req.get("tool_name").and_then(Value::as_str).unwrap_or_default();
                let input = req.get("input").cloned().unwrap_or_else(|| json!({}));
                match self.permit(tool, &input, asking).await {
                    Ok(()) => success(json!({"behavior": "allow", "updatedInput": input})),
                    Err(message) => success(json!({"behavior": "deny", "message": message})),
                }
            }
            "mcp_message" if req.get("server_name").and_then(Value::as_str) == Some(bridge::SERVER) => {
                let env = BridgeEnv {
                    session_id: &self.session_id,
                    turn_id: &self.turn_id,
                    model: &self.model,
                    cwd: &self.cwd,
                    backend: BACKEND,
                    krowk_version: &self.krowk_version,
                    gate: &self.gate,
                    cancel: asking.map(|(_, c)| c),
                    evidence: self.evidence.as_ref().zip(asking.map(|(e, _)| e)),
                };
                success(json!({"mcp_response": bridge::handle(req.get("message").unwrap_or(&Value::Null), &env).await}))
            }
            // No hooks are registered, so none should be called back.
            "hook_callback" => success(json!({})),
            other => json!({"type": "control_response", "response": {"subtype": "error", "request_id": request_id, "error": format!("krowk does not answer the control request {other:?}")}}),
        }
    }
}

/// One running `claude` process.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Its stdout, a JSON value a line, from the one task that reads it.
    out: mpsc::Receiver<Value>,
    stderr: Arc<Mutex<String>>,
    binary: String,
    launch: Launch,
    /// `initialize` listed models: `set_model` switches in place.
    set_model: bool,
    next: u64,
    exited: bool,
    /// The process group: Claude Code and whatever it started.
    pid: Option<u32>,
    /// The permission mode its last turn reported; empty before one has.
    mode: String,
    /// The group was stopped and reaped; nothing is left to signal.
    group_done: bool,
    /// Its agents and a turn it began, shared with the engine.
    status: Arc<Status>,
    /// The last turn's permissions: what is asked between turns is
    /// answered under them.
    answers: Option<Arc<Answers>>,
    /// A turn Claude Code began by itself: its lines read so far, `init`
    /// first. Not empty means it waits.
    begun: Vec<Value>,
    /// An `init` read between turns before anything had finished — not a
    /// turn's start by itself — kept in case what follows is one.
    held_init: Option<Value>,
    /// Why the turn Claude Code begins next begins: the last background
    /// agent that finished, in words. Kept apart from `armed`, so a prompt
    /// in between takes away the leave to start a turn, not the words.
    finished: Option<String>,
    /// A background agent finished since the last prompt: an `init` read
    /// between turns is Claude Code beginning a turn to answer it.
    armed: bool,
    /// Background agents `background_tasks_changed` took off the list, for
    /// the words of their `task_notification`, which comes after it.
    ended: Vec<BackendAgent>,
    /// Lines read while krowk waited on a control request before a turn —
    /// an agent's end, its calls — for that turn to read first.
    backlog: VecDeque<Value>,
    /// A `result` has carried `origin`: this binary says which turn a
    /// result closes, and none is ever taken on count.
    origins: bool,
}

/// How long a stopped process group gets between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// After Claude Code exits, how long what it already wrote is read.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

#[cfg(unix)]
fn signal_group(pid: Option<u32>, sig: i32) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
        // SAFETY: a signal to the group this process made (process_group(0)).
        unsafe {
            libc::kill(-pid, sig);
        }
    }
}

impl Drop for Proc {
    /// A process let go of without `shutdown` — a host dropped mid-turn — is
    /// killed with everything it started, never left running.
    fn drop(&mut self) {
        if !self.group_done {
            #[cfg(unix)]
            signal_group(self.pid, libc::SIGKILL);
            crate::group::release(self.pid);
        }
    }
}

/// The next line of a turn: what was read before it began, then stdout.
async fn next_line(queued: &mut VecDeque<Value>, out: &mut mpsc::Receiver<Value>) -> Option<Value> {
    match queued.pop_front() {
        Some(v) => Some(v),
        None => out.recv().await,
    }
}

/// Whether a line is of the main conversation, not a subagent's.
fn main_thread(msg: &Value) -> bool {
    msg.get("parent_tool_use_id").is_none_or(Value::is_null)
}

fn tail(s: &str) -> String {
    let s = s.trim();
    let cut = s.char_indices().rev().nth(STDERR_TAIL).map(|(i, _)| i).unwrap_or(0);
    s[cut..].to_string()
}

impl Proc {
    async fn spawn(b: &Backend, cwd: &Path, launch: Launch, ask: &Answers, status: Arc<Status>) -> Result<Proc, EngineError> {
        let mut cmd = tokio::process::Command::new(b.path.as_deref().unwrap_or(Path::new(&b.binary)));
        cmd.args(args(&launch, &b.args)).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let (remove, set) = environment(b);
        for k in remove {
            cmd.env_remove(k);
        }
        cmd.envs(set);
        // Its own process group: a Ctrl-C at the terminal is krowk's to turn
        // into an interrupt, not a signal that kills Claude Code mid-write.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::new("backend_not_found", format!("{} was not found — install Claude Code (https://claude.com/claude-code), or name the binary with `krowk connect anthropic --method subscription --binary <path>`", b.binary))
            } else {
                EngineError::new("backend_failed", format!("{} could not be started: {e}", b.binary))
            }
        })?;
        let pid = child.id();
        crate::group::register(pid);
        let stdin = child.stdin.take();
        // One reader for the life of the process: between turns too, when
        // Claude Code goes on without krowk. A line that is not JSON (a
        // warning a wrapper printed) is skipped; the channel closes at the
        // end of the stream.
        let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
        let (tx, out) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Ok(Some(l)) = lines.next_line().await {
                if let Ok(v) = serde_json::from_str::<Value>(&l)
                    && tx.send(v).await.is_err()
                {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let keep = stderr.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut s = keep.lock().unwrap_or_else(|e| e.into_inner());
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if s.len() > 4 * STDERR_TAIL {
                        let t = tail(&s);
                        *s = t;
                    }
                }
            });
        }
        let mut p = Proc {
            child,
            stdin,
            out,
            stderr,
            binary: b.binary.clone(),
            launch,
            set_model: false,
            next: 0,
            exited: false,
            pid,
            group_done: false,
            mode: String::new(),
            status,
            answers: None,
            begun: Vec::new(),
            held_init: None,
            finished: None,
            armed: false,
            ended: Vec::new(),
            backlog: VecDeque::new(),
            origins: false,
        };
        let init = p.request(json!({"subtype": "initialize", "hooks": null}), ask, INITIALIZE_TIMEOUT).await?;
        p.set_model = init.get("models").is_some_and(Value::is_array);
        Ok(p)
    }

    fn alive(&mut self) -> bool {
        !self.exited && matches!(self.child.try_wait(), Ok(None))
    }

    fn died(&mut self, while_doing: &str) -> EngineError {
        self.exited = true;
        let status = self.child.try_wait().ok().flatten().map(|s| format!(" ({s})")).unwrap_or_default();
        let said = tail(&self.stderr.lock().unwrap_or_else(|e| e.into_inner()));
        let said = if said.is_empty() { String::new() } else { format!(": {said}") };
        EngineError::new("backend_exited", format!("claude exited{status} {while_doing}{said} — the session is kept; continue it with --resume"))
    }

    async fn send(&mut self, v: &Value) -> Result<(), EngineError> {
        let mut line = v.to_string();
        line.push('\n');
        let Some(stdin) = self.stdin.as_mut() else { return Err(self.died("before krowk could write to it")) };
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return Err(self.died("while krowk was writing to it"));
        }
        Ok(())
    }

    fn request_id(&mut self) -> String {
        self.next += 1;
        format!("krowk_{}", self.next)
    }

    /// Sends a control request and waits for its answer, answering Claude
    /// Code's own requests meanwhile — `initialize` is answered only after
    /// the MCP server has been.
    async fn request(&mut self, req: Value, ask: &Answers, within: Duration) -> Result<Value, EngineError> {
        let id = self.request_id();
        let subtype = req["subtype"].as_str().unwrap_or_default().to_string();
        self.send(&json!({"type": "control_request", "request_id": id, "request": req})).await?;
        let wait = async {
            loop {
                let Some(msg) = self.out.recv().await else { return Err(self.died(&format!("before it answered {subtype}"))) };
                match msg["type"].as_str() {
                    Some("control_request") => {
                        let answer = ask.answer(msg["request_id"].as_str().unwrap_or_default(), &msg["request"], None).await;
                        self.send(&answer).await?;
                    }
                    Some("control_response") if msg.pointer("/response/request_id").and_then(Value::as_str) == Some(id.as_str()) => {
                        let r = &msg["response"];
                        if r["subtype"] == "error" {
                            return Err(EngineError::new("backend_failed", format!("claude refused {subtype}: {}", r["error"].as_str().unwrap_or("no reason given"))));
                        }
                        return Ok(r.get("response").cloned().unwrap_or(Value::Null));
                    }
                    Some("control_response") => {}
                    // What goes on while it answers — a turn Claude Code
                    // began, an agent's lines — is kept for the turn that
                    // reads it next.
                    _ if !self.begun.is_empty() => self.begun.push(msg),
                    _ => self.backlog.push_back(msg),
                }
            }
        };
        let answered = tokio::time::timeout(within, wait).await;
        match answered {
            Ok(r) => r,
            Err(_) => {
                self.terminate().await;
                Err(EngineError::new("backend_unresponsive", format!("{} did not answer {subtype} within {} seconds — is it Claude Code?", self.binary, within.as_secs())))
            }
        }
    }

    /// Switches the model in place; false when Claude Code refused, and the
    /// caller starts a new process instead.
    async fn set_model(&mut self, model: &str, ask: &Answers) -> bool {
        match self.request(json!({"subtype": "set_model", "model": model}), ask, INITIALIZE_TIMEOUT).await {
            Ok(_) => {
                self.launch.model = model.into();
                true
            }
            Err(_) => false,
        }
    }

    /// Runs one turn: the prompt in, the stream out, until `result`. With
    /// no prompt, the turn Claude Code began by itself, from what was read
    /// of it between turns.
    async fn turn(&mut self, prompt: Option<&str>, ctx: &mut TurnContext, ask: &Answers, events: &Events, b: &Backend, instance: &str) -> Result<TurnEnd, EngineError> {
        let mut t = Translator::default();
        t.meter = std::mem::take(&mut *self.status.meter());
        let r = self.read_turn(prompt, &mut t, ctx, ask, events, b, instance).await;
        *self.status.meter() = std::mem::take(&mut t.meter);
        r
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_turn(&mut self, prompt: Option<&str>, t: &mut Translator, ctx: &mut TurnContext, ask: &Answers, events: &Events, b: &Backend, instance: &str) -> Result<TurnEnd, EngineError> {
        if let Some(prompt) = prompt {
            self.send(&json!({"type": "user", "message": {"role": "user", "content": prompt}, "parent_tool_use_id": null, "session_id": ""})).await?;
        }
        let (mut queued, mut why, mut owed, mut before) = self.take_begun(prompt.is_some());
        let mut interrupted = false;
        let mut receipt_required = false;
        let mut receipt: Option<String> = None;
        let mut deadline: Option<tokio::time::Instant> = None;
        let mut announced = false;
        let mut gone = false;
        let mut drain: Option<tokio::time::Instant> = None;
        let result = loop {
            let until = crate::engine::sleep_until(deadline);
            let drained = crate::engine::sleep_until(drain);
            tokio::select! {
                biased;
                _ = cancelled(&mut ctx.cancel), if !interrupted => {
                    interrupted = true;
                    let (id, until) = self.interrupt(receipt_required).await?;
                    receipt = Some(id);
                    deadline = Some(until);
                }
                _ = until => {
                    // Claude Code did not stop in time: the process goes, the
                    // turn keeps what it made, the session continues on --resume.
                    self.terminate().await;
                    finish(t, events).await;
                    return Ok(TurnEnd::Interrupted);
                }
                // Claude Code exited — crashed, or was killed — while
                // something it started still holds its output open: what it
                // wrote is read for a moment, then the turn ends.
                _ = self.child.wait(), if !gone => {
                    gone = true;
                    drain = Some(tokio::time::Instant::now() + DRAIN_GRACE);
                }
                _ = drained => {
                    finish(t, events).await;
                    let e = self.died("before the turn finished");
                    return if interrupted { Ok(TurnEnd::Interrupted) } else { Err(e) };
                }
                msg = next_line(&mut queued, &mut self.out) => {
                    let Some(msg) = msg else {
                        finish(t, events).await;
                        let e = self.died("before the turn finished");
                        if interrupted {
                            return Ok(TurnEnd::Interrupted);
                        }
                        return Err(e);
                    };
                    match msg["type"].as_str() {
                        Some("control_request") => self.answer(&msg, ask, events, &ctx.cancel).await?,
                        Some("control_response") => {
                            // The interrupt's receipt: once acknowledged, the
                            // result is waited for the full grace.
                            if receipt.is_some() && msg.pointer("/response/request_id").and_then(Value::as_str) == receipt.as_deref() {
                                receipt = None;
                                deadline = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                            }
                        }
                        _ => {
                            if let Some(agents) = self.track(&msg) {
                                let _ = events.send(EngineEvent::BackendAgents { agents }).await;
                            }
                            let out = t.apply(&msg)?;
                            forward(events, out).await;
                            if !announced && let Some(init) = t.init.clone() {
                                announced = true;
                                receipt_required = init.capabilities.iter().any(|c| c == "interrupt_receipt_v1");
                                self.came_up(&init, ask, events, b, instance).await?;
                            }
                            if let Some(o) = t.outcome.take() {
                                if self.fold(&o, prompt.is_some(), &mut owed, (&mut why, &mut before), events).await {
                                    continue;
                                }
                                break o;
                            }
                        }
                    }
                }
            }
        };
        self.wind_up(before, events).await;
        if interrupted {
            return Ok(TurnEnd::Interrupted);
        }
        if result.subtype == "success" && !result.is_error {
            return Ok(TurnEnd::Completed);
        }
        Err(failed(&result, t.limit.as_ref(), instance))
    }

    /// Asks Claude Code to stop: the request's id, and how long it has.
    async fn interrupt(&mut self, receipt_required: bool) -> Result<(String, tokio::time::Instant), EngineError> {
        let id = self.request_id();
        self.send(&json!({"type": "control_request", "request_id": id.clone(), "request": {"subtype": "interrupt"}})).await?;
        Ok((id, tokio::time::Instant::now() + if receipt_required { RECEIPT_GRACE } else { INTERRUPT_GRACE }))
    }

    /// What was read between turns, for the turn about to be read: the
    /// lines queued, why Claude Code began its own turn, how many results
    /// are owed before a prompt's, and the words of what finished before.
    fn take_begun(&mut self, prompted: bool) -> (VecDeque<Value>, Option<String>, u32, Option<String>) {
        // A turn Claude Code began is read from where the idle loop left
        // it. Under a prompt it is the race the host could not see — it
        // began as the prompt was on its way — and its `result` is not the
        // prompt's: an `origin` says so, and a binary too old to send one
        // is taken at its word that one result is owed first.
        let mut queued: VecDeque<Value> = std::mem::take(&mut self.begun).into();
        let why = self.status.pending().take().map(|p| p.reason);
        let owed = u32::from(prompted && !queued.is_empty() && !self.origins);
        queued.append(&mut self.backlog);
        self.held_init = None;
        if !queued.is_empty() {
            self.ended.clear();
        }
        // What finished before a prompt either began its turn — waiting, or
        // folded here — or begins it only after the prompt's answer, which
        // a stray `init` is not proof of: the leave to start one goes, the
        // words stay for the turn that does follow.
        let before = if prompted { self.finished.take() } else { None };
        if prompted {
            self.armed = false;
        }
        (queued, why, owed, before)
    }

    /// Whether the `result` read ends a turn of Claude Code's own inside a
    /// prompted one, which is folded into it: noted, and the prompt's
    /// answer read on. `owed` counts results due before the prompt's.
    async fn fold(&mut self, o: &stream::Outcome, prompted: bool, owed: &mut u32, (why, before): (&mut Option<String>, &mut Option<String>), events: &Events) -> bool {
        self.origins |= o.unprompted;
        if self.origins {
            *owed = 0;
        }
        if !(prompted && (o.unprompted || *owed > 0)) {
            return false;
        }
        *owed = if o.unprompted { 0 } else { *owed - 1 };
        let reason = why.take().or_else(|| self.finished.take()).or_else(|| before.take()).unwrap_or_else(|| BEGAN.into());
        *before = None;
        self.armed = false;
        self.ended.clear();
        note(events, folded(&reason)).await;
        true
    }

    /// What is left once a turn's `result` is read.
    async fn wind_up(&mut self, before: Option<String>, events: &Events) {
        // Words no fold took are the next self-started turn's, unless an
        // agent finished since.
        if self.finished.is_none() {
            self.finished = before;
        }
        // An agent run in the foreground ends with the turn that ran it.
        if let Some(agents) = self.drop_foreground() {
            let _ = events.send(EngineEvent::BackendAgents { agents }).await;
        }
    }

    /// Answers a `control_request` of Claude Code's. An approved
    /// ExitPlanMode or EnterPlanMode moves it to default or plan for what
    /// follows.
    async fn answer(&mut self, msg: &Value, ask: &Answers, events: &Events, cancel: &tokio::sync::watch::Receiver<bool>) -> Result<(), EngineError> {
        let answer = ask.answer(msg["request_id"].as_str().unwrap_or_default(), &msg["request"], Some((events, cancel))).await;
        if msg.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool")
            && answer.pointer("/response/response/behavior").and_then(Value::as_str) == Some("allow")
        {
            match msg.pointer("/request/tool_name").and_then(Value::as_str) {
                Some("ExitPlanMode") => self.mode = "default".into(),
                Some("EnterPlanMode") => self.mode = "plan".into(),
                _ => {}
            }
        }
        self.send(&answer).await
    }

    /// Claude Code's `init`: its mode taken, then the turn announced.
    async fn came_up(&mut self, init: &Init, ask: &Answers, events: &Events, b: &Backend, instance: &str) -> Result<(), EngineError> {
        self.mode.clone_from(&init.permission_mode);
        // A mode looser than krowk's — a setting, a
        // wrapper — is stopped before it runs anything.
        if !init.permission_mode.is_empty() && looser_than(&init.permission_mode, ask.mode) {
            self.terminate().await;
            return Err(EngineError::new(
                "backend_permission_mode",
                format!(
                    "Claude Code on {instance} came up in its `{}` permission mode, looser than krowk's `{}` for this turn, so krowk stopped it before it ran anything — check `permissions.defaultMode` in its settings and the instance's `args`, or rerun krowk with a mode that allows it",
                    init.permission_mode,
                    ask.mode.name()
                ),
            ));
        }
        announce(events, init, b, instance).await
    }

    /// Keeps the list of agents from Claude Code's `system` lines, and says
    /// it, whole, when it changed. An agent counts when it is the
    /// conversation's own (`local_agent`, at the first depth) — not a shell,
    /// not one a subagent started. It is gone at its `task_notification`,
    /// at a `task_updated` that finishes it, or — run in the background —
    /// when `background_tasks_changed` no longer lists it.
    fn track(&mut self, msg: &Value) -> Option<Vec<BackendAgent>> {
        if msg["type"] != "system" {
            return None;
        }
        let s = |k: &str| msg.get(k).and_then(Value::as_str).unwrap_or_default();
        let id = s("task_id");
        let status = self.status.clone();
        let mut agents = status.agents();
        let ids = |a: &[Tracked]| a.iter().map(|t| t.agent.task_id.clone()).collect::<Vec<_>>();
        let before = ids(&agents);
        let mut done = None;
        match s("subtype") {
            "task_started" => {
                let own = s("task_type") == "local_agent" && msg["owned_by_subagent"] != true && msg.get("spawn_depth").and_then(Value::as_u64).is_none_or(|d| d <= 1);
                if own && !id.is_empty() && !agents.iter().any(|t| t.agent.task_id == id) {
                    let agent = BackendAgent { task_id: id.into(), description: s("description").into(), agent: Some(s("subagent_type").to_string()).filter(|a| !a.is_empty()) };
                    agents.push(Tracked { agent, background: msg["is_backgrounded"] == true });
                }
            }
            "task_notification" => done = Some(s("status")),
            "task_updated" => {
                let st = msg.pointer("/patch/status").and_then(Value::as_str).unwrap_or_default();
                if !matches!(st, "" | "pending" | "running") {
                    done = Some(st);
                }
            }
            "background_tasks_changed" => {
                let listed: Vec<&str> = msg["tasks"].as_array().map(|a| a.iter().filter_map(|t| t["task_id"].as_str()).collect()).unwrap_or_default();
                let (kept, gone): (Vec<Tracked>, Vec<Tracked>) = agents.drain(..).partition(|t| !t.background || listed.contains(&t.agent.task_id.as_str()));
                *agents = kept;
                for t in gone {
                    self.finished = Some(format!("background agent “{}” finished", t.agent.description));
                    self.armed = true;
                    self.ended.push(t.agent);
                }
            }
            _ => return None,
        }
        if let Some(st) = done {
            let st = if st.is_empty() { "finished" } else { st };
            let at = agents.iter().position(|t| t.agent.task_id == id);
            let gone = at.map(|i| agents.remove(i));
            // Its end in words, for the turn Claude Code begins to answer it.
            let what = match gone {
                Some(t) => t.background.then_some(t.agent.description),
                None => self.ended.iter().find(|a| a.task_id == id).map(|a| a.description.clone()),
            };
            if let Some(d) = what {
                self.finished = Some(format!("background agent “{d}” {st}"));
                self.armed = true;
            }
        }
        (ids(&agents) != before).then(|| agents.iter().map(|t| t.agent.clone()).collect())
    }

    /// The agents run in the foreground, gone with their turn.
    fn drop_foreground(&mut self) -> Option<Vec<BackendAgent>> {
        let mut agents = self.status.agents();
        let before = agents.len();
        agents.retain(|t| t.background);
        (agents.len() != before).then(|| agents.iter().map(|t| t.agent.clone()).collect())
    }

    /// A line read between turns, after `track`: a subagent's call is
    /// metered, and a turn Claude Code began by itself is kept, from its
    /// first line, for the turn that runs it.
    fn idle_line(&mut self, msg: Value) {
        if !main_thread(&msg) {
            if msg["type"] == "assistant" {
                self.status.meter().see(&msg, None);
            }
            return;
        }
        match (msg["type"].as_str().unwrap_or_default(), msg["subtype"].as_str().unwrap_or_default()) {
            // A turn begins with `init`, and one after an agent finished is
            // Claude Code answering it. One with nothing finished is kept
            // aside: Claude Code sends some between turns that no turn
            // follows.
            ("system", "init") if self.armed => self.begun.push(msg),
            ("system", "init") => self.held_init = Some(msg),
            // Whatever began it, the conversation's own lines are a turn.
            ("stream_event" | "assistant" | "user" | "result", _) => {
                self.begun.extend(self.held_init.take());
                self.begun.push(msg);
            }
            _ => {}
        }
    }

    /// Stops the whole process group — Claude Code and every shell and
    /// server it started: SIGTERM, a moment to write what it was writing,
    /// then SIGKILL.
    async fn terminate(&mut self) {
        self.exited = true;
        if self.group_done {
            return;
        }
        #[cfg(unix)]
        {
            signal_group(self.pid, libc::SIGTERM);
            let _ = tokio::time::timeout(TERM_GRACE, self.child.wait()).await;
            signal_group(self.pid, libc::SIGKILL);
        }
        let _ = self.child.kill().await;
        self.group_done = true;
        crate::group::release(self.pid);
    }

    async fn kill(mut self) {
        self.terminate().await;
    }

    /// Asks the process to exit by closing its stdin, gives it a moment, and
    /// then stops what is left of its group, so nothing it started outlives
    /// the session.
    async fn shutdown(mut self) {
        drop(self.stdin.take());
        let _ = tokio::time::timeout(EXIT_GRACE, self.child.wait()).await;
        self.terminate().await;
    }
}


/// Why a turn Claude Code ended without success failed.
fn failed(result: &stream::Outcome, limit: Option<&crate::protocol::LimitStatus>, instance: &str) -> EngineError {
    let said = if !result.text.trim().is_empty() { result.text.trim().to_string() } else if !result.errors.is_empty() { result.errors.join("; ") } else { result.subtype.clone() };
    let lower = said.to_lowercase();
    // The account's plan limit, or the API's rate limit: what a switch to
    // another instance answers (R-INST-7), with when it lifts.
    let rejected = limit.filter(|l| l.status == crate::protocol::LimitState::Limited);
    // Only what Claude Code reports about the account — its rate-limit
    // event, the API's status, its own error list — never the model's
    // words, which a prompt can make say anything.
    let errors = result.errors.join(" ").to_lowercase();
    if rejected.is_some() || result.api_status == Some(429) || errors.contains("usage limit") || errors.contains("limit reached") || errors.contains("rate limit") {
        let resets = rejected.and_then(|l| l.resets_at_ms).or_else(|| limit.and_then(|l| l.resets_at_ms));
        let until = resets.map(|ms| format!(" until {}", crate::host::clock(ms))).unwrap_or_default();
        return EngineError::new("rate_limited", format!("Claude Code on {instance} is limited{until} ({said}) — continue on another instance with --model, or wait")).with_status(result.api_status.unwrap_or(429)).with_resets(resets);
    }
    if lower.contains("/login") || lower.contains("not logged in") || lower.contains("invalid api key") || result.api_status == Some(401) {
        let add = crate::connect::connect_command(instance, "claude-code");
        return EngineError::new("not_authenticated", format!("Claude Code is not signed in for the {instance} instance ({said}) — sign in with `{add}`, which runs Claude's own login")).with_status(401);
    }
    EngineError::new("backend_failed", format!("Claude Code could not finish the turn: {said}")).with_status(result.api_status.unwrap_or(0))
}

/// What the translator still holds, sent on as the turn ends.
async fn finish(t: &mut Translator, events: &Events) {
    let mut out = Vec::new();
    t.finish(&mut out);
    forward(events, out).await;
}

async fn forward(events: &Events, out: Vec<EngineEvent>) {
    for ev in out {
        let _ = events.send(ev).await;
    }
}

/// A note of krowk's in the turn, as a `userText` item.
async fn note(events: &Events, text: String) {
    let item_id = krowk_store::new_id();
    let _ = events.send(EngineEvent::ItemStarted { item_id: item_id.clone(), kind: ItemKind::UserText }).await;
    let _ = events.send(EngineEvent::ItemCompleted { item_id, item: Item::UserText { text } }).await;
}

/// The turn's context and the Claude session behind it, from `init`.
async fn announce(events: &Events, init: &Init, b: &Backend, instance: &str) -> Result<(), EngineError> {
    let bridged = bridge::definitions();
    let tools = init
        .tools
        .iter()
        .map(|name| bridged.iter().find(|d| &d.name == name).cloned().unwrap_or_else(|| ToolDefinition { name: name.clone(), description: String::new(), input_schema: json!({}), grammar: None }))
        .collect();
    let _ = events.send(EngineEvent::Context { system: SYSTEM_NOTE.into(), tools }).await;
    if let Some((_, status)) = init.mcp_servers.iter().find(|(n, _)| n == bridge::SERVER)
        && status != "connected"
    {
        return Err(EngineError::new("backend_failed", format!("Claude Code on {instance} did not connect krowk's tools (the krowk MCP server is {status})")));
    }
    let billing = match init.api_key_source.as_str() {
        "" => None,
        "none" => Some(Billing::Subscription),
        _ => Some(Billing::ApiKey),
    };
    // A session id is joined to a path only when it is one (a UUID's
    // characters): the vendor's output is not trusted to name a file.
    let transcript = crate::handoff::valid_session_id(&init.session_id).then(|| b.home.as_ref().map(|h| transcript_path(h, &init.cwd, &init.session_id).display().to_string())).flatten();
    if !init.session_id.is_empty() {
        let _ = events.send(EngineEvent::BackendSession { backend: BACKEND.into(), session_id: init.session_id.clone(), transcript, billing }).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_back_1_the_process_is_launched_with_the_stream_json_protocol_and_krowks_mcp_server() {
        let l = Launch { model: "haiku".into(), resume: Some("cc-1".into()), effort: Some("high"), plan: false, disallowed: Vec::new() };
        let a = args(&l, &["--add-dir".into(), "/x".into()]);
        let s = a.join(" ");
        assert!(s.starts_with("-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio --mcp-config "), "{s}");
        let rest = format!("--strict-mcp-config --settings {SETTINGS} --model haiku --resume cc-1 --effort high --permission-mode default --add-dir /x");
        assert!(s.contains(&format!(r#"{{"mcpServers":{{"krowk":{{"name":"krowk","type":"sdk"}}}}}} {rest}"#)) || s.contains(&format!(r#"{{"mcpServers":{{"krowk":{{"type":"sdk","name":"krowk"}}}}}} {rest}"#)), "{s}");
        // Commits and pull requests made through Claude Code carry no Claude attribution.
        let settings: Value = serde_json::from_str(SETTINGS).unwrap();
        assert_eq!(settings, json!({"attribution": {"commit": "", "pr": ""}, "includeCoAuthoredBy": false}));
        assert!(s.contains("--permission-mode default --add-dir"), "the mode is always named, so settings cannot loosen it: {s}");
        assert!(args(&Launch { plan: true, resume: None, effort: None, ..l.clone() }, &[]).join(" ").ends_with("--model haiku --permission-mode plan"));
        // R-PERM-1: krowk's deny rules reach what Claude Code allows by itself.
        let denied = args(&Launch { disallowed: vec!["Bash(rm:*)".into(), "Read(.env)".into()], ..l }, &[]).join(" ");
        assert!(denied.contains("--disallowedTools Bash(rm:*) Read(.env) --permission-mode default"), "{denied}");
        let mut p = permissions::Policy::modes_only(Path::new("/repo"));
        p.loaded.rules.push((permissions::Kind::Deny, permissions::rules::parse("Bash(rm:*)", "test", Path::new("/repo")).unwrap()));
        let gate = |m| Gate::new(p.clone(), m, Default::default(), None, None, "s", "t");
        assert_eq!(disallowed(&gate(PermissionMode::BypassPermissions)), ["Bash(rm:*)"]);
        assert!(disallowed(&gate(PermissionMode::Unhinged)).is_empty(), "unhinged starts Claude Code without krowk's deny rules");
        assert!(looser_than("bypassPermissions", PermissionMode::AcceptEdits) && looser_than("acceptEdits", PermissionMode::Default) && looser_than("auto", PermissionMode::AcceptEdits));
        assert!(looser_than("default", PermissionMode::Plan) && looser_than("somethingNew", PermissionMode::AcceptEdits));
        assert!(!looser_than("default", PermissionMode::Default) && !looser_than("plan", PermissionMode::Plan) && !looser_than("bypassPermissions", PermissionMode::BypassPermissions) && !looser_than("bypassPermissions", PermissionMode::Unhinged));
        assert_eq!([Effort::None, Effort::Medium, Effort::Max].map(effort_level), ["low", "medium", "max"]);
    }

    #[test]
    fn r_back_1_approvals_follow_krowks_modes_and_its_own_tools_always_run() {
        let cwd = std::env::temp_dir().join(format!("krowk-approve-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let judge = |m, t: &str, i: &Value, protected: &[PathBuf]| crate::permissions::judge(m, Ok(call_of(t, i, &cwd)), &cwd, protected);
        let approve = |m, t: &str, i: &Value| judge(m, t, i, &[]);
        let d = PermissionMode::Default;
        assert!(approve(d, "mcp__krowk__session_info", &json!({})).is_ok());
        assert!(approve(d, "Read", &json!({"file_path": "README.md"})).is_ok());
        assert!(approve(d, "Read", &json!({"file_path": "/etc/passwd"})).unwrap_err().contains("outside the working directory"));
        assert!(approve(d, "Edit", &json!({"file_path": "a.txt"})).unwrap_err().contains("default mode"));
        assert!(approve(PermissionMode::AcceptEdits, "Edit", &json!({"file_path": "a.txt"})).is_ok());
        assert!(approve(PermissionMode::AcceptEdits, "Write", &json!({"file_path": ".git/config"})).unwrap_err().contains(".git"));
        assert!(approve(PermissionMode::AcceptEdits, "Bash", &json!({"command": "ls"})).unwrap_err().contains("runs a command"));
        assert!(approve(PermissionMode::BypassPermissions, "Bash", &json!({"command": "ls"})).is_ok());
        assert!(approve(d, "WebFetch", &json!({"url": "https://x"})).is_err(), "a fetch is asked about");
        assert!(approve(d, "SomethingNew", &json!({})).is_err(), "and so is a tool this build does not know");
        assert!(approve(PermissionMode::Plan, "Write", &json!({"file_path": "a.txt"})).unwrap_err().contains("plan mode"));
        // A glob that names another directory is judged like a path.
        assert!(approve(d, "Glob", &json!({"pattern": "/etc/**/*.conf"})).unwrap_err().contains("outside the working directory"));
        assert!(approve(d, "Grep", &json!({"pattern": "x", "glob": "/root/*"})).unwrap_err().contains("outside the working directory"));
        assert!(approve(d, "Glob", &json!({"pattern": "**/*.rs"})).is_ok() && approve(d, "Grep", &json!({"pattern": "/etc/"})).is_ok(), "a relative glob, and grep's regex, are not paths");
        assert!(approve(d, "Glob", &json!({"pattern": format!("{}/src/**", cwd.display())})).is_ok());
        assert!(approve(PermissionMode::BypassPermissions, "Glob", &json!({"pattern": "/etc/*"})).is_ok());
        // R-BACK-6's other half: an edit that would make Claude Code run
        // something — its settings, hooks, agents — needs a person's say.
        let ae = PermissionMode::AcceptEdits;
        for f in [".claude/settings.json", ".claude/settings.local.json", ".claude/agents/x.md", "sub/.claude/hooks/h.sh", ".Claude/settings.json", ".CLAUDE./agents/a.md", ".claude /settings.json", ".krowk/config.json"] {
            let e = approve(ae, "Write", &json!({"file_path": f})).unwrap_err();
            assert!(e.contains(".claude") || e.contains(".krowk"), "{f}: {e}");
        }
        let config = cwd.join("cc-config");
        std::fs::create_dir_all(&config).unwrap();
        let err = judge(ae, "Edit", &json!({"file_path": config.join("settings.json").display().to_string()}), std::slice::from_ref(&config)).unwrap_err();
        assert!(err.contains("settings decide what runs"), "{err}");
        assert!(approve(ae, "Edit", &json!({"file_path": ".GIT/config"})).unwrap_err().contains(".git"));
        let err = judge(ae, "Write", &json!({"file_path": cwd.join("CC-Config/settings.json").display().to_string()}), std::slice::from_ref(&config)).unwrap_err();
        assert!(err.contains("settings decide what runs"), "the config directory in another case: {err}");
        assert!(judge(PermissionMode::BypassPermissions, "Write", &json!({"file_path": ".claude/settings.json"}), &[config]).is_ok());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn r_back_5_the_transcript_is_found_where_claude_code_keeps_it() {
        let home = std::env::temp_dir().join(format!("krowk-transcript-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(transcript_path(&home, "/tmp/a.b/c", "s1"), home.join("projects/-tmp-a-b-c/s1.jsonl"), "where it will be");
        std::fs::create_dir_all(home.join("projects/-shortened-123")).unwrap();
        std::fs::write(home.join("projects/-shortened-123/s2.jsonl"), "").unwrap();
        assert_eq!(transcript_path(&home, "/a/very/long/path", "s2"), home.join("projects/-shortened-123/s2.jsonl"), "or where it is");
        let _ = std::fs::remove_dir_all(&home);
    }
}
