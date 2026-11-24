//! Attended sessions the Launcher started, and the account each goes on
//! to after `/exit` (Rollover for launched sessions D5, R-RL-5).
//!
//! `gruntas run` stays the harness's parent (`gruntas run claude`, no
//! `--unattended`). As it starts the harness it writes `Launched` to
//! `<daemon dir>/attended/<harness pid>.json`: that pid's session is one
//! the Launcher will carry on. When that account's usage meets rollover's
//! early-switch rule or its limit, the daemon (`due`) queues the ladder's
//! pick as `Next` in `<pid>.next.json` beside it and tells the session
//! through its inbox (`sync::god::inbox::Steer::Notice`), which the
//! plugin's mod shows. Once the harness exits, the Launcher takes the
//! record (`take`) and resumes the same session on that account, or hands
//! it to the other harness.
//!
//! `Next` is the plain record of where `/exit` continues: anyone who
//! queues an account for a session (a person's pick, too) writes it with
//! `queue`. One already queued is kept: a later reading never replaces it.

use crate::instances::Registry;
use crate::ladder::Rung;
use crate::launch::Harness;
use crate::protocol::{Attendance, LimitState, LimitStatus, ModelRef, PluginHarness, Registration, UsageWindow};
use crate::usage::Data;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The records' directory, in the daemon's.
pub const DIR: &str = "attended";

/// What the Launcher started, as it writes it beside the harness's pid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Launched {
    /// The account (instance) it runs on.
    pub account: String,
    /// The model it runs: the one the person passed, else the account's
    /// default.
    pub model: String,
}

/// The account `/exit` continues a session on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Next {
    /// The harness's own id of the session to continue.
    pub session: String,
    /// The account (instance) to continue on.
    pub account: String,
    /// The model there; empty → the account's default.
    #[serde(default)]
    pub model: String,
    pub rung: Rung,
    /// The session's transcript, where its harness keeps it on the account
    /// it leaves; copied to the next account's home before resuming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

pub fn launched_path(daemon: &Path, pid: u32) -> PathBuf {
    daemon.join(DIR).join(format!("{pid}.json"))
}

pub fn next_path(daemon: &Path, pid: u32) -> PathBuf {
    daemon.join(DIR).join(format!("{pid}.next.json"))
}

fn write(daemon: &Path, file: &str, value: &impl Serialize) -> std::io::Result<()> {
    let dir = daemon.join(DIR);
    gruntas_api::home::make(&dir).map_err(std::io::Error::other)?;
    crate::daemon::registry::write_private(&dir, file, &serde_json::to_vec(value).map_err(std::io::Error::other)?)
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The Launcher's mark on the harness `pid` it started; any `Next` an
/// earlier harness of that pid left is dropped.
pub fn mark(daemon: &Path, pid: u32, launched: &Launched) -> std::io::Result<()> {
    let _ = std::fs::remove_file(next_path(daemon, pid));
    write(daemon, &format!("{pid}.json"), launched)
}

pub fn launched(daemon: &Path, pid: u32) -> Option<Launched> {
    read(&launched_path(daemon, pid))
}

/// `next` queued for the harness `pid`, in place of any queued before.
pub fn queue(daemon: &Path, pid: u32, next: &Next) -> std::io::Result<()> {
    write(daemon, &format!("{pid}.next.json"), next)
}

/// `next` queued for the harness `pid` unless one already is: whether it
/// was. The daemon's way, so two usage reports at once tell a session once.
pub fn offer(daemon: &Path, pid: u32, next: &Next) -> std::io::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = daemon.join(DIR);
    gruntas_api::home::make(&dir).map_err(std::io::Error::other)?;
    let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(next_path(daemon, pid)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    // A torn record would hold the place of a whole one.
    if let Err(e) = f.write_all(&serde_json::to_vec(next).map_err(std::io::Error::other)?) {
        let _ = std::fs::remove_file(next_path(daemon, pid));
        return Err(e);
    }
    Ok(true)
}

pub fn queued(daemon: &Path, pid: u32) -> Option<Next> {
    read(&next_path(daemon, pid))
}

/// Once the harness `pid` exited: what was queued for it, if anything, and
/// both records gone.
pub fn take(daemon: &Path, pid: u32) -> Option<Next> {
    let next = queued(daemon, pid);
    let _ = std::fs::remove_file(next_path(daemon, pid));
    let _ = std::fs::remove_file(launched_path(daemon, pid));
    next
}

/// The harness `pid` now runs session `session` (a `/clear` or a
/// `/resume` in it changes the id): the account queued for it continues
/// that one, from its own transcript.
pub fn follow(daemon: &Path, pid: u32, session: &str, transcript: Option<&str>) -> std::io::Result<()> {
    match queued(daemon, pid) {
        Some(n) if n.session != session => queue(daemon, pid, &Next { session: session.into(), transcript: transcript.map(String::from), ..n }),
        _ => Ok(()),
    }
}

/// The records of harnesses no longer running dropped, so a pid used
/// again is never taken for one the Launcher started.
pub fn sweep(daemon: &Path) {
    let Ok(entries) = std::fs::read_dir(daemon.join(DIR)) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.split('.').next()).and_then(|p| p.parse::<u32>().ok());
        if pid.is_some_and(|p| !crate::daemon::registry::alive(p)) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// What the session is told: the account `/exit` continues on, and why;
/// on the API-key rung, the spend cap `cap_usd` too (D3).
pub fn notice(next: &Next, limited: bool, cap_usd: f64) -> String {
    let why = if limited { "This account has hit its usage limit" } else { "This account's usage is running out" };
    if next.rung == Rung::ApiKey {
        let step = crate::ladder::Step { rung: next.rung, to: ModelRef { instance: next.account.clone(), model: next.model.clone() } };
        return format!("{why}. {} Type /exit to continue this session there.", crate::ladder::notice(&step, cap_usd));
    }
    let rung = match next.rung {
        Rung::SameHarness => "another subscription",
        Rung::OtherHarness | Rung::ApiKey => "a subscription of the other harness, from a handoff",
    };
    let model = if next.model.is_empty() { String::new() } else { format!(" on {}", next.model) };
    format!("{why}. Next: {}{model} ({rung}) — type /exit to continue this session there.", next.account)
}

/// The usage windows a plugin reported, as a turn's limit reports.
fn limits(windows: &[UsageWindow]) -> Vec<LimitStatus> {
    windows
        .iter()
        .filter_map(|w| {
            let window = match w.kind.as_str() {
                "fiveHour" => "five_hour",
                "sevenDay" => "seven_day",
                _ => return None,
            };
            let status = if w.used_percent >= 100.0 { LimitState::Limited } else { LimitState::Allowed };
            Some(LimitStatus { status, window: Some(window.into()), used_percent: Some(w.used_percent), resets_at_ms: w.resets_at_ms.and_then(|r| i64::try_from(r).ok()) })
        })
        .collect()
}

/// The sessions to tell, each with its harness's pid, the account it goes
/// on to, and whether its own is at its limit: the attended sessions the
/// Launcher started, of `sessions` (the daemon's registrations on the
/// reporting account), when `windows` on `login` meet rollover's early
/// switch (`Data::runs_low`, asked of `before`, the record as it was) or
/// its limit, and the ladder has a next account. A session with an account
/// already queued is not told again.
#[allow(clippy::too_many_arguments)]
pub fn due(daemon: &Path, before: &Data, after: &Data, login: &str, windows: &[UsageWindow], sessions: &[Registration], registry: &Registry, now_ms: i64) -> Vec<(u32, Next, bool)> {
    let limits = limits(windows);
    if before.runs_low(login, &limits, now_ms).is_none() {
        return Vec::new();
    }
    let limited = limits.iter().any(|l| l.status == LimitState::Limited);
    let mut out = Vec::new();
    for s in sessions {
        let Some(pid) = s.harness_pid else { continue };
        if s.attendance == Some(Attendance::Unattended) || queued(daemon, pid).is_some() {
            continue;
        }
        let Some(l) = launched(daemon, pid) else { continue };
        // No spend reading of an attended session reaches the daemon yet,
        // so the key rung stays closed to it (D3), Codex's always.
        let harness = if s.harness == PluginHarness::Claude { Harness::Claude } else { Harness::Codex };
        let keys = crate::ladder::keys_open(harness, true, None, None);
        let from = ModelRef { instance: l.account, model: l.model };
        if let Some(step) = registry.ladder.next(registry, after, &from, keys, now_ms) {
            out.push((pid, Next { session: s.session_id.clone(), account: step.to.instance, model: step.to.model, rung: step.rung, transcript: s.transcript_path.clone() }, limited));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::from_config_json;
    use serde_json::json;

    fn registry() -> Registry {
        let raw = json!({ "instances": {
            "claude:a": { "kind": "claude-code", "configDir": "/h/a" },
            "claude:b": { "kind": "claude-code", "configDir": "/h/b" },
        }});
        Registry::resolve(&from_config_json(&raw).unwrap(), &|k| if k == "HOME" { "/h".into() } else { String::new() })
    }

    fn session(id: &str, pid: u32, attendance: Option<Attendance>) -> Registration {
        Registration { session_id: id.into(), harness: PluginHarness::Claude, harness_pid: Some(pid), config_home: Some("/h/a".into()), cwd: None, repository: None, transcript_path: Some("/h/a/projects/-r/s.jsonl".into()), attendance }
    }

    fn window(used: f64) -> Vec<UsageWindow> {
        vec![UsageWindow { kind: "fiveHour".into(), used_percent: used, resets_at_ms: None }]
    }

    /// R-RL-5: a launched attended session whose account runs low is told
    /// once, with the ladder's next account; one the Launcher did not
    /// start, an unattended one, or an account with room is not.
    #[test]
    fn rl03_a_launched_attended_session_running_low_gets_the_next_account() {
        let daemon = std::env::temp_dir().join(format!("gruntas-rl03-due-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&daemon);
        std::fs::create_dir_all(&daemon).unwrap();
        // The default account shares claude:a's login: never a move.
        let data: Data = serde_json::from_value(json!({ "instances": { "claude": { "login": "a", "seenAtMs": 1 }, "claude:a": { "login": "a", "seenAtMs": 1 } } })).unwrap();
        let r = registry();
        let sessions = [session("s1", 11, None), session("s2", 12, None), session("s3", 13, Some(Attendance::Unattended))];
        let started = Launched { account: "claude:a".into(), model: "opus".into() };
        for pid in [11, 13] {
            mark(&daemon, pid, &started).unwrap();
        }
        assert!(due(&daemon, &data, &data, "a", &window(50.0), &sessions, &r, 1).is_empty(), "room left");
        let told = due(&daemon, &data, &data, "a", &window(100.0), &sessions, &r, 1);
        let next = Next { session: "s1".into(), account: "claude:b".into(), model: "opus".into(), rung: Rung::SameHarness, transcript: Some("/h/a/projects/-r/s.jsonl".into()) };
        assert_eq!(told, vec![(11, next.clone(), true)]);
        assert_eq!(notice(&next, true, 10.0), "This account has hit its usage limit. Next: claude:b on opus (another subscription) — type /exit to continue this session there.");
        let key = Next { account: "claude:key".into(), rung: Rung::ApiKey, ..next.clone() };
        assert_eq!(notice(&key, false, 10.0), "This account's usage is running out. Continuing on claude:key (opus): an API-key account, billed per use, up to $10.00 this session. Type /exit to continue this session there.");
        assert!(offer(&daemon, 11, &next).unwrap());
        assert!(!offer(&daemon, 11, &Next { account: "claude:other".into(), ..next.clone() }).unwrap(), "one queued is kept");
        assert!(due(&daemon, &data, &data, "a", &window(100.0), &sessions, &r, 1).is_empty(), "told once");
        follow(&daemon, 11, "s1b", None).unwrap();
        assert_eq!(queued(&daemon, 11).map(|n| (n.session, n.transcript)), Some(("s1b".into(), None)), "the session the harness runs now");
        follow(&daemon, 11, "s1", next.transcript.as_deref()).unwrap();
        assert_eq!(take(&daemon, 11), Some(next));
        mark(&daemon, 13, &started).unwrap();
        mark(&daemon, std::process::id(), &started).unwrap();
        // A pid no process has (above Linux's pid_max).
        mark(&daemon, 4_194_400, &started).unwrap();
        sweep(&daemon);
        assert!(launched(&daemon, std::process::id()).is_some() && launched(&daemon, 4_194_400).is_none(), "only the gone harness's records dropped");
        assert!(!launched_path(&daemon, 11).exists() && take(&daemon, 11).is_none(), "taken once, with its mark");
        let _ = std::fs::remove_dir_all(&daemon);
    }
}
