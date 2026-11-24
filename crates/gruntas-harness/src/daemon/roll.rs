//! Rollover for an unattended Claude Code worker (Rollover for launched
//! sessions D4, R-RL-4): when its account's usage runs out, the ladder
//! (`ladder::Ladder::next`) picks the next account; on another Claude Code
//! account, with `rollover` `auto` (which the person acknowledged), the
//! daemon stops the background session, copies its transcript into that
//! account's config home (`handoff::carry`) and resumes it there in the
//! background (`claude --bg --resume <id>`), re-sending the prompt the
//! limit interrupted. On the other harness's rung it stops the session and
//! continues it there from a handoff (`Cross`, the launcher's
//! `continue_on`). Otherwise the session waits, input-required, naming the
//! next account.
//!
//! A Codex worker runs in the host daemon's Codex engine, whose shipped
//! rollover moves it: nothing here.

use super::tasks::{Launch, Worker};
use crate::handoff::{carry, Vendor};
use crate::instances::{Registry, Resolved};
use crate::ladder::Rung;
use crate::protocol::{ModelRef, Rollover};
use crate::usage::{home_key, Usage};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How long `claude --bg --resume` may take to hand the session over.
const RESUME_WAIT: Duration = Duration::from_secs(60);

/// What became of a worker on an account out of usage.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// It runs on `account` now, as `launch`.
    Moved { account: String, launch: Launch, notice: String },
    /// It was stopped and continued on the other harness as `session`.
    Continued { session: String, notice: String },
    /// It waits on the person, for this reason.
    Held(String),
}

/// Continues a session (the second argument) on the other harness at the
/// ladder's step: the new session's id (the launcher's `continue_on`).
/// The third is the session's working directory, where its handoff starts.
pub type Cross = std::sync::Arc<dyn Fn(&crate::ladder::Step, &str, Option<&Path>) -> Result<String, String> + Send + Sync>;

/// The session's registration, as far as a move needs it.
#[derive(Debug, Clone, Default)]
pub struct Place {
    pub cwd: Option<String>,
    pub transcript: Option<String>,
}

/// `w`, whose account in `limited_home` is out of usage, moved or held;
/// none when `w` runs on another account.
// blocking: reads the usage record, stops and starts `claude`, copies files.
pub fn roll(w: &Worker, at: &Place, limited_home: &str, registry: &Registry, usage: &Usage, cross: Option<&Cross>, now_ms: i64) -> Option<Outcome> {
    let (from, from_home) = account_of(registry, w.launch.config_home.as_deref())?;
    if home_key(&from_home.display().to_string()) != home_key(limited_home) {
        return None;
    }
    let model = flag(&w.launch.args, "--model").or_else(|| crate::instances::default_model_of("claude-code").map(String::from)).unwrap_or_default();
    let data = usage.read();
    // No spend reading of a background worker reaches the daemon, so the
    // key rungs stay closed to it (D3).
    let keys = crate::ladder::keys_open(crate::launch::Harness::Claude, false, None, None);
    let Some(step) = registry.ladder.next(registry, &data, &ModelRef { instance: from.name.clone(), model }, keys, now_ms) else {
        return Some(Outcome::Held(format!("{} is out of usage, and no other account has room", from.name)));
    };
    let notice = crate::ladder::notice(&step, registry.ladder.key_cap_usd);
    let to = step.to.instance.clone();
    if registry.rollover != Rollover::Auto {
        return Some(Outcome::Held(format!("{} is out of usage; the next account is {to}. `gruntas config set rollover auto` moves an unattended session there on its own", from.name)));
    }
    if let (Rung::OtherHarness, Some(cross)) = (step.rung, cross) {
        let continued = super::tasks::stop(&w.id, &w.launch).and_then(|()| cross(&step, &w.session_id, at.cwd.as_deref().map(Path::new)));
        return Some(match continued {
            Ok(session) => Outcome::Continued { session, notice },
            Err(e) => Outcome::Held(format!("{} is out of usage, and continuing on {to} failed: {e}", from.name)),
        });
    }
    if step.rung != Rung::SameHarness {
        return Some(Outcome::Held(format!("{} is out of usage; the next account is {to}, on the other harness", from.name)));
    }
    let Ok(target) = registry.get(&to) else { return Some(Outcome::Held(format!("{} is out of usage; the next account, {to}, cannot be found", from.name))) };
    Some(match resume(w, at, &from_home, target) {
        Ok(launch) => {
            if let Err(e) = usage.mark_moved(data.login_of(&to), now_ms) {
                eprintln!("{to} not marked as moved to: {e}");
            }
            Outcome::Moved { account: to, launch, notice }
        }
        Err(e) => Outcome::Held(format!("{} is out of usage, and moving to {to} failed: {e}", from.name)),
    })
}

/// The Claude Code account a launch's config home is (none: Claude Code's
/// own, the `claude` account's), with that home.
fn account_of<'r>(registry: &'r Registry, config_home: Option<&str>) -> Option<(&'r Resolved, PathBuf)> {
    let home = match config_home {
        Some(h) => PathBuf::from(h),
        None => registry.instances.get("claude")?.backend.as_ref()?.home.clone()?,
    };
    let key = home_key(&home.display().to_string());
    let mut on = registry.instances.values().filter(|i| i.kind == "claude-code" && i.api_key_env.is_empty() && i.backend.as_ref().and_then(|b| b.home.as_ref()).is_some_and(|h| home_key(&h.display().to_string()) == key));
    let first = on.next()?;
    // Several accounts on one home: the default's name, as it was launched.
    let pick = match config_home {
        None if first.name != "claude" => on.find(|i| i.name == "claude").unwrap_or(first),
        _ => first,
    };
    Some((pick, home))
}

/// The move itself: the session stopped, its transcript carried into
/// `to`'s home, and resumed there in the background, the interrupted
/// prompt sent again.
fn resume(w: &Worker, at: &Place, from_home: &Path, to: &Resolved) -> Result<Launch, String> {
    let b = to.backend.as_ref().ok_or_else(|| format!("{} is not a Claude Code account", to.name))?;
    let to_home = b.home.as_ref().ok_or_else(|| format!("{}'s config directory is not known", to.name))?;
    let transcript = at.transcript.as_deref().ok_or("its transcript's place is not known")?;
    let cwd = at.cwd.as_deref().filter(|d| Path::new(d).is_absolute()).ok_or("its working directory is not known")?;
    super::tasks::stop(&w.id, &w.launch)?;
    carry(Path::new(transcript), from_home, to_home, Vendor::ClaudeCode, &w.session_id)?;
    let prompt = if w.mid_turn || ended_on_limit(Path::new(transcript)) { last_prompt(Path::new(transcript)) } else { None };
    let program = b.path.clone().unwrap_or_else(|| PathBuf::from(&b.binary));
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&w.launch.args).args(["--resume", &w.session_id]).args(prompt).current_dir(cwd).stdin(Stdio::null());
    let (remove, set) = crate::claude::environment(b);
    for k in remove {
        cmd.env_remove(k);
    }
    cmd.envs(set);
    // The default account keeps the daemon's own CLAUDE_CONFIG_DIR, as its
    // home (where the transcript went) was read from it.
    let config_home = b.config_dir.as_ref().map(|d| d.display().to_string()).or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok().filter(|d| !d.is_empty()));
    // It hands the session to its background service and returns; one
    // that does not within `RESUME_WAIT` is killed, as nobody watches.
    let mut child = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{} could not be run: {e}", program.display()))?;
    let err = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut said = String::new();
        if let Some(e) = err {
            let _ = std::io::Read::read_to_string(&mut std::io::Read::take(e, 16 << 10), &mut said);
        }
        let _ = tx.send(said);
    });
    let deadline = Instant::now() + RESUME_WAIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`claude --bg --resume` did not finish within {}s", RESUME_WAIT.as_secs()));
            }
        }
    };
    if !status.success() {
        let said = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
        return Err(format!("`claude --bg --resume` failed ({status}): {}", said.trim()));
    }
    Ok(Launch { program: program.display().to_string(), config_home, args: w.launch.args.clone() })
}

/// `--model X` or `--model=X` among `args`.
fn flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(name).and_then(|v| v.strip_prefix('=')) {
            return Some(v.to_string());
        }
    }
    None
}

/// The transcript's lines, as JSON.
fn lines(transcript: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(transcript).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Whether the session's last turn ended on the usage limit: Claude Code
/// writes the limit's error as its last assistant line.
fn ended_on_limit(transcript: &Path) -> bool {
    lines(transcript).iter().rev().find(|l| l["type"] == "assistant").is_some_and(|l| l["isApiErrorMessage"] == true && l["error"] == "rate_limit")
}

/// The last prompt the person (or the launcher) gave: the last user line
/// that is text, not a tool's result, Claude Code's own reminders left out.
fn last_prompt(transcript: &Path) -> Option<String> {
    lines(transcript).iter().rev().filter(|l| l["type"] == "user" && l["isMeta"] != true && l["isSidechain"] != true).find_map(|l| {
        let text = match &l["message"]["content"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) if parts.iter().all(|p| p["type"] == "text") => parts.iter().filter_map(|p| p["text"].as_str()).filter(|t| !t.starts_with('<')).collect::<Vec<_>>().join("\n"),
            _ => return None,
        };
        let text = text.trim();
        (!text.is_empty() && !text.starts_with('<')).then(|| text.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rl02_the_interrupted_prompt_is_the_last_typed_one_and_a_limit_is_its_error_line() {
        let dir = std::env::temp_dir().join(format!("gruntas-roll-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"first"}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>x</system-reminder>"},{"type":"text","text":"fix the bug"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"caveat"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#,
            r#"{"type":"assistant","isApiErrorMessage":true,"error":"rate_limit","message":{"role":"assistant","content":[{"type":"text","text":"You've hit your limit"}]}}"#,
        ];
        std::fs::write(&t, lines.join("\n")).unwrap();
        assert_eq!(last_prompt(&t).as_deref(), Some("fix the bug"));
        assert!(ended_on_limit(&t));
        std::fs::write(&t, lines[..4].join("\n")).unwrap();
        assert!(!ended_on_limit(&t), "an ordinary last answer");
        assert_eq!(flag(&["--bg".into(), "--model=opus".into()], "--model").as_deref(), Some("opus"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
