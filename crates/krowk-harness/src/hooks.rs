//! Claude-format command hooks (R-COMPAT-1), run by krowk's native loop:
//! the `hooks` of the settings `permissions::settings` reads, unchanged
//! (read by `permissions::hooks`) —
//!
//! ```json
//! {"hooks": {"PreToolUse": [{"matcher": "Bash|Edit", "hooks": [{"type": "command", "command": "./check.sh", "timeout": 30}]}]}}
//! ```
//!
//! | event | when | matcher | exit 2 |
//! |---|---|---|---|
//! | `SessionStart` | a session's first turn (`startup`) or a resumed session's first turn in this host (`resume`) | the source | — (stdout is context) |
//! | `UserPromptSubmit` | a prompt, before the model sees it | — | the prompt is refused, with stderr as the reason |
//! | `PreToolUse` | a tool call, before its permission is judged | the tool, by Claude Code's name | the call is not run; the model reads stderr |
//! | `PostToolUse` | a tool call, after it ran | the tool | the model reads stderr beside the result |
//! | `Stop` | the model is done | — | the turn goes on; the model reads stderr |
//! | `SubagentStop` | a subagent is done (its turn fires neither `Stop` nor `UserPromptSubmit`) | — | the subagent goes on; it reads stderr |
//!
//! A subagent's hooks are given its parent's `session_id` and
//! `transcript_path`, as Claude Code gives them, and its own beside them:
//! `agent_session_id`, `agent_transcript_path`, and `agent_type` when it
//! runs a definition.
//!
//! A hook is `sh -c <command>` in the session's directory, in its own
//! process group, with the event as JSON on stdin (`session_id`,
//! `transcript_path`, `cwd`, `permission_mode`, `hook_event_name`, and the
//! event's own fields: `tool_name`, `tool_input`, `tool_response`,
//! `prompt`, `source`, `stop_hook_active`), and `CLAUDE_PROJECT_DIR` set to
//! the repository root — what Claude Code gives one, so a script written
//! for it runs as it is. It has `timeout` seconds (60 by default); at the
//! timeout, or when the turn is interrupted, its group is killed. Exit 0 is
//! success, and a JSON object on stdout is read as Claude Code reads it:
//! `continue: false` stops the turn with its `stopReason` (shown to the
//! person), `systemMessage` is shown to the person, `decision`/`reason`,
//! `hookSpecificOutput.permissionDecision` and `.additionalContext` are
//! the model's. Exit 2 blocks, with stderr as the reason; any other exit is
//! a failure that blocks nothing. Not read: `suppressOutput` (krowk shows
//! no hook output anywhere to suppress), a `PreToolUse` hook's
//! `updatedInput` (a hook cannot rewrite what the model asked for), and a
//! `timeout` longer than a day, which is held to a day.
//!
//! **Hooks are commands, so where they come from matters**: only the
//! person's own settings, and a repository's once it is trusted — never a
//! file the model can write (see `permissions::settings`). A backend runs
//! its own vendor's hooks itself (Claude Code reads the same files).

pub use crate::permissions::hooks::{parse, Decision, Event, Hooks};
use crate::permissions::hooks::Command;
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::watch;

/// What of a hook's stdout and stderr is kept.
const MAX_OUTPUT: usize = 64 << 10;

/// A matcher against a tool's name (or a session's source): a regular
/// expression over the whole name, as Claude Code's are (`Edit|Write`,
/// `mcp__.*`), or the name itself when it is not one.
fn matcher_matches(m: &str, subject: &str) -> bool {
    match regex_lite::Regex::new(&format!("^(?:{m})$")) {
        Ok(re) => re.is_match(subject),
        Err(_) => m == subject,
    }
}

/// What the hooks of one event said, taken together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// A hook blocked: exit 2, or `decision: "block"` / a `deny`. The
    /// reason is the model's (or, for a prompt, the person's) to read.
    pub block: Option<String>,
    /// A `PreToolUse` hook's `permissionDecision`: allow or ask.
    pub decision: Option<Decision>,
    /// Text for the model: stdout of a `SessionStart` or `UserPromptSubmit`
    /// hook, and any hook's `additionalContext`.
    pub context: Vec<String>,
    /// Hooks that failed without blocking (another exit, a timeout).
    pub failures: Vec<String>,
    /// A hook said `continue: false`: the turn stops, with its
    /// `stopReason` — for the person, not the model.
    pub stop: Option<String>,
    /// `systemMessage`s: shown to the person, never to the model.
    pub messages: Vec<String>,
}

/// The fields every event's input carries.
#[derive(Debug, Clone)]
pub struct Base<'a> {
    pub session_id: &'a str,
    pub transcript_path: &'a str,
    pub cwd: &'a Path,
    pub project_dir: &'a Path,
    pub permission_mode: &'a str,
}

/// Runs every hook of `event` whose matcher takes `subject`, one after
/// another, with `fields` added to the input. An interrupt stops the one
/// running and the rest.
pub async fn run(hooks: &Hooks, event: Event, subject: Option<&str>, base: &Base<'_>, fields: Value, cancel: &watch::Receiver<bool>) -> Outcome {
    let mut out = Outcome::default();
    let mut input = json!({
        "session_id": base.session_id,
        "transcript_path": base.transcript_path,
        "cwd": base.cwd.display().to_string(),
        "permission_mode": base.permission_mode,
        "hook_event_name": event.name(),
    });
    if let (Some(m), Value::Object(f)) = (input.as_object_mut(), fields) {
        m.extend(f);
    }
    let body = input.to_string();
    for g in hooks.groups.iter().filter(|g| g.event == event) {
        if let (Some(m), Some(s)) = (&g.matcher, subject)
            && !matcher_matches(m, s)
        {
            continue;
        }
        for c in &g.commands {
            if *cancel.borrow() {
                return out;
            }
            let ran = exec(c, &body, base, cancel.clone()).await;
            read(event, c, &g.source, ran, &mut out);
            if out.block.is_some() || out.stop.is_some() {
                return out;
            }
        }
    }
    out
}

/// How one hook process came out.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn read(event: Event, c: &Command, source: &str, ran: Result<Ran, String>, out: &mut Outcome) {
    let ran = match ran {
        Ok(r) => r,
        Err(e) => {
            out.failures.push(format!("the {} hook `{}` ({source}) could not run: {e}", event.name(), c.command));
            return;
        }
    };
    if ran.timed_out {
        out.failures.push(format!("the {} hook `{}` ({source}) did not finish within {} seconds and was stopped", event.name(), c.command, c.timeout.as_secs()));
        return;
    }
    match ran.code {
        Some(0) => {}
        Some(2) => {
            let why = ran.stderr.trim();
            out.block = Some(if why.is_empty() { format!("the {} hook `{}` blocked it without saying why", event.name(), c.command) } else { why.to_string() });
            return;
        }
        code => {
            let said = ran.stderr.trim();
            out.failures.push(format!("the {} hook `{}` ({source}) failed ({}){}", event.name(), c.command, code.map_or("killed".to_string(), |c| format!("exit {c}")), if said.is_empty() { String::new() } else { format!(": {said}") }));
            return;
        }
    }
    let stdout = ran.stdout.trim();
    let Some(j) = serde_json::from_str::<Value>(stdout).ok().filter(Value::is_object) else {
        // Plain text is context for the two events whose output Claude
        // Code adds to the conversation.
        if matches!(event, Event::SessionStart | Event::UserPromptSubmit) && !stdout.is_empty() {
            out.context.push(stdout.to_string());
        }
        return;
    };
    let reason = |k: &str| j.get(k).and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty()).map(String::from);
    let specific = j.get("hookSpecificOutput");
    if let Some(m) = reason("systemMessage") {
        out.messages.push(m);
    }
    if j.get("continue").and_then(Value::as_bool) == Some(false) {
        out.stop = Some(reason("stopReason").unwrap_or_else(|| format!("the {} hook `{}` stopped the turn", event.name(), c.command)));
        return;
    }
    if let Some(ctx) = specific.and_then(|s| s.get("additionalContext")).and_then(Value::as_str).filter(|c| !c.trim().is_empty()) {
        out.context.push(ctx.trim().to_string());
    }
    let why = |fallback: &str| reason("reason").unwrap_or_else(|| format!("the {} hook `{}` {fallback}", event.name(), c.command));
    match j.get("decision").and_then(Value::as_str) {
        Some("block") => out.block = Some(why("blocked it")),
        Some("approve") if event == Event::PreToolUse => out.decision = Some(Decision::Allow),
        _ => {}
    }
    if event == Event::PreToolUse {
        let pd = specific.and_then(|s| s.get("permissionDecision")).and_then(Value::as_str);
        let pr = specific.and_then(|s| s.get("permissionDecisionReason")).and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty()).map(String::from);
        match pd {
            Some("deny") => out.block = Some(pr.unwrap_or_else(|| why("denied it"))),
            Some("ask") => out.decision = Some(Decision::Ask),
            Some("allow") if out.decision != Some(Decision::Ask) => out.decision = Some(Decision::Allow),
            _ => {}
        }
    }
}

async fn exec(c: &Command, input: &str, base: &Base<'_>, mut cancel: watch::Receiver<bool>) -> Result<Ran, String> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(&c.command).current_dir(base.cwd).env("CLAUDE_PROJECT_DIR", base.project_dir);
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let body = input.to_string();
        // Written beside the wait: a hook that never reads its input must
        // not hold krowk on a full pipe.
        tokio::spawn(async move {
            let _ = stdin.write_all(body.as_bytes()).await;
        });
    }
    let kill = || {
        #[cfg(unix)]
        if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
            // SAFETY: the group this hook was started in (process_group(0)).
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    };
    let cut = |b: Vec<u8>| String::from_utf8_lossy(&b[..b.len().min(MAX_OUTPUT)]).into_owned();
    tokio::select! {
        r = child.wait_with_output() => {
            let o = r.map_err(|e| e.to_string())?;
            kill();
            Ok(Ran { code: o.status.code(), stdout: cut(o.stdout), stderr: cut(o.stderr), timed_out: false })
        }
        _ = tokio::time::sleep(c.timeout) => {
            kill();
            Ok(Ran { code: None, stdout: String::new(), stderr: String::new(), timed_out: true })
        }
        _ = crate::engine::cancelled(&mut cancel) => {
            kill();
            Ok(Ran { code: None, stdout: String::new(), stderr: "interrupted".into(), timed_out: false })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn base(dir: &Path) -> Base<'_> {
        Base { session_id: "s", transcript_path: "/t", cwd: dir, project_dir: dir, permission_mode: "default" }
    }

    fn hooks(v: Value) -> Hooks {
        parse(&v, "test").unwrap()
    }

    #[tokio::test]
    async fn r_compat_1_hooks_load_in_claude_codes_format_and_follow_its_exit_codes() {
        let d = std::env::temp_dir().join(format!("krowk-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (_tx, cancel) = watch::channel(false);
        let h = hooks(json!({
            "PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "cat > seen.txt; echo 'no rm here' >&2; exit 2"}]},
                {"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "echo '{\"hookSpecificOutput\": {\"permissionDecision\": \"ask\"}}'"}]}
            ],
            "UserPromptSubmit": [{"hooks": [{"command": "echo 'the build is red'"}]}],
            "Notification": [{"hooks": [{"command": "true"}]}],
            "Stop": [{"hooks": [{"type": "prompt", "prompt": "x"}]}]
        }));
        assert!(!h.has(Event::Stop), "a hook type krowk does not run is skipped");
        let o = run(&h, Event::PreToolUse, Some("Bash"), &base(&d), json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf x"}}), &cancel).await;
        assert_eq!(o.block.as_deref(), Some("no rm here"), "exit 2 blocks with stderr");
        assert!(std::fs::read_to_string(d.join("seen.txt")).unwrap().contains("rm -rf x"), "the input arrives on stdin");
        let o = run(&h, Event::PreToolUse, Some("Read"), &base(&d), json!({}), &cancel).await;
        assert_eq!(o, Outcome::default(), "a matcher that does not take the tool");
        let o = run(&h, Event::PreToolUse, Some("Write"), &base(&d), json!({}), &cancel).await;
        assert_eq!(o.decision, Some(Decision::Ask));
        let o = run(&h, Event::UserPromptSubmit, None, &base(&d), json!({"prompt": "hi"}), &cancel).await;
        assert_eq!(o.context, ["the build is red"]);
        let slow = hooks(json!({"Stop": [{"hooks": [{"command": "sleep 5", "timeout": 0.2}]}], "PostToolUse": [{"hooks": [{"command": "exit 3"}]}]}));
        let t = std::time::Instant::now();
        let o = run(&slow, Event::Stop, None, &base(&d), json!({}), &cancel).await;
        assert!(o.block.is_none() && o.failures[0].contains("did not finish") && t.elapsed() < Duration::from_secs(3), "{o:?}");
        let o = run(&slow, Event::PostToolUse, Some("Bash"), &base(&d), json!({}), &cancel).await;
        assert!(o.block.is_none() && o.failures[0].contains("exit 3"), "another exit blocks nothing: {o:?}");
        assert!(parse(&json!({"PreToolUse": [{"hooks": [{"type": "command"}]}]}), "f").unwrap_err().contains("needs a \"command\""));
        // A timeout that would overflow the clock, or is not a duration at
        // all, is named with its file — never a panic.
        for bad in [json!(-1), json!(0), json!("soon"), json!(1e300)] {
            let r = parse(&json!({"Stop": [{"hooks": [{"command": "true", "timeout": bad}]}]}), "/x/settings.json");
            if bad == json!(1e300) {
                assert!(r.is_ok(), "a huge timeout is held to a day");
            } else {
                assert!(r.unwrap_err().contains("/x/settings.json"), "{bad}");
            }
        }
        // continue:false stops, with its reason; systemMessage is the person's.
        let stop = hooks(json!({"Stop": [{"hooks": [{"command": "echo '{\"continue\": false, \"stopReason\": \"the build broke\", \"systemMessage\": \"see CI\"}'"}]}]}));
        let o = run(&stop, Event::Stop, None, &base(&d), json!({}), &cancel).await;
        assert_eq!((o.stop.as_deref(), o.messages.as_slice()), (Some("the build broke"), ["see CI".to_string()].as_slice()));
        let _ = std::fs::remove_dir_all(&d);
    }
}
