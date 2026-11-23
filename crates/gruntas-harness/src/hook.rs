//! `gruntas hook <harness> <event>`: the one entry point every hook of
//! gruntas's plugin runs, in Claude Code and in Codex (D3). It reads the
//! harness's payload on stdin, turns it into one internal event
//! (`normalise`), and sends what the daemon keeps of it as an `activity`
//! frame over the daemon's socket (`to_activity`, `send`).
//!
//! A hook never blocks or fails its session: whatever goes wrong — no
//! daemon, a stalled one, a payload it cannot read — it gives up silently,
//! notes why in `hook.log` beside the daemon's socket, and exits 0 with
//! nothing on stdout, so the harness carries on as if there were no hook.
//! Everything runs on a worker thread the main thread waits on for at most
//! `GIVE_UP`: past that the process exits whatever the worker is doing, so
//! the hook is done well inside the 2 seconds it is allowed.

use crate::protocol::{Activity, ClientFrame, PluginHarness, Registration, ServerFrame, UsageWindow, PROTOCOL_VERSION};
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
    /// And done: `PostToolUse`.
    ToolEnd { session: Session, tool_call_id: String, tool_name: String },
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
        "PostToolUse" => {
            let t = tool()?;
            HookEvent::ToolEnd { session: session()?, tool_call_id: t.tool_use_id, tool_name: t.tool_name }
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
/// only when its command is on the heavy list, which is KT03's; the turn's
/// end likewise.
pub fn to_activity(harness: PluginHarness, event: &HookEvent, ctx: &Context) -> Option<Activity> {
    match event {
        HookEvent::SessionStart { session, .. } => Some(Activity::Register(Registration {
            session_id: session.id.clone(),
            harness,
            harness_pid: ctx.harness_pid,
            config_home: ctx.config_home.clone(),
            cwd: session.cwd.clone(),
            repository: None,
            transcript_path: session.transcript_path.clone(),
            attendance: None,
        })),
        HookEvent::SessionEnd { session } => Some(Activity::Deregister { session_id: session.id.clone() }),
        HookEvent::ToolStart { .. } | HookEvent::ToolEnd { .. } | HookEvent::Stop { .. } => None,
        HookEvent::Usage { config_home, windows } => Some(Activity::Usage { config_home: config_home.clone(), windows: windows.clone() }),
    }
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

/// Sends `activity` to the daemon on `socket` and waits for its answer:
/// the hello and the frame in one write, then the writing half closed, so
/// a daemon too old to know the frame closes the connection at once rather
/// than leaving the hook to wait. Blocks: the caller bounds it (`GIVE_UP`).
pub fn send(socket: &Path, cwd: &str, version: &str, activity: Activity) -> Result<(), String> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).map_err(|e| format!("no daemon on {}: {e}", socket.display()))?;
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
    let ctx = Context { harness_pid: None, config_home: config_home(h, env) };
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
    match send(&socket, &cwd, version, activity) {
        Ok(()) => (None, None),
        Err(e) => (None, Some(format!("{event}: {e}"))),
    }
}

/// `gruntas hook <harness> <event>` (`args` after `hook`): runs `run` on a
/// worker and waits at most `GIVE_UP`; prints what it has, notes why it gave
/// up, and is always exit 0.
pub fn main(args: &[String], version: &str) -> i32 {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
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
    fn kt01_the_config_home_is_the_harness_variable_else_its_default() {
        let env = |k: &str| match k {
            "HOME" => "/h".to_string(),
            "CODEX_HOME" => "/x/codex/".to_string(),
            _ => String::new(),
        };
        assert_eq!(config_home(PluginHarness::Claude, &env).as_deref(), Some("/h/.claude"));
        assert_eq!(config_home(PluginHarness::Codex, &env).as_deref(), Some("/x/codex"));
    }
}
