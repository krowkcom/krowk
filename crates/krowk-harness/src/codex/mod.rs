//! The Codex backend (R-BACK-3): krowk drives the user's own, unmodified
//! `codex` binary as `codex app-server` — the JSON-RPC interface OpenAI
//! built for third-party clients, and the sanctioned way to use a ChatGPT
//! subscription from one — as one long-lived process per session:
//!
//! ```text
//! codex app-server --listen stdio:// [the instance's args] -c hooks.PreToolUse=[…]
//! ```
//!
//! with `CODEX_HOME` set to the instance's home. One JSON object per line
//! each way; requests carry an `id` and are answered under it, both ways.
//! What krowk speaks, checked against the schema `codex app-server
//! generate-json-schema --experimental` writes for the pinned Codex
//! (`schema/codex/`, kept fresh by `scripts/codex_schema.sh`):
//!
//! | message | direction | what krowk does |
//! |---|---|---|
//! | `initialize`, `initialized` | krowk → codex | first on every process, as client `krowk`, opting into the experimental API for dynamic tools |
//! | `hooks/list`, `config/value/write` | krowk → codex | the paste guard krowk passes as a `PreToolUse` hook (`-c hooks.PreToolUse=…`, `with_paste_guard`) is trusted, by its hash, in the instance's Codex config |
//! | `account/read` | krowk → codex | whether the instance is signed in, and to what: a ChatGPT plan is a subscription, an API key a key (R-INST-3) |
//! | `model/list` | krowk → codex | the efforts each model takes, so `--effort` lands on one it does |
//! | `thread/start`, `thread/resume` | krowk → codex | the session's thread, in krowk's mode, with krowk's tools; resumed by the id the log's `backend.session` holds |
//! | `turn/start` | krowk → codex | a prompt; the turn is everything up to its `turn/completed` |
//! | `turn/steer` | krowk → codex | `Command::Steer`, into the running turn |
//! | `turn/interrupt` | krowk → codex | `Command::Interrupt`; the process lives on for the next turn |
//! | `item/*`, `thread/tokenUsage/updated` | codex → krowk | translated into krowk's events by `stream` (R-BACK-5) |
//! | `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, … | codex → krowk | judged by krowk's permission evaluator (`crate::permissions`), as `Bash(<command>)` and `Edit(<files>)` |
//! | `item/tool/call` | codex → krowk | a call of krowk's own tools, answered by `crate::bridge` |
//! | `item/tool/requestUserInput` | codex → krowk | the model's questions, asked of the person (`crate::ask`) and answered by question id |
//!
//! **The mode.** Every mode but `bypassPermissions` and `unhinged` runs Codex in its
//! `read-only` sandbox with approvals `on-request` and krowk as the
//! reviewer, so every edit and every command that needs more than reading
//! is asked about, and krowk's permission evaluator answers it — asking the
//! person when a client is attached; `bypassPermissions` and `unhinged`
//! are `danger-full-access` with no approvals. The mode
//! is named on `thread/start` and `thread/resume`, so a default in Codex's
//! config cannot loosen it, and a thread Codex reports in a looser sandbox,
//! or with another reviewer, is stopped before a turn runs. What Codex's
//! sandbox lets a command do without asking — read the disk, not write it
//! — and what the user's own Codex rules allow by themselves, is Codex's to
//! decide: the vendor behaving as its user set it up, as with every
//! backend. krowk's deny rules reach Codex only through what it asks, so a
//! `Read(.env)` deny does not stop a command reading the file inside
//! Codex's read-only sandbox; the OS sandbox (Harness ticket 26) is what
//! closes that.
//!
//! **Compliance** (R-BACK-3) is structural: krowk starts the binary as the
//! user installed it, as itself (client `krowk`), sends it no system
//! prompt, never uses Codex's OAuth client and never opens Codex's login
//! file — the login is Codex's, made by `codex login` and reported by
//! `codex login status` and `account/read` (`auth`). A compliance test
//! holds every source file and every run to that.

pub mod auth;
pub mod stream;

use crate::bridge::{self, BridgeEnv};
use crate::catalog::ModelInfo;
use crate::engine::{cancelled, BoxFuture, Engine, EngineError, EngineEvent, Events, Steer, TurnContext, TurnEnd};
use crate::instances::{Backend, Resolved};
use crate::permissions::{self, Access, Call, Gate, Verdict};
use crate::protocol::{Billing, Effort, Item, ItemKind, LimitState, LimitStatus, ModelRef, PermissionMode, Question, QuestionOption, WireApi};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stream::Translator;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// The binary a `codex-app-server` instance runs when its definition names none.
pub const BINARY: &str = "codex";
/// The backend's name in the log.
pub const BACKEND: &str = "codex-app-server";

/// How long the process has to answer `initialize`: a cold start, not a
/// binary that is not Codex.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `thread/start` may take: Codex starts the thread's MCP servers.
const THREAD_TIMEOUT: Duration = Duration::from_secs(120);
/// After an interrupt, how long the turn waits for Codex's `turn/completed`
/// before the process is stopped; the session continues on `thread/resume`.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);
/// How long a process asked to exit (stdin closed) gets.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// How long a stopped process group gets between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// After Codex exits, how long what it already wrote is read.
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// How often a running turn looks for steering to pass on.
const STEER_POLL: Duration = Duration::from_millis(100);
/// The end of stderr kept for a failure's words.
const STDERR_TAIL: usize = 4096;

/// What a turn's context record says of the system prompt: it is Codex's,
/// and krowk neither sees nor changes it.
pub const SYSTEM_NOTE: &str = "(Codex's own system prompt: krowk sends none and does not see it)";

/// The ambient variables a Codex process does not inherit unless its
/// instance names them (`env`, `apiKeyEnv`). Inherited, each would change
/// whose account, which server or whose state the instance runs on without
/// anyone asking for it: the native `openai` instance's key, base URL and
/// organization; Codex's own key, access token, workload identity and
/// connector tokens; the sign-in and token endpoints and the client Codex
/// signs in as; its originator; where it keeps its state database outside
/// `CODEX_HOME`; and the ids of a Codex session krowk itself may be running
/// inside. Read off the variables codex 0.154.0 names; everything else
/// (proxies, certificates, the sandbox's own) is the person's environment,
/// as when they start `codex` by hand.
pub const NOT_INHERITED: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
    "OPENAI_CLUSTER",
    "OPENAI_IDENTITY_TOKEN_FILE",
    "OPENAI_FEDERATION_RULE_ID",
    "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_CONNECTORS_TOKEN",
    "CODEX_GITHUB_PERSONAL_ACCESS_TOKEN",
    "CODEX_AUTHAPI_BASE_URL",
    "CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL",
    "CODEX_AGENT_IDENTITY_JWKS_BASE_URL",
    "CODEX_APP_SERVER_CHATGPT_BASE_URL",
    "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
    "CODEX_APP_SERVER_LOGIN_ISSUER",
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
    "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "CODEX_SQLITE_HOME",
    "CODEX_ROLLOUT_TRACE_ROOT",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
];

/// What of `NOT_INHERITED` to remove for this instance.
pub fn cleared(b: &Backend) -> impl Iterator<Item = &'static str> + '_ {
    NOT_INHERITED.iter().copied().filter(|k| !b.env.contains_key(*k) && b.key.as_ref().is_none_or(|(to, _)| to != k))
}

/// The environment a Codex command gets on top of krowk's own: the
/// inherited keys and base URL taken away, the home, the instance's `env`,
/// and its key under the name its model provider reads.
pub fn environment(b: &Backend) -> (Vec<&'static str>, Vec<(String, String)>) {
    let mut set: Vec<(String, String)> = Vec::new();
    if let Some(dir) = &b.config_dir {
        set.push(("CODEX_HOME".into(), dir.display().to_string()));
    }
    set.extend(b.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    if let Some((to, key)) = &b.key {
        set.push((to.clone(), key.clone()));
    }
    (cleared(b).collect(), set)
}

/// The whole argument list: `app-server` on stdio, then the instance's own.
pub fn args(extra: &[String]) -> Vec<String> {
    let mut a: Vec<String> = ["app-server", "--listen", "stdio://"].iter().map(|s| s.to_string()).collect();
    a.extend(extra.iter().cloned());
    a
}

/// How long the paste guard's trust is waited for: a Codex that stalls on
/// it starts without the guard rather than late.
const TRUST_TIMEOUT: Duration = Duration::from_secs(10);

/// The command Codex runs the paste guard (`crate::paste_guard`) with:
/// `exe` — this krowk — as `krowk __paste-guard`, quoted for the shell.
/// Linux names a binary replaced since it started `… (deleted)`; the hook
/// runs the one now at that path.
pub fn paste_guard_command(exe: &Path) -> String {
    let exe = exe.display().to_string();
    let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
    format!("{} {}", permissions::quote(exe), crate::paste_guard::HOOK_ARG)
}

/// The instance's arguments with the paste guard as a Codex `PreToolUse`
/// hook on its shell tool. Codex runs a hook under every approval policy
/// and sandbox, and the model reads its deny reason, where a declined
/// approval would tell it nothing. A `hooks.PreToolUse` the instance sets
/// itself is kept, and the guard added to it: a second `-c` would replace
/// it whole.
pub fn with_paste_guard(mut args: Vec<String>, command: &str) -> Vec<String> {
    // A JSON string is a TOML basic string, escapes and all.
    let entry = format!("{{matcher=\"^Bash$\",hooks=[{{type=\"command\",command={}}}]}}", Value::String(command.into()));
    // Codex keeps the last one. Only a one-line inline array without
    // comments is added to: anything else gets a `-c` of its own after it,
    // so Codex always starts, the guard on.
    let last = args.iter().rposition(|a| a.trim_start_matches("--config=").starts_with("hooks.PreToolUse="));
    let own = last.filter(|&i| {
        let v = args[i].trim_start_matches("--config=").trim_start_matches("hooks.PreToolUse=").trim();
        v.starts_with('[') && v.ends_with(']') && !v.contains(['#', '\n'])
    });
    match own {
        Some(i) => {
            let list = args[i].trim_end().strip_suffix(']').unwrap_or_default().trim_end().to_string();
            let sep = if list.ends_with('[') || list.ends_with(',') { "" } else { "," };
            args[i] = format!("{list}{sep}{entry}]");
        }
        None => args.extend(["-c".to_string(), format!("hooks.PreToolUse=[{entry}]")]),
    }
    args
}

/// Whether a hook `hooks/list` reports is the paste guard krowk passed:
/// its command exactly, from the command line.
fn is_paste_guard(hook: &Value, command: &str) -> bool {
    str_of(hook, "source") == "sessionFlags" && str_of(hook, "eventName") == "preToolUse" && str_of(hook, "command") == command
}

/// Codex's approval policy and sandbox for a krowk mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub approval: &'static str,
    pub sandbox: &'static str,
}

pub fn policy(mode: PermissionMode) -> Policy {
    match mode {
        PermissionMode::BypassPermissions | PermissionMode::Unhinged => Policy { approval: "never", sandbox: "danger-full-access" },
        _ => Policy { approval: "on-request", sandbox: "read-only" },
    }
}

/// How much a sandbox Codex reports lets through, against the one asked
/// for: read-only lets nothing be written, and anything this build does not
/// know is taken for full access.
fn sandbox_rank(kind: &str) -> u8 {
    match kind {
        "readOnly" | "read-only" => 0,
        "workspaceWrite" | "workspace-write" => 1,
        _ => 2,
    }
}

/// `initialize`'s parameters: krowk as itself, never as a Codex client.
pub fn initialize_params(krowk_version: &str) -> Value {
    json!({"clientInfo": {"name": "krowk", "title": "krowk", "version": krowk_version}, "capabilities": {"experimentalApi": true}})
}

/// krowk's tools, as the thread's dynamic tools: one `krowk` namespace
/// holding every tool the bridge exposes.
pub fn dynamic_tools() -> Value {
    let tools: Vec<Value> = bridge::EXPOSED.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "inputSchema": (t.input_schema)()})).collect();
    json!([{"type": "namespace", "name": bridge::SERVER, "description": "krowk's own tools: what krowk knows about this session, and what it can do beyond this agent.", "tools": tools}])
}

/// The effort a turn is sent: the rung asked for, mapped onto the ones
/// Codex's `model/list` says the model takes — else the catalog's or the
/// family's — else sent as it is, for Codex to judge.
pub fn effort_for(want: Option<Effort>, codex: Option<&[String]>, info: Option<&ModelInfo>, model: &str) -> Option<String> {
    let want = want?;
    let takes: Vec<Effort> = match codex {
        Some(listed) => listed.iter().filter_map(|e| Effort::parse(e)).collect(),
        None => crate::effort::supported(info, crate::toolset::family_from_id(model)),
    };
    if takes.is_empty() {
        return Some(want.name().into());
    }
    crate::effort::map(want, &takes).map(|e| e.name().to_string())
}

/// A command Codex asks to run outside its read-only sandbox, as krowk's
/// permission evaluator judges it: `Bash(<command>)`, so the rules written
/// for Claude Code hold for Codex's commands too.
pub fn command_call(command: &str) -> Call {
    Call { tool: "Bash".into(), access: Access::Bash(command.to_string()), subject: None }
}

/// A patch Codex asks to apply, by every file it names (each change's path
/// and a move's destination, `stream::change_paths`): an `Edit` of them. A
/// patch whose files krowk did not see, or that asks to write anywhere
/// under a root for the rest of the session, is refused before it is
/// judged: krowk judges each change by its files.
pub fn file_change_call(paths: Option<&[String]>, grant_root: Option<&str>, cwd: &Path) -> Result<Call, String> {
    if let Some(root) = grant_root.filter(|r| !r.is_empty()) {
        return Err(format!("krowk declined the change: it asked to write anywhere under {root} for the rest of the session, and krowk approves each change by its files"));
    }
    let paths = paths.filter(|p| !p.is_empty()).ok_or("krowk declined the change: it did not see which files the patch changes")?;
    let at = |p: &String| {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
    };
    Ok(Call { tool: "Edit".into(), access: Access::Edit(paths.iter().map(at).collect()), subject: None })
}

/// What a Codex account's home shares with the person's own Codex home:
/// the configuration — settings, instructions, prompts, skills, rules —
/// never the login, the threads or Codex's state. Each is a symlink, so an
/// edit in one shows in every account — and a write Codex makes to its
/// config (`config/value/write`, a project it is told to trust) lands in
/// the person's own `config.toml`, which is the point of sharing it.
pub const SHARED: &[&str] = &["config.toml", "AGENTS.md", "AGENTS.override.md", "prompts", "skills", "rules"];

/// What of the person's `skills` is not shared: Codex installs its own
/// bundled skills into `skills/.system` of whatever home it runs in, and
/// through a linked `skills` it would write them into the person's.
const OWN_SKILLS: &[&str] = &[".system"];

/// Removes `to` when it is this module's link to `from` and `from` is gone.
#[cfg(unix)]
fn prune(to: &Path, from: &Path) -> std::io::Result<()> {
    let ours = to.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) && std::fs::read_link(to).ok().as_deref() == Some(from);
    if ours && from.symlink_metadata().is_err() {
        std::fs::remove_file(to)?;
    }
    Ok(())
}

#[cfg(unix)]
fn link(from: &Path, to: &Path) -> std::io::Result<bool> {
    match to.symlink_metadata() {
        Err(_) => {
            std::os::unix::fs::symlink(from, to)?;
            Ok(true)
        }
        // Linked here already: still shared. Anything else is the account's own.
        Ok(m) => Ok(m.file_type().is_symlink() && std::fs::read_link(to).ok().as_deref() == Some(from)),
    }
}

/// Links what `own` has of `SHARED` into the account's `home`, leaving
/// alone anything `home` already has, and says what `home` shares: an
/// account's own login stays its own (R-INST-1's per-account home). `skills`
/// is a directory of the account's own holding a link per skill, so what
/// Codex writes there itself stays in the account. A link this made whose
/// target is gone — a skill or a file the person removed — is removed with
/// it. Safe to run again at any time: it runs when the account is added and
/// before each process starts, so a skill added later reaches every
/// account. Nothing is linked when the two are one directory, and nothing
/// at all off unix.
pub fn share(home: &Path, own: &Path) -> std::io::Result<Vec<String>> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canon(home) == canon(own) {
        return Ok(Vec::new());
    }
    let mut shared = Vec::new();
    #[cfg(unix)]
    for name in SHARED {
        let (from, to) = (own.join(name), home.join(name));
        if from.symlink_metadata().is_err() {
            prune(&to, &from)?;
            continue;
        }
        if *name == "skills" && from.is_dir() {
            // An account added before skills had a directory of its own
            // linked the whole of them: that link is replaced.
            if to.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) && std::fs::read_link(&to).ok().as_deref() == Some(from.as_path()) {
                std::fs::remove_file(&to)?;
            }
            if to.symlink_metadata().is_err() {
                std::fs::create_dir(&to)?;
            }
            if !to.symlink_metadata()?.is_dir() {
                continue;
            }
            let mut any = false;
            for e in std::fs::read_dir(&to)?.flatten() {
                prune(&e.path(), &from.join(e.file_name()))?;
            }
            for e in std::fs::read_dir(&from)?.flatten() {
                if OWN_SKILLS.iter().any(|o| e.file_name() == *o) {
                    continue;
                }
                any |= link(&e.path(), &to.join(e.file_name()))?;
            }
            if any {
                shared.push(name.to_string());
            }
            continue;
        }
        if link(&from, &to)? {
            shared.push(name.to_string());
        }
    }
    Ok(shared)
}

/// The thread config that turns off every MCP server `config/read` names:
/// `{"mcp_servers": {"<name>": {"enabled": false}}}`, none when it names none.
pub fn mcp_off(config_read: &Value) -> Option<Value> {
    let servers = config_read.pointer("/config/mcp_servers").and_then(Value::as_object).filter(|m| !m.is_empty())?;
    let off: serde_json::Map<String, Value> = servers.keys().map(|k| (k.clone(), json!({"enabled": false}))).collect();
    Some(json!({ "mcp_servers": off }))
}

/// A JSON-RPC message from Codex.
#[derive(Debug)]
enum Msg {
    Response { id: Value, result: Result<Value, String> },
    Request { id: Value, method: String, params: Value },
    Note { method: String, params: Value },
}

fn classify(mut v: Value) -> Option<Msg> {
    let method = v.get("method").and_then(Value::as_str).map(String::from);
    let params = v.get_mut("params").map(Value::take).unwrap_or(Value::Null);
    match (method, v.get("id").cloned()) {
        (Some(method), Some(id)) => Some(Msg::Request { id, method, params }),
        (Some(method), None) => Some(Msg::Note { method, params }),
        (None, Some(id)) => {
            let result = match v.get("error") {
                Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("no reason given").to_string()),
                None => Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
            };
            Some(Msg::Response { id, result })
        }
        (None, None) => None,
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// The fix for an instance with no login.
fn sign_in_fix(instance: &str) -> String {
    crate::connect::connect_command(instance, "codex-app-server")
}

/// A turn Codex ended in failure, as krowk's error. A login gone stale is
/// named as one, with the command that renews it.
fn failure(err: &Value, instance: &str) -> EngineError {
    let message = err.get("message").and_then(Value::as_str).unwrap_or("no reason given").trim().to_string();
    let info = err.get("codexErrorInfo").cloned().unwrap_or(Value::Null);
    let kind = info.as_str().map(String::from).or_else(|| info.as_object().and_then(|o| o.keys().next().cloned())).unwrap_or_default();
    let status = info.as_object().and_then(|o| o.values().next()).and_then(|v| v.get("httpStatusCode")).and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()).unwrap_or(0);
    if kind == "unauthorized" || status == 401 || message.contains("401 Unauthorized") {
        return EngineError::new("not_authenticated", format!("Codex is not signed in for the {instance} instance ({message}) — sign in with `{}`, which runs Codex's own login", sign_in_fix(instance))).with_status(401);
    }
    match kind.as_str() {
        "usageLimitExceeded" => EngineError::new("usage_limit", format!("the account behind {instance} has reached its Codex usage limit: {message}")).with_status(status),
        "contextWindowExceeded" => EngineError::new("context_window_exceeded", format!("the conversation no longer fits the model's context window: {message}")),
        _ => EngineError::new("backend_failed", format!("Codex could not finish the turn: {message}")).with_status(status),
    }
}

/// A rate-limit snapshot (`account/rateLimits/updated`'s `rateLimits`) as
/// krowk's limit status (R-INST-6): the fuller of its two windows, limited
/// when Codex says a limit was reached, a warning from 80% used.
pub fn limit_of(snapshot: &Value) -> Option<LimitStatus> {
    let window = |k: &str| snapshot.get(k).filter(|w| w.is_object());
    let used = |w: &Value| w.get("usedPercent").and_then(Value::as_f64).unwrap_or(0.0);
    let w = match (window("primary"), window("secondary")) {
        (Some(p), Some(s)) => Some(if used(s) > used(p) { s } else { p }),
        (p, s) => p.or(s),
    };
    let reached = snapshot.get("rateLimitReachedType").is_some_and(|r| !r.is_null());
    if w.is_none() && !reached {
        return None;
    }
    let used_percent = w.map(used);
    let status = if reached || used_percent.is_some_and(|u| u >= 100.0) {
        LimitState::Limited
    } else if used_percent.is_some_and(|u| u >= 80.0) {
        LimitState::Warning
    } else {
        LimitState::Allowed
    };
    Some(LimitStatus {
        status,
        window: w.and_then(|w| w.get("windowDurationMins")).and_then(Value::as_i64).map(|m| format!("{m}m")),
        used_percent,
        resets_at_ms: w.and_then(|w| w.get("resetsAt")).and_then(Value::as_i64).map(|s| s * 1000),
    })
}

/// The engine for one session on one `codex-app-server` instance. The host
/// keeps it between turns, so the process it starts serves the whole
/// session.
pub struct CodexEngine {
    instance: Resolved,
    krowk_version: String,
    proc: tokio::sync::Mutex<Option<Proc>>,
}

impl CodexEngine {
    pub fn new(instance: Resolved, krowk_version: &str) -> Result<CodexEngine, EngineError> {
        if instance.backend.is_none() {
            return Err(EngineError::new("bad_config", format!("{} is not a Codex instance", instance.name)));
        }
        Ok(CodexEngine { instance, krowk_version: krowk_version.into(), proc: tokio::sync::Mutex::new(None) })
    }

    fn backend(&self) -> &Backend {
        self.instance.backend.as_ref().expect("checked in new")
    }
}

impl Engine for CodexEngine {
    fn provider(&self) -> &str {
        &self.instance.provider
    }

    fn wire_api(&self) -> WireApi {
        WireApi::CodexAppServer
    }

    fn run_turn<'a>(&'a self, mut ctx: TurnContext, events: Events) -> BoxFuture<'a, Result<TurnEnd, EngineError>> {
        Box::pin(async move {
            let mut slot = self.proc.lock().await;
            let bypass = ctx.permission_mode.asks_nothing();
            let ask = Answers {
                session_id: ctx.session_id.clone(),
                turn_id: ctx.turn_id.clone(),
                model: ctx.model.clone(),
                cwd: ctx.cwd.clone(),
                krowk_version: self.krowk_version.clone(),
                evidence: ctx.evidence.clone(),
                gate: ctx.gate.protecting(self.backend().home.iter().chain(&self.backend().shared_home).cloned()),
            };
            // The running process serves this turn when it is alive and in
            // the same sandbox; another mode is another thread setting,
            // taken by a new process on `thread/resume`.
            if let Some(p) = slot.as_mut()
                && (!p.alive() || p.bypass != bypass)
                && let Some(old) = slot.take()
            {
                old.shutdown().await;
            }
            if slot.is_none() {
                // What the person added to, or removed from, their own Codex
                // configuration since the account was set up reaches it now.
                // A link that cannot be made is no reason not to run.
                if let (Some(home), Some(own)) = (&self.backend().config_dir, &self.backend().shared_home) {
                    let _ = share(home, own);
                }
                *slot = Some(Proc::spawn(self.backend(), &ctx.cwd, bypass, &ask, &self.instance.name).await?);
            }
            let p = slot.as_mut().expect("spawned above");
            let outcome = async {
                // The thread: the one this process has open, else the
                // session's to resume — by the path it was copied to, when
                // it came from another account (R-INST-4) — else a new one.
                let want = ctx.backend_session.clone();
                let path = ctx.handoff.as_ref().and_then(|h| h.carried_to.as_ref()).map(|p| p.display().to_string());
                let mut resumed = p.thread.is_some() && (want.is_none() || want == p.thread);
                let mut fell_back = None;
                if !resumed {
                    match p.open_thread(want.as_deref(), path.as_deref(), &ctx, &ask, &self.instance.name).await {
                        Ok(()) => resumed = want.is_some(),
                        // A thread Codex cannot resume is no reason to lose
                        // the session: the log has it, and a new thread is
                        // seeded from it (R-SWITCH-4).
                        Err(e) if e.code == "backend_resume_failed" && ctx.handoff.is_some() => {
                            fell_back = Some(e.message);
                            p.open_thread(None, None, &ctx, &ask, &self.instance.name).await?;
                        }
                        Err(e) => return Err(e),
                    }
                }
                let (prompt, images) = match ctx.history.last().map(|h| &h.item) {
                    Some(Item::UserText { text, images }) => (text.clone(), images.clone()),
                    _ => return Err(EngineError::new("empty_prompt", "a backend turn needs the prompt as its last item")),
                };
                // What the thread did not run goes ahead of the prompt, in
                // the same input (R-SWITCH-2); what was sent is reported.
                let (prompt, handoff) = crate::handoff::Handoff::opening(ctx.handoff.as_ref(), &prompt, resumed, fell_back);
                if let Some(ev) = handoff {
                    let _ = events.send(ev).await;
                }
                p.turn(Steer { text: prompt, images, from_krowk: false }, &mut ctx, &ask, &events, &self.instance.name).await
            }
            .await;
            // A process that died, or a turn that failed partway, is not
            // trusted with the next turn: that one starts clean on resume.
            if (outcome.is_err() || !p.alive())
                && let Some(old) = slot.take()
            {
                old.kill().await;
            }
            outcome
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(p) = self.proc.lock().await.take() {
                p.shutdown().await;
            }
        })
    }
}

/// What a request from Codex is answered with: the turn it arrived in.
struct Answers {
    session_id: String,
    turn_id: String,
    model: ModelRef,
    cwd: PathBuf,
    krowk_version: String,
    /// The turn's permissions, with the instance's home kept from edits too.
    gate: Gate,
    /// Where a bridged `publish` sends files.
    evidence: Option<crate::evidence::Evidence>,
}

/// Where a question for a person goes while a turn runs; outside one
/// (while a thread opens) nobody is asked.
type Asking<'a> = Option<(&'a Events, &'a tokio::sync::watch::Receiver<bool>)>;

impl Answers {
    /// Judges a call, asking a person when the verdict says to and a turn
    /// is there to ask through.
    async fn permit(&self, call: Result<Call, String>, tool: &str, input: &Value, asking: Asking<'_>) -> Result<(), String> {
        let call = call?;
        let r = match asking {
            Some((events, cancel)) => self.gate.check(&call, tool, input, None, events, cancel).await.map(drop),
            None => match self.gate.verdict(&call, None) {
                Verdict::Allow(_) => Ok(()),
                Verdict::Deny(m) => Err(m),
                Verdict::Ask { reason, .. } => Err(format!("{} needs approval — {reason} — and Codex asked outside a turn, where nobody can be asked", permissions::summary(&call))),
            },
        };
        r.map_err(|e| if e.starts_with("krowk declined") { e } else { format!("krowk declined it: {e}") })
    }

    /// `item/tool/requestUserInput`, asked of the person: each question's
    /// answers by its id — the option picked, or what the person wrote,
    /// or both, the writing as Codex's own `user_note:`. Nobody to ask, a
    /// decline or an interrupt is said to the model as every question's
    /// answer, so it goes on knowing why.
    async fn ask_user(&self, params: &Value, asking: Asking<'_>) -> Value {
        let questions: Vec<Question> = params
            .get("questions")
            .and_then(Value::as_array)
            .map(|qs| {
                qs.iter()
                    .map(|q| Question {
                        id: str_of(q, "id").to_string(),
                        header: str_of(q, "header").to_string(),
                        question: str_of(q, "question").to_string(),
                        options: q.get("options").and_then(Value::as_array).map(|os| os.iter().map(|o| QuestionOption { label: str_of(o, "label").to_string(), description: str_of(o, "description").to_string() }).collect()).unwrap_or_default(),
                        multi_select: false,
                        secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        // A deny rule on Claude Code's name for it holds, as it does for
        // the native tool and Claude Code's own.
        let call = Call { tool: "AskUserQuestion".into(), access: Access::Session, subject: None };
        let answered = match (self.gate.verdict(&call, None), asking) {
            (Verdict::Deny(m), _) => Err(m),
            (_, Some((events, cancel))) if !questions.is_empty() => self.gate.ask("request_user_input", params, questions.clone(), events, cancel).await,
            _ => Err(permissions::NOBODY_TO_ASK.into()),
        };
        // Answered with nothing — a client that sent no answers — is a decline.
        let answered = answered.and_then(|a| if questions.iter().any(|q| a.iter().find(|a| a.id == q.id).and_then(crate::ask::said).is_some()) { Ok(a) } else { Err(permissions::DECLINED.into()) });
        let answers: serde_json::Map<String, Value> = questions
            .iter()
            .map(|q| {
                let said: Vec<String> = match &answered {
                    Ok(answers) => answers.iter().find(|a| a.id == q.id).map(|a| a.picked.iter().cloned().chain(a.text.as_ref().map(|t| if a.picked.is_empty() { t.clone() } else { format!("user_note: {t}") })).collect()).unwrap_or_default(),
                    Err(why) => vec![why.clone()],
                };
                (q.id.clone(), json!({ "answers": said }))
            })
            .collect();
        json!({ "answers": answers })
    }

    /// The answer to one of Codex's requests: a result, or a JSON-RPC error.
    async fn answer(&self, method: &str, params: &Value, t: &mut Translator, asking: Asking<'_>) -> Result<Value, (i64, String)> {
        let item = str_of(params, "itemId").to_string();
        // A decline's reason is kept for the call's result: Codex tells the
        // model only that it was declined.
        let decide = |t: &mut Translator, verdict: Result<(), String>| -> Value {
            match verdict {
                Ok(()) => json!({"decision": "accept"}),
                Err(why) => {
                    t.declined.insert(item.clone(), why);
                    json!({"decision": "decline"})
                }
            }
        };
        match method {
            "item/commandExecution/requestApproval" => {
                let command = str_of(params, "command").to_string();
                let verdict = self.permit(Ok(command_call(&command)), "shell", params, asking).await;
                Ok(decide(t, verdict))
            }
            "item/fileChange/requestApproval" => {
                let paths = t.changes.get(&item).cloned();
                let call = file_change_call(paths.as_deref(), params.get("grantRoot").and_then(Value::as_str), &self.cwd);
                let verdict = self.permit(call, "apply_patch", params, asking).await;
                Ok(decide(t, verdict))
            }
            // More sandbox for the rest of the turn: none is granted — what
            // needs it is asked about call by call.
            "item/permissions/requestApproval" => Ok(json!({"permissions": {}, "scope": "turn"})),
            "item/tool/requestUserInput" => Ok(self.ask_user(params, asking).await),
            "mcpServer/elicitation/request" => Ok(json!({"action": "decline"})),
            "item/tool/call" => Ok(self.tool_call(params, asking).await),
            // The requests of Codex's first protocol, for a Codex that
            // still sends them.
            "applyPatchApproval" => {
                let paths = params.get("fileChanges").map(stream::change_paths).unwrap_or_default();
                let call = file_change_call(Some(&paths), params.get("grantRoot").and_then(Value::as_str), &self.cwd);
                let verdict = self.permit(call, "apply_patch", params, asking).await;
                Ok(json!({"decision": if verdict.is_ok() { "approved" } else { "denied" }}))
            }
            "execCommandApproval" => {
                let command = params.get("command").map(|c| match c {
                    Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
                    v => v.as_str().unwrap_or_default().to_string(),
                });
                let verdict = self.permit(Ok(command_call(&command.unwrap_or_default())), "shell", params, asking).await;
                Ok(json!({"decision": if verdict.is_ok() { "approved" } else { "denied" }}))
            }
            m => Err((-32601, format!("krowk does not answer {m:?}"))),
        }
    }

    /// A call of one of krowk's tools, run by the bridge as a `tools/call`.
    async fn tool_call(&self, params: &Value, asking: Asking<'_>) -> Value {
        let namespace = params.get("namespace").and_then(Value::as_str).unwrap_or_default();
        let tool = str_of(params, "tool");
        if namespace != bridge::SERVER {
            return json!({"contentItems": [{"type": "inputText", "text": format!("krowk has no tool {tool:?} in the namespace {namespace:?} — its tools are in {:?}", bridge::SERVER)}], "success": false});
        }
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
        let call = json!({"jsonrpc": "2.0", "id": 0, "method": "tools/call", "params": {"name": tool, "arguments": params.get("arguments").cloned().unwrap_or(Value::Null)}});
        let answer = bridge::handle(&call, &env).await;
        let text = answer.pointer("/result/content").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")).unwrap_or_default();
        let failed = answer.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(true);
        json!({"contentItems": [{"type": "inputText", "text": text}], "success": !failed})
    }
}

/// What a request krowk sent during a turn was.
enum Pending {
    TurnStart,
    Steer(Steer),
    Interrupt,
}

/// One running `codex app-server`.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    out: LineReader<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<String>>,
    binary: String,
    /// Started in `danger-full-access` (krowk's bypassPermissions or unhinged).
    bypass: bool,
    next: u64,
    exited: bool,
    /// The process group: Codex and whatever it started.
    pid: Option<u32>,
    /// The group was stopped and reaped; nothing is left to signal.
    group_done: bool,
    /// What `account/read` said the instance is billed to.
    billing: Option<Billing>,
    /// The efforts each model takes, from `model/list`.
    efforts: HashMap<String, Vec<String>>,
    /// The thread this process has open, and its transcript.
    thread: Option<String>,
    transcript: Option<String>,
}

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

/// The longest line read from Codex: a resumed thread or a big tool output
/// is megabytes; a line past this is skipped whole rather than held.
const MAX_LINE: usize = 64 << 20;

/// Raw lines from a stream, at most `cap` bytes each. Its partial line
/// lives in the reader, not in the future reading it, so a read cancelled
/// in a `select!` loses nothing.
struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
    /// The line being read is longer than the cap, and is being skipped.
    over: bool,
    cap: usize,
}

/// One line, or a line too long to keep.
#[derive(Debug, PartialEq)]
enum Line {
    Bytes(Vec<u8>),
    TooLong,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    fn new(inner: R) -> LineReader<R> {
        LineReader { inner, buf: Vec::new(), over: false, cap: MAX_LINE }
    }

    /// The next line without its newline, or none at the end of the stream.
    async fn next(&mut self) -> Option<Line> {
        loop {
            let avail = self.inner.fill_buf().await.ok()?;
            if avail.is_empty() {
                // The end: a last line with no newline is still a line.
                if self.buf.is_empty() && !self.over {
                    return None;
                }
                return Some(self.take());
            }
            let (chunk, done) = match avail.iter().position(|b| *b == b'\n') {
                Some(i) => (i, true),
                None => (avail.len(), false),
            };
            if !self.over && self.buf.len() + chunk <= self.cap {
                self.buf.extend_from_slice(&avail[..chunk]);
            } else {
                self.over = true;
                self.buf.clear();
            }
            self.inner.consume(if done { chunk + 1 } else { chunk });
            if done {
                return Some(self.take());
            }
        }
    }

    fn take(&mut self) -> Line {
        let over = std::mem::take(&mut self.over);
        let buf = std::mem::take(&mut self.buf);
        if over { Line::TooLong } else { Line::Bytes(buf) }
    }
}

/// The next JSON-RPC message, or none at the end of the stream. A line that
/// is not JSON — a warning a wrapper printed, bytes that are not UTF-8, a
/// line past `MAX_LINE` — is skipped, never the end of the session.
async fn next_msg<R: AsyncBufRead + Unpin>(out: &mut LineReader<R>) -> Option<Msg> {
    loop {
        match out.next().await? {
            Line::Bytes(l) => {
                if let Some(m) = serde_json::from_slice::<Value>(&l).ok().and_then(classify) {
                    return Some(m);
                }
            }
            Line::TooLong => {}
        }
    }
}

fn tail(s: &str) -> String {
    let s = s.trim();
    let cut = s.char_indices().rev().nth(STDERR_TAIL).map(|(i, _)| i).unwrap_or(0);
    s[cut..].to_string()
}

/// A notification of Codex's during a turn on `thread`.
async fn on_note(thread: &str, method: &str, params: &Value, (subs, t): (&mut stream::SubThreads, &mut Translator), (codex_turn, completed): (&mut Option<String>, &mut Option<Value>), limit: &mut Option<LimitStatus>, events: &Events) {
    // Another thread's — a subagent Codex runs — is
    // its own conversation, which Codex keeps; what
    // its calls cost is the session's spend.
    if method == "thread/started" || params.get("threadId").and_then(Value::as_str).is_some_and(|th| th != thread) {
        forward(events, subs.apply(thread, method, params).into_iter().collect()).await;
        return;
    }
    let of_turn = params.get("turnId").and_then(Value::as_str).or_else(|| params.pointer("/turn/id").and_then(Value::as_str));
    if let (Some(mine), Some(theirs)) = (&*codex_turn, of_turn)
        && mine != theirs
    {
        return;
    }
    match method {
        "turn/started" if codex_turn.is_none() => *codex_turn = of_turn.map(String::from),
        // How near the account is to its limit (R-INST-6).
        "account/rateLimits/updated" => {
            if let Some(l) = params.get("rateLimits").and_then(limit_of) {
                *limit = Some(l.clone());
                let _ = events.send(EngineEvent::Limits(l)).await;
            }
        }
        "turn/completed" => *completed = Some(params.get("turn").cloned().unwrap_or(Value::Null)),
        _ => forward(events, t.apply(method, params)).await,
    }
}

/// How krowk's turn ends with the Codex turn that completed, when it does:
/// interrupted, or failed for Codex's reason.
async fn concluded(turn: &Value, interrupted: bool, t: &mut Translator, events: &Events, limit: Option<&LimitStatus>, instance: &str) -> Option<Result<TurnEnd, EngineError>> {
    match str_of(turn, "status") {
        _ if interrupted => {
            finish(t, events).await;
            Some(Ok(TurnEnd::Interrupted))
        }
        "interrupted" => {
            finish(t, events).await;
            Some(Ok(TurnEnd::Interrupted))
        }
        "failed" => {
            finish(t, events).await;
            let err = turn.get("error").filter(|e| !e.is_null()).cloned().or_else(|| t.error.clone()).unwrap_or_else(|| json!({"message": "no reason given"}));
            let e = failure(&err, instance);
            // A limit says when it lifts, when Codex said.
            Some(Err(if e.limited() { e.with_resets(limit.and_then(|l| l.resets_at_ms)) } else { e }))
        }
        _ => None,
    }
}

/// What the next Codex turn in krowk's reads, once Codex finished one:
/// the steering it refused, and any that arrived since, each logged where
/// it landed. None when nothing is waiting, and krowk's turn ends.
async fn more_input(refused: Vec<Steer>, steers: &crate::engine::Steers, t: &mut Translator, events: &Events) -> Option<Vec<Steer>> {
    let mut more = refused;
    more.extend(steers.take());
    if more.is_empty() {
        if steers.close_if_empty() {
            finish(t, events).await;
            return None;
        }
        more = steers.take();
    }
    for steer in &more {
        steered(events, steer.clone()).await;
    }
    Some(more)
}

/// Steering Codex answered: taken into the running turn, it is logged
/// where it landed; refused, as the turn was ending, it is the next Codex
/// turn's input instead.
async fn steer_answered(taken: bool, steer: Steer, refused: &mut Vec<Steer>, events: &Events) {
    if taken {
        steered(events, steer).await;
    } else {
        refused.push(steer);
    }
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

/// A steer Codex took, as the `userText` item the log shows it as, where it
/// landed.
async fn steered(events: &Events, steer: Steer) {
    let id = krowk_store::new_id();
    let _ = events.send(EngineEvent::ItemStarted { item_id: id.clone(), kind: ItemKind::UserText }).await;
    let _ = events.send(EngineEvent::ItemCompleted { item_id: id, item: steer.item() }).await;
}

/// `turn/start`'s params.
fn turn_params(thread: &str, input: &[Steer], session_dir: &Path, model: &str, effort: Option<&String>) -> Value {
    let mut params = json!({"threadId": thread, "input": text_input(input, session_dir), "model": model});
    if let Some(e) = effort {
        params["effort"] = json!(e);
    }
    params
}

/// A turn's input: each text, and after it the images it names, as files
/// Codex reads itself, each labelled with its `[Image #N]`.
fn text_input(input: &[Steer], session_dir: &Path) -> Value {
    let text = |t: &str| json!({"type": "text", "text": t, "text_elements": []});
    let mut out = Vec::new();
    for s in input {
        out.push(text(&s.text));
        for r in &s.images {
            match crate::images::path(session_dir, r).filter(|p| p.is_file()) {
                Some(p) => {
                    out.push(text(&crate::images::label(r, None)));
                    out.push(json!({"type": "localImage", "path": p}));
                }
                None => out.push(text(&crate::images::gone(r))),
            }
        }
    }
    Value::Array(out)
}

impl Proc {
    async fn spawn(b: &Backend, cwd: &Path, bypass: bool, ask: &Answers, instance: &str) -> Result<Proc, EngineError> {
        let mut cmd = tokio::process::Command::new(b.path.as_deref().unwrap_or(Path::new(&b.binary)));
        // Unix only: how Codex runs a hook's command on Windows is not
        // known here, so it is not guessed at.
        let guard = std::env::current_exe().ok().filter(|_| cfg!(unix)).map(|exe| paste_guard_command(&exe));
        let argv = match &guard {
            Some(command) => args(&with_paste_guard(b.args.clone(), command)),
            None => args(&b.args),
        };
        cmd.args(argv).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let (remove, set) = environment(b);
        for k in remove {
            cmd.env_remove(k);
        }
        cmd.envs(set);
        // Its own process group: a Ctrl-C at the terminal is krowk's to turn
        // into an interrupt, not a signal that kills Codex mid-write.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::new("backend_not_found", format!("{} was not found — install Codex (https://developers.openai.com/codex), or name the binary with `krowk connect openai --method subscription --binary <path>`", b.binary))
            } else {
                EngineError::new("backend_failed", format!("{} could not be started: {e}", b.binary))
            }
        })?;
        let pid = child.id();
        crate::group::register(pid);
        let stdin = child.stdin.take();
        let out = LineReader::new(BufReader::new(child.stdout.take().expect("piped")));
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
        let mut p = Proc { child, stdin, out, stderr, binary: b.binary.clone(), bypass, next: 0, exited: false, pid, group_done: false, billing: None, efforts: HashMap::new(), thread: None, transcript: None };
        p.request("initialize", initialize_params(&ask.krowk_version), ask, INITIALIZE_TIMEOUT).await?;
        p.notify("initialized").await?;
        // Signed in, and to what, asked before a thread exists for a turn
        // that could not run.
        let account = p.request("account/read", json!({"refreshToken": false}), ask, INITIALIZE_TIMEOUT).await?;
        p.billing = match account.pointer("/account/type").and_then(Value::as_str) {
            Some("chatgpt") => Some(Billing::Subscription),
            Some(_) => Some(Billing::ApiKey),
            None => None,
        };
        if account.get("account").is_none_or(Value::is_null) && account.get("requiresOpenaiAuth").and_then(Value::as_bool) == Some(true) {
            p.terminate().await;
            return Err(EngineError::new("not_authenticated", format!("Codex is not signed in for the {instance} instance — sign in with `{}`, which runs Codex's own login", sign_in_fix(instance))).with_status(401));
        }
        // What each model takes; a Codex that cannot list them still runs.
        if let Ok(list) = p.request("model/list", json!({}), ask, INITIALIZE_TIMEOUT).await {
            for m in list.get("data").and_then(Value::as_array).into_iter().flatten() {
                let efforts: Vec<String> = m.get("supportedReasoningEfforts").and_then(Value::as_array).map(|a| a.iter().map(|e| str_of(e, "reasoningEffort").to_string()).collect()).unwrap_or_default();
                for key in [str_of(m, "id"), str_of(m, "model")] {
                    if !key.is_empty() {
                        p.efforts.insert(key.to_string(), efforts.clone());
                    }
                }
            }
        }
        if let Some(command) = &guard {
            p.trust_paste_guard(cwd, command, ask).await;
        }
        Ok(p)
    }

    /// Codex runs a hook only once its user has trusted it, by its hash.
    /// The paste guard krowk passed is trusted here, by Codex's own config
    /// write, so it holds from the first thread. A Codex that lists no
    /// hooks runs without it.
    async fn trust_paste_guard(&mut self, cwd: &Path, command: &str, ask: &Answers) {
        let Ok(list) = self.request("hooks/list", json!({ "cwds": [cwd] }), ask, TRUST_TIMEOUT).await else { return };
        let hooks = list.get("data").and_then(Value::as_array).into_iter().flatten().flat_map(|d| d.get("hooks").and_then(Value::as_array).into_iter().flatten());
        let state: serde_json::Map<String, Value> = hooks
            .filter(|h| is_paste_guard(h, command) && !matches!(str_of(h, "trustStatus"), "trusted" | "managed"))
            .map(|h| (str_of(h, "key").to_string(), json!({ "trusted_hash": str_of(h, "currentHash") })))
            .collect();
        if !state.is_empty() {
            let _ = self.request("config/value/write", json!({"keyPath": "hooks.state", "mergeStrategy": "upsert", "value": state}), ask, TRUST_TIMEOUT).await;
        }
    }

    fn alive(&mut self) -> bool {
        !self.exited && matches!(self.child.try_wait(), Ok(None))
    }

    /// How a turn ends when Codex exits during it: an interrupted turn
    /// keeps what it made, any other fails.
    fn died_in_turn(&mut self, interrupted: bool) -> Result<TurnEnd, EngineError> {
        let e = self.died("before the turn finished");
        if interrupted { Ok(TurnEnd::Interrupted) } else { Err(e) }
    }

    fn died(&mut self, while_doing: &str) -> EngineError {
        self.exited = true;
        let status = self.child.try_wait().ok().flatten().map(|s| format!(" ({s})")).unwrap_or_default();
        let said = tail(&self.stderr.lock().unwrap_or_else(|e| e.into_inner()));
        let said = if said.is_empty() { String::new() } else { format!(": {said}") };
        EngineError::new("backend_exited", format!("codex app-server exited{status} {while_doing}{said} — the session is kept; continue it with --resume"))
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

    /// Sends a request and returns its id.
    async fn call(&mut self, method: &str, params: Value) -> Result<u64, EngineError> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"id": id, "method": method, "params": params})).await?;
        Ok(id)
    }

    async fn notify(&mut self, method: &str) -> Result<(), EngineError> {
        self.send(&json!({ "method": method })).await
    }

    async fn reply(&mut self, id: Value, answer: Result<Value, (i64, String)>) -> Result<(), EngineError> {
        match answer {
            Ok(result) => self.send(&json!({"id": id, "result": result})).await,
            Err((code, message)) => self.send(&json!({"id": id, "error": {"code": code, "message": message}})).await,
        }
    }

    /// Sends a request and waits for its answer, answering Codex's own
    /// requests meanwhile. Notifications before a turn are not the turn's.
    async fn request(&mut self, method: &str, params: Value, ask: &Answers, within: Duration) -> Result<Value, EngineError> {
        let id = self.call(method, params).await?;
        let mut scratch = Translator::default();
        let wait = async {
            loop {
                let Some(msg) = next_msg(&mut self.out).await else { return Err(self.died(&format!("before it answered {method}"))) };
                match msg {
                    Msg::Request { id, method, params } => {
                        let answer = ask.answer(&method, &params, &mut scratch, None).await;
                        self.reply(id, answer).await?;
                    }
                    Msg::Response { id: got, result } if got.as_u64() == Some(id) => {
                        return result.map_err(|e| EngineError::new("backend_failed", format!("Codex refused {method}: {e}")));
                    }
                    _ => {}
                }
            }
        };
        match tokio::time::timeout(within, wait).await {
            Ok(r) => r,
            Err(_) => {
                self.terminate().await;
                Err(EngineError::new("backend_unresponsive", format!("{} did not answer {method} within {} seconds — is it Codex?", self.binary, within.as_secs())))
            }
        }
    }

    /// Starts the session's thread, or resumes the one the log names, in
    /// krowk's mode and with krowk's tools; a thread Codex reports in a
    /// looser sandbox, or with another reviewer than krowk, is refused.
    async fn open_thread(&mut self, resume: Option<&str>, path: Option<&str>, ctx: &TurnContext, ask: &Answers, instance: &str) -> Result<(), EngineError> {
        let pol = policy(ctx.permission_mode);
        let cwd = ctx.cwd.display().to_string();
        let (method, mut params) = match resume {
            Some(thread) => ("thread/resume", json!({"threadId": thread, "model": ctx.model.model, "cwd": cwd, "approvalPolicy": pol.approval, "approvalsReviewer": "user", "sandbox": pol.sandbox, "excludeTurns": true})),
            None => ("thread/start", json!({"model": ctx.model.model, "cwd": cwd, "approvalPolicy": pol.approval, "approvalsReviewer": "user", "sandbox": pol.sandbox, "dynamicTools": dynamic_tools()})),
        };
        // A rollout copied from another account's home is resumed from where
        // it was copied to, not looked up by id in Codex's index of this
        // home, which has never heard of it.
        if let (Some(path), Some(_)) = (path, resume) {
            params["path"] = json!(path);
        }
        // Outside bypassPermissions and unhinged no MCP server of the person's or the
        // project's config runs: Codex would start each one — a command —
        // on the thread without asking, as Claude Code's are kept out by
        // --strict-mcp-config. Codex's effective config for this directory
        // names them, and the thread turns each off. MCP servers an
        // installed Codex plugin brings may not be listed there, and are
        // not reached by this — still open: the pinned protocol has no
        // thread setting krowk can verify turns a plugin's server off.
        if !ctx.permission_mode.asks_nothing() {
            let config = self.request("config/read", json!({ "cwd": cwd }), ask, INITIALIZE_TIMEOUT).await.map_err(|e| EngineError::new(&e.code, format!("krowk asks Codex which MCP servers its config names, to keep them off, and it did not say: {}", e.message)))?;
            if let Some(off) = mcp_off(&config) {
                params["config"] = off;
            }
        }
        let r = match self.request(method, params, ask, THREAD_TIMEOUT).await {
            Ok(r) => r,
            Err(e) if resume.is_some() && e.code == "backend_failed" => {
                return Err(EngineError::new("backend_resume_failed", format!("Codex on {instance} could not resume its thread {} ({}) — its home may have moved; start a new session", resume.unwrap_or_default(), e.message)));
            }
            Err(e) => return Err(e),
        };
        let sandbox = r.pointer("/sandbox/type").and_then(Value::as_str).unwrap_or_default();
        let reviewer = str_of(&r, "approvalsReviewer");
        if sandbox_rank(sandbox) > sandbox_rank(pol.sandbox) || (!reviewer.is_empty() && reviewer != "user") {
            self.terminate().await;
            return Err(EngineError::new(
                "backend_permission_mode",
                format!(
                    "Codex on {instance} opened the thread in its `{sandbox}` sandbox with `{reviewer}` reviewing approvals, looser than krowk's `{}` for this turn, so krowk stopped it before it ran anything — check `sandbox_mode`, `approvals_reviewer` and any managed requirements in its config, and the instance's `args`",
                    ctx.permission_mode.name()
                ),
            ));
        }
        let thread = r.get("thread").cloned().unwrap_or(Value::Null);
        let id = str_of(&thread, "id");
        if id.is_empty() {
            self.terminate().await;
            return Err(EngineError::new("backend_failed", format!("Codex answered {method} without a thread id")));
        }
        self.thread = Some(id.to_string());
        self.transcript = thread.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()).map(String::from);
        Ok(())
    }

    /// Runs one krowk turn: the prompt as a Codex turn, steering passed in
    /// as it arrives, until Codex completes it with nothing left unread.
    async fn turn(&mut self, prompt: Steer, ctx: &mut TurnContext, ask: &Answers, events: &Events, instance: &str) -> Result<TurnEnd, EngineError> {
        let thread = self.thread.clone().expect("opened before a turn");
        let _ = events.send(EngineEvent::Context { system: SYSTEM_NOTE.into(), tools: bridge::definitions() }).await;
        let _ = events.send(EngineEvent::BackendSession { backend: BACKEND.into(), session_id: thread.clone(), transcript: self.transcript.clone(), billing: self.billing }).await;
        let effort = effort_for(ctx.effort, self.efforts.get(&ctx.model.model).map(Vec::as_slice), ctx.model_info.as_ref(), &ctx.model.model);
        let mut t = Translator::new(&ctx.model.model);
        let mut subs = stream::SubThreads::default();
        let mut limit: Option<LimitStatus> = None;
        let mut input = vec![prompt];
        let mut interrupted = false;
        loop {
            let params = turn_params(&thread, &input, &ctx.session_dir, &ctx.model.model, effort.as_ref());
            let mut pending: HashMap<u64, Pending> = HashMap::new();
            pending.insert(self.call("turn/start", params).await?, Pending::TurnStart);
            let mut codex_turn: Option<String> = None;
            let mut want_interrupt = false;
            let mut deadline: Option<tokio::time::Instant> = None;
            let mut gone = false;
            let mut drain: Option<tokio::time::Instant> = None;
            let mut refused: Vec<Steer> = Vec::new();
            let mut completed: Option<Value> = None;
            let mut poll = tokio::time::interval(STEER_POLL);
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while completed.is_none() || pending.values().any(|p| matches!(p, Pending::Steer(_))) {
                let until = crate::engine::sleep_until(deadline);
                let drained = crate::engine::sleep_until(drain);
                let steering = codex_turn.is_some() && !interrupted && completed.is_none();
                tokio::select! {
                    biased;
                    _ = cancelled(&mut ctx.cancel), if !interrupted => {
                        interrupted = true;
                        want_interrupt = !self.interrupt(&thread, codex_turn.as_deref(), &mut pending).await?;
                        deadline = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                    }
                    _ = until => {
                        // Codex did not stop in time: the process goes, the
                        // turn keeps what it made, the session continues on
                        // `thread/resume`.
                        self.terminate().await;
                        finish(&mut t, events).await;
                        return Ok(TurnEnd::Interrupted);
                    }
                    // Codex exited — crashed, or was killed — while something
                    // it started still holds its output open: what it wrote
                    // is read for a moment, then the turn ends.
                    _ = self.child.wait(), if !gone => {
                        gone = true;
                        drain = Some(tokio::time::Instant::now() + DRAIN_GRACE);
                    }
                    _ = drained => {
                        finish(&mut t, events).await;
                        return self.died_in_turn(interrupted);
                    }
                    _ = poll.tick(), if steering => {
                        let turn = codex_turn.clone().expect("steering only once the turn has an id");
                        self.steer(&thread, &turn, ctx.steers.take(), &ctx.session_dir, &mut pending).await?;
                    }
                    msg = next_msg(&mut self.out) => {
                        let Some(msg) = msg else {
                            finish(&mut t, events).await;
                            return self.died_in_turn(interrupted);
                        };
                        match msg {
                            Msg::Response { id, result } => match id.as_u64().and_then(|id| pending.remove(&id)) {
                                Some(Pending::TurnStart) => self.turn_started(result, &thread, &mut codex_turn, &mut want_interrupt, &mut pending).await?,
                                Some(Pending::Steer(steer)) => steer_answered(result.is_ok(), steer, &mut refused, events).await,
                                Some(Pending::Interrupt) | None => {}
                            },
                            Msg::Request { id, method, params } => {
                                let answer = ask.answer(&method, &params, &mut t, Some((events, &ctx.cancel))).await;
                                self.reply(id, answer).await?;
                            }
                            Msg::Note { method, params } => on_note(&thread, &method, &params, (&mut subs, &mut t), (&mut codex_turn, &mut completed), &mut limit, events).await,
                        }
                    }
                }
            }
            let turn = completed.unwrap_or(Value::Null);
            if let Some(end) = concluded(&turn, interrupted, &mut t, events, limit.as_ref(), instance).await {
                return end;
            }
            match more_input(refused, &ctx.steers, &mut t, events).await {
                Some(more) => input = more,
                None => return Ok(TurnEnd::Completed),
            }
        }
    }

    /// Asks Codex to interrupt its turn; false when it has no id yet, and
    /// the interrupt waits for one.
    async fn interrupt(&mut self, thread: &str, codex_turn: Option<&str>, pending: &mut HashMap<u64, Pending>) -> Result<bool, EngineError> {
        let Some(turn) = codex_turn else {
            return Ok(false);
        };
        pending.insert(self.call("turn/interrupt", json!({"threadId": thread, "turnId": turn})).await?, Pending::Interrupt);
        Ok(true)
    }

    /// Passes steering into Codex's running turn.
    async fn steer(&mut self, thread: &str, turn: &str, steers: Vec<Steer>, session_dir: &Path, pending: &mut HashMap<u64, Pending>) -> Result<(), EngineError> {
        for steer in steers {
            let id = self.call("turn/steer", json!({"threadId": thread, "expectedTurnId": turn, "input": text_input(std::slice::from_ref(&steer), session_dir)})).await?;
            pending.insert(id, Pending::Steer(steer));
        }
        Ok(())
    }

    /// Codex's answer to `turn/start`: the Codex turn's id, and the
    /// interrupt that waited for it.
    async fn turn_started(&mut self, result: Result<Value, String>, thread: &str, codex_turn: &mut Option<String>, want_interrupt: &mut bool, pending: &mut HashMap<u64, Pending>) -> Result<(), EngineError> {
        let r = result.map_err(|e| EngineError::new("backend_failed", format!("Codex refused the turn: {e}")))?;
        if codex_turn.is_none() {
            *codex_turn = r.pointer("/turn/id").and_then(Value::as_str).map(String::from);
        }
        if *want_interrupt && let Some(turn) = codex_turn.clone() {
            *want_interrupt = false;
            pending.insert(self.call("turn/interrupt", json!({"threadId": thread, "turnId": turn})).await?, Pending::Interrupt);
        }
        Ok(())
    }

    /// Stops the whole process group — Codex and every shell and server it
    /// started: SIGTERM, a moment to write what it was writing, then SIGKILL.
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_back_3_the_process_is_app_server_on_stdio_in_krowks_mode() {
        assert_eq!(args(&["-c".into(), "model_provider=openrouter".into()]).join(" "), "app-server --listen stdio:// -c model_provider=openrouter");
        // The hook's command is this krowk's path, quoted for the shell
        // Codex runs it in, inside a TOML string.
        let command = paste_guard_command(Path::new("/opt/my tools/krowk (deleted)"));
        assert_eq!(command, "'/opt/my tools/krowk' __paste-guard");
        let entry = r#"{matcher="^Bash$",hooks=[{type="command",command="'/opt/my tools/krowk' __paste-guard"}]}"#;
        assert_eq!(with_paste_guard(vec!["-c".into(), "model=x".into()], &command), ["-c", "model=x", "-c", &format!("hooks.PreToolUse=[{entry}]")]);
        // The instance's own hooks are kept, the guard added beside them.
        let own = r#"hooks.PreToolUse=[{matcher="^Bash$",hooks=[{type="command",command="/usr/bin/user-guard"}]}]"#;
        let merged = with_paste_guard(vec!["-c".into(), own.into()], &command);
        assert_eq!(merged, ["-c".to_string(), format!("{},{entry}]", own.strip_suffix(']').unwrap())]);
        assert_eq!(with_paste_guard(vec!["-c".into(), "hooks.PreToolUse=[]".into()], &command), ["-c".to_string(), format!("hooks.PreToolUse=[{entry}]")]);
        let trailing = "hooks.PreToolUse=[{matcher=\"x\",hooks=[]}, ]";
        assert_eq!(with_paste_guard(vec!["-c".into(), trailing.into()], &command)[1], format!("hooks.PreToolUse=[{{matcher=\"x\",hooks=[]}},{entry}]"), "TOML's trailing comma");
        // What cannot be added to safely keeps its own `-c`, and Codex starts.
        let commented = "hooks.PreToolUse=[{matcher=\"x\",hooks=[]}, # mine\n]";
        assert_eq!(with_paste_guard(vec!["-c".into(), commented.into()], &command), ["-c", commented, "-c", &format!("hooks.PreToolUse=[{entry}]")]);
        // Of two, the guard joins the last, which is the one Codex keeps.
        let twice = with_paste_guard(vec!["-c".into(), "hooks.PreToolUse=[]".into(), "-c".into(), "hooks.PreToolUse=[]".into()], &command);
        assert_eq!(twice[1..], ["hooks.PreToolUse=[]".to_string(), "-c".into(), format!("hooks.PreToolUse=[{entry}]")]);
        let then_commented = with_paste_guard(vec!["-c".into(), "hooks.PreToolUse=[]".into(), "-c".into(), commented.into()], &command);
        assert_eq!(then_commented.last().unwrap(), &format!("hooks.PreToolUse=[{entry}]"));
        for m in [PermissionMode::Default, PermissionMode::Plan, PermissionMode::AcceptEdits] {
            assert_eq!(policy(m), Policy { approval: "on-request", sandbox: "read-only" }, "every edit and command beyond reading is asked about");
        }
        assert_eq!(policy(PermissionMode::BypassPermissions), Policy { approval: "never", sandbox: "danger-full-access" });
        assert_eq!(policy(PermissionMode::Unhinged), policy(PermissionMode::BypassPermissions));
        assert!(sandbox_rank("workspaceWrite") > sandbox_rank("read-only") && sandbox_rank("dangerFullAccess") > sandbox_rank("workspace-write") && sandbox_rank("somethingNew") == sandbox_rank("danger-full-access"));
        let init = initialize_params("1.2.3");
        assert_eq!((init["clientInfo"]["name"].as_str(), init["capabilities"]["experimentalApi"].as_bool()), (Some("krowk"), Some(true)), "krowk as itself");
        let tools = dynamic_tools();
        assert_eq!((tools[0]["type"].as_str(), tools[0]["name"].as_str(), tools[0]["tools"][0]["name"].as_str()), (Some("namespace"), Some("krowk"), Some("session_info")));
        // The environment: the native openai instance's key never reaches a
        // ChatGPT account, unless the instance names it.
        let mut b = Backend { binary: "codex".into(), path: None, config_dir: Some("/data/codex-team".into()), home: None, env: Default::default(), args: vec![], key: None, shared_home: None };
        assert_eq!(cleared(&b).collect::<Vec<_>>(), NOT_INHERITED);
        b.key = Some(("OPENAI_API_KEY".into(), "sk-router".into()));
        b.env.insert("OPENAI_BASE_URL".into(), "https://router.example/v1".into());
        let (remove, set) = environment(&b);
        assert!(!remove.contains(&"OPENAI_API_KEY") && !remove.contains(&"OPENAI_BASE_URL") && remove.contains(&"CODEX_API_KEY"), "{remove:?}");
        for identity in ["CODEX_ACCESS_TOKEN", "CODEX_SQLITE_HOME", "CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "CODEX_APP_SERVER_LOGIN_ISSUER", "CODEX_THREAD_ID"] {
            assert!(remove.contains(&identity), "{identity} reaches Codex");
        }
        assert_eq!(set, [("CODEX_HOME".to_string(), "/data/codex-team".to_string()), ("OPENAI_BASE_URL".into(), "https://router.example/v1".into()), ("OPENAI_API_KEY".into(), "sk-router".into())]);
    }

    #[cfg(unix)]
    #[test]
    fn r_inst_1_an_accounts_home_shares_the_configuration_and_keeps_its_login() {
        let base = std::env::temp_dir().join(format!("krowk-codex-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (own, team) = (base.join("own"), base.join("team"));
        std::fs::create_dir_all(own.join("skills/lint")).unwrap();
        std::fs::create_dir_all(&team).unwrap();
        std::fs::write(own.join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        std::fs::write(own.join("AGENTS.md"), "be brief\n").unwrap();
        // The person's login and threads, which are theirs alone.
        let login = ["auth", ".json"].concat();
        std::fs::write(own.join(&login), "").unwrap();
        std::fs::create_dir_all(own.join("sessions")).unwrap();
        std::fs::write(team.join("AGENTS.md"), "the team's own\n").unwrap();
        let shared = share(&team, &own).unwrap();
        assert_eq!(shared, ["config.toml", "skills"], "what the account does not have of its own");
        assert_eq!(std::fs::read_to_string(team.join("config.toml")).unwrap(), "model = \"gpt-5.5\"\n");
        assert_eq!(std::fs::read_to_string(team.join("AGENTS.md")).unwrap(), "the team's own\n", "never replaced");
        assert!(team.join(&login).symlink_metadata().is_err() && team.join("sessions").symlink_metadata().is_err(), "the login and the threads are not shared");
        assert_eq!(share(&team, &own).unwrap(), shared, "adding the account again changes nothing");
        assert!(share(&own, &own).unwrap().is_empty());
        // Skills: the account's own directory, a link per skill, and never
        // Codex's bundled `.system` — which it writes into the home it runs in.
        std::fs::create_dir_all(own.join("skills/review")).unwrap();
        std::fs::create_dir_all(own.join("skills/.system/bundled")).unwrap();
        share(&team, &own).unwrap();
        assert!(team.join("skills").symlink_metadata().unwrap().is_dir(), "a directory of the account's own");
        assert_eq!(std::fs::read_link(team.join("skills/review")).unwrap(), own.join("skills/review"));
        assert!(team.join("skills/.system").symlink_metadata().is_err());
        std::fs::create_dir_all(team.join("skills/.system/codex")).unwrap();
        assert!(!own.join("skills/.system/codex").exists(), "what Codex installs in the account stays there");
        // A skill added later is linked on the next run, one removed is
        // unlinked, and so is a shared file the person deleted.
        std::fs::create_dir_all(own.join("skills/deploy")).unwrap();
        std::fs::remove_dir_all(own.join("skills/review")).unwrap();
        std::fs::remove_file(own.join("config.toml")).unwrap();
        share(&team, &own).unwrap();
        assert_eq!(std::fs::read_link(team.join("skills/deploy")).unwrap(), own.join("skills/deploy"));
        assert!(team.join("skills/review").symlink_metadata().is_err(), "a dangling skill link is pruned");
        assert!(team.join("config.toml").symlink_metadata().is_err(), "a dangling config link is pruned");
        assert!(team.join("skills/.system/codex").exists() && team.join("AGENTS.md").exists(), "the account's own are untouched");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn r_back_3_the_mcp_servers_codexs_config_names_are_turned_off() {
        let read = json!({"config": {"model": "gpt-5.5", "mcp_servers": {"tripwire": {"command": "/bin/sh", "enabled": true}, "docs.search": {"url": "https://x"}}}, "origins": {}});
        assert_eq!(mcp_off(&read), Some(json!({"mcp_servers": {"tripwire": {"enabled": false}, "docs.search": {"enabled": false}}})), "a name with a dot stays one name");
        assert_eq!(mcp_off(&json!({"config": {}, "origins": {}})), None);
        assert_eq!(mcp_off(&json!({"config": {"mcp_servers": {}}, "origins": {}})), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_back_3_a_line_that_is_not_json_utf8_or_short_is_skipped_not_the_end() {
        let mut raw: Vec<u8> = b"not json\n\xff\xfe{\"broken\n".to_vec();
        raw.extend_from_slice(b"{\"method\":\"turn/started\",\"params\":{}}\n");
        raw.extend_from_slice(&[b'x'; 64]);
        raw.extend_from_slice(b"\n{\"id\":1,\"result\":{}}");
        let mut r = LineReader::new(BufReader::new(&raw[..]));
        r.cap = 48;
        assert!(matches!(next_msg(&mut r).await, Some(Msg::Note { method, .. }) if method == "turn/started"), "invalid UTF-8 is skipped");
        assert!(matches!(next_msg(&mut r).await, Some(Msg::Response { .. })), "a line past the cap is skipped, and a last line without a newline read");
        assert!(next_msg(&mut r).await.is_none());
        // Line by line: the long one is reported, not kept.
        let mut r = LineReader::new(BufReader::new(&raw[..]));
        r.cap = 48;
        let mut lines = Vec::new();
        while let Some(l) = r.next().await {
            lines.push(l);
        }
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[3], Line::TooLong);
    }

    #[test]
    fn r_back_3_effort_lands_on_a_rung_codex_says_the_model_takes() {
        let listed: Vec<String> = ["low", "medium", "high", "xhigh"].iter().map(|s| s.to_string()).collect();
        assert_eq!(effort_for(Some(Effort::Max), Some(&listed), None, "gpt-5.5").as_deref(), Some("xhigh"));
        assert_eq!(effort_for(Some(Effort::Minimal), Some(&listed), None, "gpt-5.5").as_deref(), Some("low"));
        assert_eq!(effort_for(None, Some(&listed), None, "gpt-5.5"), None, "none asked, none sent: the thread's default");
        assert_eq!(effort_for(Some(Effort::High), None, None, "mystery").as_deref(), Some("high"), "a model nobody lists is sent the rung as it is");
    }

    #[test]
    fn r_back_3_approvals_follow_krowks_modes() {
        let cwd = std::env::temp_dir().join(format!("krowk-codex-approve-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let f = |p: &str| vec![cwd.join(p).display().to_string()];
        let judge = |m, call: Result<Call, String>, protected: &[PathBuf]| crate::permissions::judge(m, call, &cwd, protected);
        let change = |m, paths: Option<&[String]>, root: Option<&str>| judge(m, file_change_call(paths, root, &cwd), &[]);
        assert!(judge(PermissionMode::AcceptEdits, Ok(command_call("ls")), &[]).unwrap_err().contains("runs a command"));
        assert!(judge(PermissionMode::BypassPermissions, Ok(command_call("ls")), &[]).is_ok());
        let ae = PermissionMode::AcceptEdits;
        assert!(change(PermissionMode::Default, Some(&f("a.txt")), None).unwrap_err().contains("default mode"));
        assert!(change(PermissionMode::Plan, Some(&f("a.txt")), None).unwrap_err().contains("plan mode"));
        assert!(change(ae, Some(&f("a.txt")), None).is_ok());
        assert!(change(ae, Some(&["/etc/passwd".to_string()]), None).unwrap_err().contains("outside the working directory"));
        for p in [".codex/config.toml", ".git/hooks/pre-commit", "sub/.CODEX/rules/x.rules", ".claude/settings.json", ".krowk/config.json"] {
            assert!(change(ae, Some(&f(p)), None).is_err(), "{p}");
        }
        assert!(change(ae, Some(&f("a.txt")), Some("/")).unwrap_err().contains("rest of the session"));
        assert!(change(ae, None, None).is_err(), "a patch whose files krowk did not see is not approved blind");
        // A move is judged by where it lands too, in both protocols.
        let moved = stream::change_paths(&json!([{"path": cwd.join("a.txt"), "kind": {"type": "update", "move_path": cwd.join(".git/hooks/pre-commit")}, "diff": ""}]));
        assert_eq!(moved.len(), 2);
        assert!(change(ae, Some(&moved), None).unwrap_err().contains(".git"));
        let legacy = stream::change_paths(&json!({ cwd.join("a.txt").display().to_string(): {"type": "update", "unified_diff": "", "move_path": "/etc/cron.d/x"} }));
        assert!(change(ae, Some(&legacy), None).unwrap_err().contains("outside the working directory"));
        let home = cwd.join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(judge(ae, file_change_call(Some(&f("codex-home/config.toml")), None, &cwd), std::slice::from_ref(&home)).unwrap_err().contains("settings decide what runs"));
        assert!(judge(PermissionMode::BypassPermissions, file_change_call(Some(&f(".codex/config.toml")), None, &cwd), &[home]).is_ok());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn codexs_questions_are_the_persons_to_answer_by_id_with_what_they_wrote_as_a_note() {
        use crate::protocol::{ApprovalDecision, QuestionAnswer};
        let approvals = permissions::Approvals::default();
        let gate = Gate::new(permissions::Policy::modes_only(Path::new("/repo")), PermissionMode::Default, Default::default(), Some(approvals.clone()), None, "s-1", "t-1");
        let ask = Answers { session_id: "s-1".into(), turn_id: "t-1".into(), model: ModelRef { instance: "codex:team".into(), model: "gpt-5.5".into() }, cwd: PathBuf::from("/repo"), krowk_version: "test".into(), gate, evidence: None };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_c, cancel) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let EngineEvent::Approval(req) = ev {
                    assert_eq!((req.questions[0].id.as_str(), req.questions[0].options.len(), req.questions[1].secret), ("db", 2, true), "{req:?}");
                    let answers = vec![QuestionAnswer { id: "db".into(), picked: vec!["Postgres".into()], text: Some("on 16".into()) }, QuestionAnswer { id: "key".into(), picked: vec![], text: Some("s3cret".into()) }];
                    approvals.answer("s-1", &req.request_id, ApprovalDecision::Allow, answers).unwrap();
                }
            }
        });
        let params = json!({"itemId": "i", "questions": [
            {"id": "db", "header": "DB", "question": "Which?", "isOther": true, "options": [{"label": "Postgres", "description": "d"}, {"label": "SQLite", "description": "e"}]},
            {"id": "key", "header": "Key", "question": "Token?", "isSecret": true, "options": null},
            {"id": "skip", "header": "", "question": "Skipped?", "options": [{"label": "a", "description": ""}]},
        ]});
        let got = ask.answer("item/tool/requestUserInput", &params, &mut Translator::default(), Some((&tx, &cancel))).await.unwrap();
        assert_eq!(got, json!({"answers": {"db": {"answers": ["Postgres", "user_note: on 16"]}, "key": {"answers": ["s3cret"]}, "skip": {"answers": []}}}));
        // A deny rule on AskUserQuestion holds for Codex's questions too.
        let mut policy = permissions::Policy::modes_only(Path::new("/repo"));
        policy.loaded.rules.push((permissions::Kind::Deny, permissions::rules::parse("AskUserQuestion", "test", Path::new("/repo")).unwrap()));
        let denied = Answers { gate: Gate::new(policy, PermissionMode::Default, Default::default(), Some(permissions::Approvals::default()), None, "s-1", "t-1"), ..ask };
        let got = denied.answer("item/tool/requestUserInput", &params, &mut Translator::default(), Some((&tx, &cancel))).await.unwrap();
        assert!(!got["answers"]["db"]["answers"][0].as_str().unwrap().contains("Postgres"), "{got}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_back_3_codex_requests_are_answered_and_krowks_tools_run_through_the_bridge() {
        let ask = Answers {
            session_id: "s-1".into(),
            turn_id: "t-1".into(),
            model: ModelRef { instance: "codex:team".into(), model: "gpt-5.5".into() },
            cwd: PathBuf::from("/repo"),
            krowk_version: "test".into(),
            gate: Gate::modes_only(Path::new("/repo"), PermissionMode::Default),
            evidence: None,
        };
        let mut t = Translator::default();
        let a = ask.answer("item/tool/call", &json!({"threadId": "th", "turnId": "tu", "callId": "c", "namespace": "krowk", "tool": "session_info", "arguments": {}}), &mut t, None).await.unwrap();
        assert_eq!(a["success"], true);
        let text = a["contentItems"][0]["text"].as_str().unwrap();
        assert!(text.contains("krowk session: s-1") && text.contains("instance: codex:team") && text.contains("backend: codex-app-server"), "{text}");
        assert_eq!(ask.answer("item/tool/call", &json!({"namespace": "other", "tool": "x", "arguments": {}}), &mut t, None).await.unwrap()["success"], false);
        assert_eq!(ask.answer("item/commandExecution/requestApproval", &json!({"itemId": "c1"}), &mut t, None).await.unwrap()["decision"], "decline");
        assert!(t.declined["c1"].contains("needs approval"), "the reason is kept for the call's result");
        assert_eq!(ask.answer("item/fileChange/requestApproval", &json!({"itemId": "f1"}), &mut t, None).await.unwrap()["decision"], "decline");
        assert_eq!(ask.answer("item/permissions/requestApproval", &json!({"itemId": "p1"}), &mut t, None).await.unwrap(), json!({"permissions": {}, "scope": "turn"}));
        assert_eq!(ask.answer("mcpServer/elicitation/request", &json!({}), &mut t, None).await.unwrap()["action"], "decline");
        let q = ask.answer("item/tool/requestUserInput", &json!({"questions": [{"id": "q1", "header": "h", "question": "which?"}]}), &mut t, None).await.unwrap();
        assert!(q["answers"]["q1"]["answers"][0].as_str().unwrap().contains("Decide"), "nobody to ask: the model is told to decide");
        assert_eq!(ask.answer("execCommandApproval", &json!({}), &mut t, None).await.unwrap()["decision"], "denied");
        // publish is offered too, and judged as the native one is.
        let refused = ask.answer("item/tool/call", &json!({"namespace": "krowk", "tool": "publish", "arguments": {"files": ["a.png"]}}), &mut t, None).await.unwrap();
        assert_eq!(refused["success"], false);
        assert!(refused["contentItems"][0]["text"].as_str().unwrap().contains("needs approval"), "{refused}");
        assert_eq!(ask.answer("account/chatgptAuthTokens/refresh", &json!({}), &mut t, None).await.unwrap_err().0, -32601, "krowk never handles Codex's tokens");
    }

    #[test]
    fn r_back_3_a_failed_turn_names_a_stale_login_as_one() {
        let e = failure(&json!({"message": "unexpected status 401 Unauthorized: Missing bearer", "codexErrorInfo": "other"}), "codex:team");
        assert_eq!((e.code.as_str(), e.status), ("not_authenticated", 401));
        assert!(e.message.contains("krowk connect openai --method subscription --name team"), "{}", e.message);
        let e = failure(&json!({"message": "slow down", "codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": 429}}}), "codex");
        assert_eq!((e.code.as_str(), e.status), ("backend_failed", 429));
        assert_eq!(failure(&json!({"message": "limit", "codexErrorInfo": "usageLimitExceeded"}), "codex").code, "usage_limit");
        let m = |s: &str| classify(serde_json::from_str(s).unwrap()).unwrap();
        assert!(matches!(m(r#"{"id":1,"result":{"ok":true}}"#), Msg::Response { result: Ok(_), .. }));
        assert!(matches!(m(r#"{"id":2,"error":{"code":-32600,"message":"bad"}}"#), Msg::Response { result: Err(e), .. } if e == "bad"));
        assert!(matches!(m(r#"{"id":0,"method":"item/tool/call","params":{}}"#), Msg::Request { .. }));
        assert!(matches!(m(r#"{"method":"turn/started","params":{}}"#), Msg::Note { .. }));
    }
}
