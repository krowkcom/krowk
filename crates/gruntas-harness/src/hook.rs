//! `gruntas hook <harness> <event>`: the one entry point every hook of
//! gruntas's plugin runs, in Claude Code and in Codex (D3). It reads the
//! harness's payload on stdin, turns it into one internal event
//! (`normalise`), and sends what the daemon keeps of it as an `activity`
//! frame over the daemon's socket (`to_activity`, `send`).
//!
//! A hook that only reports never blocks or fails its session: whatever
//! goes wrong — no daemon, a stalled one, a payload it cannot read — it
//! gives up silently, notes why in `hook.log` beside the daemon's socket,
//! and exits 0 with nothing on stdout, so the harness carries on as if
//! there were no hook (the two that answer are below). Its work runs on a
//! worker thread the main thread waits on for at most
//! `GIVE_UP`: past that the process exits whatever the worker is doing, so
//! the hook is done well inside the 2 seconds it is allowed.
//!
//! `SessionStart` registers the session with what the hook can find beside
//! the payload (D4): the harness's process, the working directory's origin
//! remote, and whether a person is at it; it starts the daemon when none
//! runs. `SessionEnd` hands its deregistration to a detached copy of
//! itself and returns at once (`background`).
//!
//! Two answers do reach the harness (KT05). A `PreToolUse` whose bash call
//! the paste guards refuse prints their deny (`guarded`), and only then.
//! `WorktreeCreate` and `WorktreeRemove` place Claude Code's worktrees
//! through `gruntas worktrees` (`worktree`): the session waits on those, so
//! they run to the end rather than inside `GIVE_UP`, and a worktree that
//! could not be made fails, as Claude Code needs it to.

use crate::protocol::{Activity, Attendance, ClientFrame, PluginHarness, Registration, ServerFrame, UsageWindow, PROTOCOL_VERSION};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The word that makes `gruntas` the hook rather than the CLI.
pub const ARG: &str = "hook";

/// How long the hook waits for its work before it exits anyway: under the
/// 2 s a hook may take, with room for the process's own start and exit.
pub const GIVE_UP: Duration = Duration::from_millis(1500);

/// The most of a payload read: a `PostToolUse` carries the tool's whole
/// output.
const MOST_STDIN: u64 = 16 << 20;

/// `hook.log` is started over past this.
const LOG_MOST: u64 = 256 << 10;

/// Set on the detached copy `SessionEnd` hands its work to, so that copy
/// does it rather than handing it on again.
const BACKGROUND: &str = "GRUNTAS_HOOK_BACKGROUND";

/// The variables a harness sets for its own session (its account, its
/// nesting markers, its hooks' own), kept from a daemon a hook starts. The
/// person's own settings (`CLAUDE_CODE_USE_BEDROCK`, an `apiKeyEnv`) stay.
const HARNESS_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_ENV_FILE",
    "CLAUDE_PLUGIN_ROOT",
    "CLAUDE_PROJECT_DIR",
    "CODEX_HOME",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_SESSION_ID",
    "CODEX_THREAD_ID",
];

/// How long a hook that started the daemon waits for its socket: inside
/// `GIVE_UP`, which bounds it anyway.
const START_WAIT: Duration = Duration::from_millis(1300);

/// One session's part of a payload: every event carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
}

/// A harness's payload as one internal event: the same for Claude Code and
/// Codex.
#[derive(Debug, Clone, PartialEq)]
pub enum HookEvent {
    /// `source`: `startup`, `resume`, `clear`, `compact`.
    SessionStart { session: Session, source: Option<String> },
    SessionEnd { session: Session },
    /// A tool call about to run: `PreToolUse`.
    ToolStart { session: Session, tool_call_id: String, tool_name: String, input: Value },
    /// And done: `PostToolUse`, or `PostToolUseFailure`.
    ToolEnd { session: Session, tool_call_id: String, tool_name: String, input: Value },
    /// The turn's end.
    Stop { session: Session },
    /// The account's usage windows, from the Claude plugin's mod.
    Usage { config_home: String, windows: Vec<UsageWindow> },
}

/// What the hook knows beside the payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    /// The harness's process, when found.
    pub harness_pid: Option<u32>,
    /// The account's config home.
    pub config_home: Option<String>,
    /// The working directory's origin remote.
    pub repository: Option<String>,
    pub attendance: Option<Attendance>,
    /// The commands that count toward the busy build slots
    /// (`builds.heavy`); read for a tool call only.
    pub heavy: Vec<String>,
}

#[derive(Deserialize)]
struct Common {
    session_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
}

#[derive(Deserialize)]
struct Tool {
    tool_use_id: String,
    tool_name: String,
    #[serde(default)]
    tool_input: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsagePayload {
    config_home: String,
    windows: Vec<UsageWindow>,
}

pub fn harness(word: &str) -> Option<PluginHarness> {
    match word {
        "claude" => Some(PluginHarness::Claude),
        "codex" => Some(PluginHarness::Codex),
        _ => None,
    }
}

/// `payload` as event `event` of `harness`'s hooks; none for an event this
/// hook does nothing with. Claude Code and Codex send the same shape (Codex
/// adds `model` and `turn_id`, and may send a null `transcript_path`), so
/// the harness changes nothing yet: it is here for where they part.
pub fn normalise(_harness: PluginHarness, event: &str, payload: &Value) -> Result<Option<HookEvent>, String> {
    let session = || -> Result<Session, String> {
        let c = Common::deserialize(payload).map_err(|e| format!("a {event} payload without its session: {e}"))?;
        Ok(Session { id: c.session_id, cwd: c.cwd, transcript_path: c.transcript_path })
    };
    let tool = || Tool::deserialize(payload).map_err(|e| format!("a {event} payload without its tool call: {e}"));
    Ok(Some(match event {
        "SessionStart" => HookEvent::SessionStart { session: session()?, source: payload.get("source").and_then(Value::as_str).map(String::from) },
        "SessionEnd" => HookEvent::SessionEnd { session: session()? },
        "PreToolUse" => {
            let t = tool()?;
            HookEvent::ToolStart { session: session()?, tool_call_id: t.tool_use_id, tool_name: t.tool_name, input: t.tool_input }
        }
        // A call that failed (a build or test that exited non-zero) ends
        // with `PostToolUseFailure` in Claude Code, not `PostToolUse`.
        "PostToolUse" | "PostToolUseFailure" => {
            let t = tool()?;
            HookEvent::ToolEnd { session: session()?, tool_call_id: t.tool_use_id, tool_name: t.tool_name, input: t.tool_input }
        }
        "Stop" => HookEvent::Stop { session: session()? },
        "usage" => {
            let u = UsagePayload::deserialize(payload).map_err(|e| format!("a usage payload that is not one: {e}"))?;
            HookEvent::Usage { config_home: u.config_home, windows: u.windows }
        }
        _ => return Ok(None),
    }))
}

/// What the daemon is told of `event`, if anything. A tool call is told
/// only when it runs a command on the heavy list (`ctx.heavy`), its start
/// and its end; the turn's end always, so a heavy command whose end never
/// came counts no more (D5).
pub fn to_activity(harness: PluginHarness, event: &HookEvent, ctx: &Context) -> Option<Activity> {
    match event {
        HookEvent::SessionStart { session, .. } => Some(Activity::Register(Registration {
            session_id: session.id.clone(),
            harness,
            harness_pid: ctx.harness_pid,
            config_home: ctx.config_home.clone(),
            cwd: session.cwd.clone(),
            repository: ctx.repository.clone(),
            transcript_path: session.transcript_path.clone(),
            attendance: ctx.attendance,
        })),
        HookEvent::SessionEnd { session } => Some(Activity::Deregister { session_id: session.id.clone() }),
        HookEvent::ToolStart { session, tool_call_id, input, .. } => heavy(input, ctx).then(|| Activity::HeavyStart { session_id: session.id.clone(), tool_call_id: tool_call_id.clone() }),
        HookEvent::ToolEnd { session, tool_call_id, input, .. } => heavy(input, ctx).then(|| Activity::HeavyEnd { session_id: session.id.clone(), tool_call_id: tool_call_id.clone() }),
        HookEvent::Stop { session } => Some(Activity::TurnEnd { session_id: session.id.clone() }),
        HookEvent::Usage { config_home, windows } => Some(Activity::Usage { config_home: config_home.clone(), windows: windows.clone() }),
    }
}

/// Whether a tool call's input is a shell command on the heavy list: any
/// tool's, Claude Code's `Bash` and Codex's alike, that has a `command`.
fn heavy(input: &Value, ctx: &Context) -> bool {
    input.get("command").and_then(Value::as_str).is_some_and(|c| crate::builds::listed(c, &ctx.heavy))
}

/// The heavy list config.json sets (`builds.heavy`), else the default
/// (`builds::LISTED`): a config that cannot be read changes nothing here,
/// as the hook never fails its session.
pub fn heavy_list(env: &dyn Fn(&str) -> String) -> Vec<String> {
    let set = gruntas_api::home::resolve(env)
        .ok()
        .and_then(|h| std::fs::read(h.join(gruntas_api::home::CONFIG)).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.get("builds")?.get("heavy")?.clone()).ok());
    crate::builds::BuildsConfig { heavy: set, ..Default::default() }.heavy_list()
}

/// The account a hook of `harness` runs as: its config home.
pub fn config_home(harness: PluginHarness, env: &dyn Fn(&str) -> String) -> Option<String> {
    let (var, default) = match harness {
        PluginHarness::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        PluginHarness::Codex => ("CODEX_HOME", ".codex"),
    };
    // One account, one key: `/h/.claude/` and `/h/.claude` are the same.
    let home = match env(var) {
        v if !v.is_empty() => v,
        _ => Some(env("HOME")).filter(|h| !h.is_empty()).map(|h| Path::new(&h).join(default).display().to_string())?,
    };
    Some(match home.trim_end_matches('/') {
        "" => "/".to_string(),
        h => h.to_string(),
    })
}

/// The harness's process: the hook's parent, or past it the first that is
/// not a shell, as a harness may run the hook's command line in one. None
/// when that is init (the harness already gone).
pub fn harness_pid() -> Option<u32> {
    // SAFETY: getppid takes nothing and cannot fail.
    let mut pid = u32::try_from(unsafe { libc::getppid() }).ok()?;
    for _ in 0..4 {
        if pid <= 1 {
            return None;
        }
        let (ppid, name) = process(pid)?;
        if !matches!(name.as_str(), "sh" | "bash" | "dash" | "zsh" | "ksh" | "mksh" | "fish") {
            return Some(pid);
        }
        pid = ppid;
    }
    None
}

/// Process `pid`'s parent and name.
#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<(u32, String)> {
    // `pid (name) state ppid …`; the name may hold anything, `)` too.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let ppid = stat.get(close + 1..)?.split_whitespace().nth(1)?.parse().ok()?;
    Some((ppid, stat.get(open + 1..close)?.to_string()))
}

/// Process `pid`'s parent and name.
#[cfg(not(target_os = "linux"))]
fn process(pid: u32) -> Option<(u32, String)> {
    let out = std::process::Command::new("ps").args(["-o", "ppid=", "-o", "comm=", "-p", &pid.to_string()]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let (ppid, comm) = line.trim().split_once(char::is_whitespace)?;
    let name = comm.trim().rsplit('/').next()?.trim_start_matches('-');
    Some((ppid.parse().ok()?, name.to_string()))
}

/// `cwd`'s origin remote, as git reads it (with its own hooks and
/// fsmonitor off): none outside a repository or without one.
pub fn repository(cwd: &str) -> Option<String> {
    let out = gruntas_api::git::query(Path::new(cwd)).ok()?.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").args(["remote", "get-url", "origin"]).stderr(std::process::Stdio::null()).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|u| !u.is_empty())
}

/// Whether a person is at the session: the harness runs on a terminal,
/// which its hook shares as its controlling one. A session started with no
/// terminal (a script, a service, `claude -p` from cron) is unattended, as
/// `gruntas -p` without one is.
pub fn attendance() -> Attendance {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOCTTY).open("/dev/tty") {
        Ok(_) => Attendance::Attended,
        Err(_) => Attendance::Unattended,
    }
}

/// Sends `activity` to the daemon on `socket` and waits for its answer:
/// the hello and the frame in one write, then the writing half closed, so
/// a daemon too old to know the frame closes the connection at once rather
/// than leaving the hook to wait. Blocks: the caller bounds it (`GIVE_UP`).
pub fn send(socket: &Path, cwd: &str, version: &str, activity: Activity) -> Result<(), String> {
    let stream = std::os::unix::net::UnixStream::connect(socket).map_err(|e| format!("no daemon on {}: {e}", socket.display()))?;
    talk(stream, cwd, version, activity)
}

/// The daemon on `socket`, started when none runs: under its lock, as
/// `daemon::ensure` starts it, from this very binary, its output to
/// `host.log` in gruntas's home.
fn connect_or_start(env: &dyn Fn(&str) -> String, socket: &Path) -> Result<std::os::unix::net::UnixStream, String> {
    use std::os::unix::net::UnixStream;
    let absent = |e: std::io::Error| if crate::daemon::absent(&e) { Ok(()) } else { Err(format!("no daemon on {}: {e}", socket.display())) };
    match UnixStream::connect(socket) {
        Ok(s) => return Ok(s),
        Err(e) => absent(e)?,
    }
    let dir = socket.parent().unwrap_or(Path::new("/"));
    let _lock = crate::daemon::Lock::take(dir)?;
    match UnixStream::connect(socket) {
        Ok(s) => return Ok(s),
        Err(e) => absent(e)?,
    }
    let _ = std::fs::remove_file(socket);
    let exe = std::env::current_exe().map_err(|e| format!("no daemon, and gruntas cannot find its own binary to start one: {e}"))?;
    let log = gruntas_api::home::dir(env).map_err(|e| format!("no daemon, and none started: {}", e.fix()))?.join("host.log");
    let mut child = crate::daemon::spawn_detached_without(&exe, &["host", "serve"], &log, HARNESS_ENV).map_err(|e| format!("no daemon, and none started: {e}"))?;
    let deadline = std::time::Instant::now() + START_WAIT;
    loop {
        match UnixStream::connect(socket) {
            Ok(s) => return Ok(s),
            Err(e) => absent(e)?,
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the daemon it started exited as it started ({status}): {} says why", log.display()));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("the daemon it started did not open {} within {} ms", socket.display(), START_WAIT.as_millis()));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The hello and `activity` on `stream`, and the daemon's answer.
fn talk(mut stream: std::os::unix::net::UnixStream, cwd: &str, version: &str, activity: Activity) -> Result<(), String> {
    let hello = ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: cwd.to_string(), gruntas_version: version.to_string(), answers_approvals: false, token: None, background: true };
    let mut out = serde_json::to_vec(&hello).expect("a frame serializes");
    out.push(b'\n');
    out.extend(serde_json::to_vec(&ClientFrame::Activity { id: 1, activity }).expect("a frame serializes"));
    out.push(b'\n');
    stream.write_all(&out).map_err(|e| format!("the daemon did not take the frame: {e}"))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| format!("the daemon's answer could not be read: {e}"))?;
        match serde_json::from_str::<ServerFrame>(&line) {
            Ok(ServerFrame::Activity { id: 1, error: None }) => return Ok(()),
            Ok(ServerFrame::Activity { id: 1, error: Some(e) }) => return Err(format!("the daemon refused it: {} ({})", e.message, e.code)),
            Ok(ServerFrame::Refused { message, .. }) => return Err(format!("the daemon refused the hello: {message}")),
            _ => {}
        }
    }
    Err("the daemon closed without answering (a gruntas too old to know the frame)".into())
}

/// One hook's work, on the worker: what to print on stdout, if anything,
/// and why it gave up, if it did.
pub fn run(args: &[String], stdin: impl Read, env: &dyn Fn(&str) -> String, version: &str) -> (Option<String>, Option<String>) {
    let (Some(h), Some(event)) = (args.first().and_then(|w| harness(w)), args.get(1)) else {
        return (None, Some(format!("`gruntas hook {}`: say a harness (claude, codex) and an event", args.join(" "))));
    };
    let mut raw = Vec::new();
    if let Err(e) = stdin.take(MOST_STDIN).read_to_end(&mut raw) {
        return (None, Some(format!("{event}: stdin could not be read: {e}")));
    }
    let payload: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => return (None, Some(format!("{event}: the payload is not JSON: {e}"))),
    };
    let ev = match normalise(h, event, &payload) {
        Ok(Some(ev)) => ev,
        Ok(None) => return (None, None),
        Err(e) => return (None, Some(e)),
    };
    // A refused call never runs, so there is nothing to report of it, and
    // the deny never waits on the daemon.
    if let Some(deny) = guarded(&ev) {
        return (Some(deny), None);
    }
    let ctx = match &ev {
        HookEvent::SessionStart { session, .. } => Context { harness_pid: harness_pid(), config_home: config_home(h, env), repository: session.cwd.as_deref().and_then(repository), attendance: Some(attendance()), heavy: Vec::new() },
        HookEvent::ToolStart { .. } | HookEvent::ToolEnd { .. } => Context { config_home: config_home(h, env), heavy: heavy_list(env), ..Context::default() },
        _ => Context { config_home: config_home(h, env), ..Context::default() },
    };
    let Some(activity) = to_activity(h, &ev, &ctx) else { return (None, None) };
    let cwd = match &ev {
        HookEvent::SessionStart { session, .. } | HookEvent::SessionEnd { session } | HookEvent::ToolStart { session, .. } | HookEvent::ToolEnd { session, .. } | HookEvent::Stop { session } => session.cwd.clone(),
        HookEvent::Usage { .. } => None,
    };
    let cwd = cwd.or_else(|| std::env::current_dir().ok().map(|d| d.display().to_string())).unwrap_or_else(|| "/".into());
    let socket = match crate::daemon::socket(env) {
        Ok(s) => s,
        Err(e) => return (None, Some(format!("{event}: {e}"))),
    };
    // A session that ends with no daemon has nothing to deregister from,
    // and a heavy command or turn's end nothing to clear; a heavy command's
    // start is not held up for a daemon to start (its session's start
    // started one). Only the rest start one.
    let stream = match &activity {
        Activity::Register(_) | Activity::Usage { .. } => connect_or_start(env, &socket),
        Activity::Deregister { .. } | Activity::HeavyStart { .. } | Activity::HeavyEnd { .. } | Activity::TurnEnd { .. } => std::os::unix::net::UnixStream::connect(&socket).map_err(|e| format!("no daemon on {}: {e}", socket.display())),
    };
    match stream.and_then(|s| talk(s, &cwd, version, activity)) {
        Ok(()) => (None, None),
        Err(e) => (None, Some(format!("{event}: {e}"))),
    }
}

/// What a `PreToolUse` answers, when the guards refuse its bash call: the
/// deny with their message (`paste_guard::plugin_guard`, D6). Nothing for
/// any other call, so the person's own permission prompts stand.
pub fn guarded(event: &HookEvent) -> Option<String> {
    let HookEvent::ToolStart { session, tool_name, input, .. } = event else { return None };
    let command = input.get("command").and_then(Value::as_str).filter(|_| tool_name == "Bash")?;
    let cwd = session.cwd.as_deref().unwrap_or(".");
    crate::paste_guard::plugin_guard(command, Path::new(cwd)).map(|why| crate::paste_guard::deny(&why).to_string())
}

/// The events that place Claude Code's worktrees: run to the end, not
/// bounded by `GIVE_UP`, since the session waits on the worktree.
pub const WORKTREE_EVENTS: [&str; 2] = ["WorktreeCreate", "WorktreeRemove"];

/// `WorktreeCreate`: `gruntas worktrees add` (`gruntas` being `exe`) of the
/// repository the session is in, recorded as the session's; the path it
/// made. gruntas names the directory: the payload's `name` is not used.
/// `WorktreeRemove`: `gruntas worktrees remove` of the path Claude Code
/// names, which refuses one with work in it and keeps its branch.
pub fn worktree(exe: &Path, event: &str, payload: &Value) -> Result<Option<String>, String> {
    let c = Common::deserialize(payload).map_err(|e| format!("a {event} payload without its session: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null());
    match event {
        "WorktreeCreate" => {
            let cwd = c.cwd.filter(|d| Path::new(d).is_absolute()).ok_or_else(|| format!("a {event} payload without the session's directory"))?;
            cmd.args(["worktrees", "add", "--json"]).current_dir(cwd).env("CLAUDE_CODE_SESSION_ID", &c.session_id).env_remove("GRUNTAS_SESSION");
        }
        "WorktreeRemove" => {
            // An absolute path: never a word `gruntas` could read as a flag.
            let path = payload.get("worktree_path").and_then(Value::as_str).filter(|p| Path::new(p).is_absolute()).ok_or_else(|| format!("a {event} payload without an absolute worktree_path"))?;
            cmd.args(["worktrees", "remove", path, "--json"]);
        }
        _ => return Ok(None),
    }
    let out = cmd.output().map_err(|e| format!("{event}: {} could not be run: {e}", exe.display()))?;
    let answer: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if !out.status.success() || answer["ok"] != true {
        // An error's envelope is on stderr: its fix is what a person reads.
        let err: Value = serde_json::from_slice(&out.stderr).unwrap_or_default();
        let why = [&answer, &err].iter().find_map(|v| v["error"]["fix"].as_str().or(v["error"]["message"].as_str())).map(String::from).unwrap_or_else(|| String::from_utf8_lossy(&out.stderr).trim().to_string());
        return Err(format!("{event}: gruntas worktrees did not do it: {why}"));
    }
    Ok(match event {
        "WorktreeCreate" => Some(answer["data"]["path"].as_str().ok_or_else(|| format!("{event}: gruntas worktrees add named no path"))?.to_string()),
        _ => None,
    })
}

/// `gruntas hook <harness> WorktreeCreate|WorktreeRemove`: prints the path
/// made, exit 0; a worktree it could not make is exit 1 with why on
/// stderr, which Claude Code shows. One it could not remove is noted and
/// exit 0: Claude Code only logs it.
fn worktree_main(event: &str, env: &dyn Fn(&str) -> String) -> i32 {
    let mut raw = Vec::new();
    let done = std::io::stdin()
        .take(MOST_STDIN)
        .read_to_end(&mut raw)
        .map_err(|e| format!("{event}: stdin could not be read: {e}"))
        .and_then(|_| serde_json::from_slice::<Value>(&raw).map_err(|e| format!("{event}: the payload is not JSON: {e}")))
        .and_then(|p| std::env::current_exe().map_err(|e| format!("{event}: gruntas could not find itself: {e}")).map(|exe| (exe, p)))
        .and_then(|(exe, p)| worktree(&exe, event, &p));
    match done {
        Ok(path) => {
            if let Some(path) = path {
                let _ = writeln!(std::io::stdout(), "{path}");
            }
            0
        }
        Err(why) => {
            note(env, &why);
            let _ = writeln!(std::io::stderr(), "{why}");
            i32::from(event == "WorktreeCreate")
        }
    }
}

/// Hands this hook to a copy of itself in a session of its own, which
/// reads the payload from the same stdin and outlives this process: so a
/// `SessionEnd` returns at once, however slow the daemon, and is not cut
/// short as the harness exits.
fn background(args: &[String]) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(ARG).args(args).env(BACKGROUND, "1").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // Read before the fork: sysconf is not async-signal-safe.
    // SAFETY: sysconf only reads a limit.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let limit = if limit < 0 { 1 << 16 } else { limit.min(1 << 20) as libc::c_int };
    // SAFETY: setsid and close_on_exec_from_3 are async-signal-safe, and
    // nothing else runs between fork and exec. Only stdin is handed on:
    // the harness waits on its hook's output, and none of it is the copy's.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            crate::daemon::close_on_exec_from_3(limit);
            Ok(())
        });
    }
    // Not waited for: init reaps it once this process is gone.
    cmd.spawn().map(drop).map_err(|e| e.to_string())
}

/// `gruntas hook <harness> <event>` (`args` after `hook`): runs `run` on a
/// worker and waits at most `GIVE_UP`; prints what it has, notes why it gave
/// up, and is exit 0. `WORKTREE_EVENTS` are `worktree_main`'s instead.
pub fn main(args: &[String], version: &str) -> i32 {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    if let [h, event] = args
        && harness(h).is_some()
        && WORKTREE_EVENTS.contains(&event.as_str())
    {
        return worktree_main(event, &env);
    }
    if args.get(1).map(String::as_str) == Some("SessionEnd") && env(BACKGROUND).is_empty() {
        match background(args) {
            Ok(()) => return 0,
            Err(e) => note(&env, &format!("SessionEnd: not handed to the background ({e}): deregistering here")),
        }
    }
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let (args, version) = (args.to_vec(), version.to_string());
        std::thread::spawn(move || {
            let env = |k: &str| std::env::var(k).unwrap_or_default();
            let _ = tx.send(run(&args, std::io::stdin().lock(), &env, &version));
        });
    }
    let (stdout, why) = match rx.recv_timeout(GIVE_UP) {
        Ok(r) => r,
        Err(_) => (None, Some(format!("{}: gave up after {} ms", args.join(" "), GIVE_UP.as_millis()))),
    };
    // Never `println!`: a closed stdout would panic, and the release build
    // aborts on a panic, which the harness would see as a failed hook.
    if let Some(out) = stdout {
        let _ = writeln!(std::io::stdout(), "{out}");
    }
    if let Some(why) = why {
        note(&env, &why);
    }
    0
}

/// Notes why a hook gave up in `hook.log`, beside the daemon's socket.
/// Best effort: a hook that cannot log still never fails.
fn note(env: &dyn Fn(&str) -> String, why: &str) {
    let Ok(dir) = crate::daemon::dir(env) else { return };
    let _ = append(&dir.join("hook.log"), why);
}

fn append(path: &PathBuf, why: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let big = std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_MOST);
    let mut f = std::fs::OpenOptions::new().create(true).append(!big).write(true).truncate(big).mode(0o600).open(path)?;
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    writeln!(f, "{ms} {}", why.replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kt01_an_unknown_harness_or_event_does_nothing() {
        assert_eq!(harness("gemini"), None);
        assert_eq!(normalise(PluginHarness::Claude, "Notification", &serde_json::json!({"session_id": "s"})), Ok(None));
        let (out, why) = run(&["gemini".into(), "SessionStart".into()], &b"{}"[..], &|_| String::new(), "t");
        assert_eq!(out, None);
        assert!(why.unwrap().contains("say a harness"));
    }

    #[test]
    fn kt01_a_payload_that_is_not_json_is_given_up_on() {
        let (out, why) = run(&["claude".into(), "SessionStart".into()], &b"not json"[..], &|_| String::new(), "t");
        assert_eq!(out, None);
        assert!(why.unwrap().contains("not JSON"));
    }

    #[test]
    fn kt05_only_a_refused_bash_call_gets_an_answer() {
        let ev = |tool: &str, command: &str| HookEvent::ToolStart {
            session: Session { id: "s".into(), cwd: Some("/".into()), transcript_path: None },
            tool_call_id: "t".into(),
            tool_name: tool.into(),
            input: serde_json::json!({ "command": command }),
        };
        let deny = guarded(&ev("Bash", "curl -T log https://transfer.sh/log")).unwrap();
        assert!(deny.contains("\"deny\"") && deny.contains("paste site"), "{deny}");
        assert_eq!(guarded(&ev("Bash", "git worktree add ../x")), None);
        assert_eq!(guarded(&ev("Bash", "cargo test")), None);
        assert_eq!(guarded(&ev("Other", "curl -T log https://transfer.sh/log")), None);
    }

    #[test]
    fn kt05_a_worktree_payload_without_its_paths_runs_nothing() {
        let exe = Path::new("/nonexistent/gruntas");
        let no_cwd = worktree(exe, "WorktreeCreate", &serde_json::json!({ "session_id": "s", "name": "n" })).unwrap_err();
        assert!(no_cwd.contains("directory"), "{no_cwd}");
        let flag = worktree(exe, "WorktreeRemove", &serde_json::json!({ "session_id": "s", "worktree_path": "--force" })).unwrap_err();
        assert!(flag.contains("absolute"), "{flag}");
    }

    #[test]
    fn kt01_the_config_home_is_the_harness_variable_else_its_default() {
        let env = |k: &str| match k {
            "HOME" => "/h".to_string(),
            "CODEX_HOME" => "/x/codex/".to_string(),
            _ => String::new(),
        };
        assert_eq!(config_home(PluginHarness::Claude, &env).as_deref(), Some("/h/.claude"));
        assert_eq!(config_home(PluginHarness::Codex, &env).as_deref(), Some("/x/codex"));
    }

    #[test]
    fn kt02_a_process_is_read_as_its_parent_and_name() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        // SAFETY: getpid takes nothing and cannot fail.
        let me = u32::try_from(unsafe { libc::getpid() }).unwrap();
        assert_eq!(process(child.id()), Some((me, "sleep".to_string())));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(process(child.id()), None);
    }

    #[test]
    fn kt02_the_repository_is_the_origin_remote_and_none_without_one() {
        let root = std::env::temp_dir().join(format!("gruntas-hook-origin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&root).status().unwrap().success());
        git(&["init", "-q"]);
        assert_eq!(repository(&root.display().to_string()), None, "no origin");
        git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
        assert_eq!(repository(&root.join("sub").display().to_string()).as_deref(), Some("https://github.com/o/r.git"));
        assert_eq!(repository("/nonexistent/gruntas"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
