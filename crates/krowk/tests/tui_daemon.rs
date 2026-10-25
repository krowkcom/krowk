//! Bare `krowk` on a terminal with its sessions in the host daemon
//! (R-HOST-1, R-PROTO-1): a turn outlives the terminal that started it, and
//! a second TUI follows it. A test binary of its own, so these TUIs and
//! their daemons never run beside `tui.rs`'s timing-sensitive cases — an
//! idle TUI drawing nothing, redraw rates — on a small CI runner.

#![cfg(all(feature = "harness", unix))]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;
#[path = "common/pty.rs"]
mod pty;
#[path = "common/tmux.rs"]
mod tmux;

use tmux::Tmux;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-tui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        // The fake `claude` and `codex`, signed in to nothing, first on
        // PATH: a prompt with no model is routed, which asks each vendor
        // there is, and never the real ones the machine may have.
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for (dir, bin, to) in [("claude", "fake-claude", "claude"), ("codex", "fake-codex", "codex")] {
            let at = root.join("bin").join(to);
            // Linked, not copied: a copy is a file open for writing that a test
            // forking beside it can inherit, and running it then fails with
            // "Text file busy" (ETXTBSY) — read as a vendor that could not be checked.
            std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(dir).join(bin), &at).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The inline renderer, whose scrollback these read.
        std::fs::create_dir_all(root.join("home/.krowk")).unwrap();
        std::fs::write(root.join("home/.krowk/config.json"), r#"{"tui": {"screen": "inline"}}"#).unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn env(&self, url: &str) -> Vec<(String, String)> {
        let home = self.root.join("home");
        vec![
            ("PATH".into(), format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default())),
            ("HOME".into(), home.display().to_string()),
            ("TERM".into(), "xterm-256color".into()),
            ("KROWK_NO_UPDATE_CHECK".into(), "1".into()),
            // In this process, as before the daemon: the TUI's drawing is
            // what these hold, and a test that runs the daemon says so
            // (`Daemon`), with a runtime directory of its own.
            ("KROWK_TUI_HOST".into(), "local".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-test".into()),
            ("ANTHROPIC_BASE_URL".into(), url.into()),
        ]
    }

}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A sandbox whose TUI runs its sessions in the host daemon, in a runtime
/// directory of its own (short: a socket path has a hundred-odd bytes), and
/// stops that daemon when it goes — none outlives the test.
struct Daemon {
    sandbox: Sandbox,
    run: PathBuf,
}

impl Daemon {
    fn new(name: &str) -> Daemon {
        let run = PathBuf::from(format!("/tmp/krowk-rt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        Daemon { sandbox: Sandbox::new(name), run }
    }

    fn env(&self, url: &str) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self.sandbox.env(url).into_iter().filter(|(k, _)| k != "KROWK_TUI_HOST").collect();
        env.push(("XDG_RUNTIME_DIR".into(), self.run.display().to_string()));
        // Gone soon after the test, even if the stop below cannot reach it.
        env.push(("KROWK_HOST_IDLE".into(), "5".into()));
        env
    }

    fn command(&self, url: &str, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args).env_clear().envs(self.env(url)).current_dir(self.sandbox.root.join("repo"));
        c
    }

    /// The one session the daemon has logged.
    fn session(&self) -> String {
        let dir = self.sandbox.root.join("home/.krowk/sessions");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(id) = std::fs::read_dir(&dir).ok().into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.len() == 36) {
                return id;
            }
            assert!(Instant::now() < deadline, "no session was logged");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.command("http://127.0.0.1:9", &["host", "stop", "--force"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        let _ = std::fs::remove_dir_all(&self.run);
    }
}

/// R-HOST-1, R-PROTO-1: a session started in the TUI goes on when the
/// terminal closes — it runs in the host daemon, which the TUI reached over
/// its socket — and `krowk --resume` follows it again mid-reply: what was
/// typed while nobody watched, then the rest live, to its end.
#[test]
fn r_host_1_closing_the_terminal_leaves_the_turn_running_and_reopening_krowk_follows_it() {
    const ANSWER: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa";
    let m = mock::serve(|_, _| mock::Reply::paced(mock::text_stream(ANSWER), Duration::from_millis(120)));
    let d = Daemon::new("reopen");
    let mut a = pty::Pty::spawn(d.command(&m.url, &[]), 100, 30);
    assert!(a.wait_for(" help", Duration::from_secs(10)).is_some(), "{:?}", a.text());
    a.write(b"say the alphabet\r");
    assert!(a.wait_for("charlie", Duration::from_secs(10)).is_some(), "{:?}", a.text());
    // The terminal closes: a hangup, as a closed window sends.
    unsafe { libc::kill(a.child.id() as i32, libc::SIGHUP) };
    assert!(a.wait(Duration::from_secs(10)).is_some(), "the TUI goes with its terminal");
    let seen_by_a = a.text();
    assert!(!seen_by_a.contains("papa"), "it left mid-reply");
    let session = d.session();
    // Reopened: it says the session is still running, follows it, and
    // draws the rest of the reply as it arrives.
    let mut b = pty::Pty::spawn(d.command(&m.url, &["--resume", &session]), 100, 30);
    assert!(b.wait_for("following it", Duration::from_secs(10)).is_some(), "{:?}", b.text());
    assert!(b.wait_for("papa", Duration::from_secs(15)).is_some(), "the rest, live: {:?}", b.text());
    assert!(b.wait_for("tokens", Duration::from_secs(10)).is_some(), "and the turn's end: {:?}", b.text());
    let text = b.text();
    assert!(text.contains("alpha") && text.contains("mike"), "what it missed is there too: {text:?}");
    b.write(b"\x04\x04");
    assert!(b.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
}

/// The terminal itself going away mid-reply — every descriptor of its far
/// side closed, not a signal sent — ends the TUI within a few seconds, and
/// the turn runs on in the daemon.
#[test]
fn a_closed_terminal_ends_the_tui_mid_reply_and_the_turn_runs_on() {
    const ANSWER: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa";
    let m = mock::serve(|_, _| mock::Reply::paced(mock::text_stream(ANSWER), Duration::from_millis(120)));
    let d = Daemon::new("closed");
    let mut a = pty::Pty::spawn(d.command(&m.url, &[]), 100, 30);
    assert!(a.wait_for(" help", Duration::from_secs(10)).is_some(), "{:?}", a.text());
    a.write(b"say the alphabet\r");
    assert!(a.wait_for("charlie", Duration::from_secs(10)).is_some(), "{:?}", a.text());
    a.hang_up();
    assert!(a.wait(Duration::from_secs(5)).is_some(), "the TUI outlived its terminal by 5 s");
    let mut b = pty::Pty::spawn(d.command(&m.url, &["--resume", &d.session()]), 100, 30);
    assert!(b.wait_for("papa", Duration::from_secs(15)).is_some(), "the turn ran on: {:?}", b.text());
    b.write(b"\x04\x04");
    assert!(b.wait(Duration::from_secs(10)).is_some());
}

/// R-PROTO-1: two TUIs on one session show the same reply — the second
/// follows the first's turn in the daemon.
#[test]
fn r_proto_1_two_tuis_on_one_session_show_the_same_reply() {
    const ANSWER: &str = "one two three four five six seven eight nine ten eleven twelve";
    let m = mock::serve(|_, _| mock::Reply::paced(mock::text_stream(ANSWER), Duration::from_millis(100)));
    let d = Daemon::new("two");
    let mut a = pty::Pty::spawn(d.command(&m.url, &[]), 100, 30);
    assert!(a.wait_for(" help", Duration::from_secs(10)).is_some(), "{:?}", a.text());
    a.write(b"count\r");
    assert!(a.wait_for("two", Duration::from_secs(10)).is_some(), "{:?}", a.text());
    let mut b = pty::Pty::spawn(d.command(&m.url, &["--resume", &d.session()]), 100, 30);
    for t in [&a, &b] {
        assert!(t.wait_for("twelve", Duration::from_secs(15)).is_some(), "{:?}", t.text());
        assert!(t.wait_for("tokens", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    }
    let words = |t: &pty::Pty| ANSWER.split(' ').filter(|w| t.text().contains(w)).count();
    assert_eq!((words(&a), words(&b)), (12, 12), "both show every word");
    for t in [&mut a, &mut b] {
        t.write(b"\x04\x04");
        assert!(t.wait(Duration::from_secs(10)).is_some());
    }
}

/// R-SWITCH-4: a `/model` another terminal gives the session between the
/// first TUI's turns is kept by that TUI's next prompt — it runs on the
/// model the session moved to, and nothing switches it back: with both in
/// the host daemon, and with each running its sessions itself, the second
/// one's switch only in the log, as a `krowk sync host` logs a viewer's.
#[test]
fn r_switch_4_a_switch_another_client_made_between_turns_is_kept_by_the_next_prompt() {
    switch_kept_by_the_next_prompt(true);
    switch_kept_by_the_next_prompt(false);
}

fn switch_kept_by_the_next_prompt(daemon: bool) {
    let m = mock::serve(|_, _| mock::Reply::paced(mock::text_stream("on the first one"), Duration::from_millis(5)));
    let other = mock::serve(|_, _| mock::Reply::paced(mock::text_stream("on the other one"), Duration::from_millis(5)));
    let d = Daemon::new(if daemon { "kept" } else { "keptlocal" });
    let config = serde_json::json!({"tui": {"screen": "inline"}, "instances": {"anthropic:personal": {"kind": "anthropic-api", "apiKeyEnv": "ANTHROPIC_API_KEY", "baseUrl": other.url}}});
    std::fs::write(d.sandbox.root.join("home/.krowk/config.json"), config.to_string()).unwrap();
    let krowk = |args: &[&str]| match daemon {
        true => d.command(&m.url, args),
        false => {
            let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
            c.args(args).env_clear().envs(d.sandbox.env(&m.url)).current_dir(d.sandbox.root.join("repo"));
            c
        }
    };
    let secs = Duration::from_secs;
    let mut a = pty::Pty::spawn(krowk(&["--model", "anthropic/claude-sonnet-4-6"]), 100, 30);
    assert!(a.wait_for(" help", secs(10)).is_some(), "{:?}", a.text());
    a.write(b"hello\r");
    assert!(a.wait_for("on the first one", secs(10)).is_some(), "{:?}", a.text());
    assert!(a.wait_for("tokens", secs(10)).is_some(), "{:?}", a.text());
    let session = d.session();
    let mut b = pty::Pty::spawn(krowk(&["--resume", &session]), 100, 30);
    assert!(b.wait_for("on the first one", secs(10)).is_some(), "{:?}", b.text());
    b.write(b"/model anthropic:personal/claude-sonnet-4-6\r");
    assert!(b.wait_for("now on anthropic:personal/claude-sonnet-4-6", secs(10)).is_some(), "{:?}", b.text());
    let log = d.sandbox.root.join("home/.krowk/sessions").join(&session).join("events.jsonl");
    let of = |kind: &str| std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).filter(|e| e["type"] == kind).collect::<Vec<_>>();
    let ended = |n: usize, t: &pty::Pty| {
        let deadline = Instant::now() + secs(10);
        while of("turn.completed").len() < n {
            assert!(Instant::now() < deadline, "turn {n} never ended: {:?}", t.text());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // B's own prompt too, which a TUI on the daemon sees run.
    b.write(b"from b\r");
    ended(2, &b);
    a.write(b"and again\r");
    ended(3, &a);
    assert!(a.wait_for("on the other one", secs(10)).is_some(), "A's next prompt ran where B moved the session: {:?}", a.text());
    let (switched, turns) = (of("model.switched"), of("turn.started"));
    assert_eq!(switched.len(), 1, "B's switch alone, never one back: {switched:?}");
    assert_eq!(turns.iter().map(|t| t["model"]["instance"].as_str().unwrap_or_default()).collect::<Vec<_>>(), ["anthropic", "anthropic:personal", "anthropic:personal"], "{turns:?}");
    for t in [&mut a, &mut b] {
        t.write(b"\x04\x04");
        assert!(t.wait(secs(10)).is_some());
    }
}

/// R-LAG-9, R-TUI-1: the 10k-token answer of `tui.rs`, with the session in
/// the host daemon — where each event's append and each context record go
/// to the blocking pool and are awaited before the event is sent, so the
/// frames reach the TUI at the pace of the pool, not the engine's. Every
/// line is in scrollback once and in order, the prompt once, and no live
/// row with them.
#[test]
fn r_lag_9_a_10k_token_answer_from_the_daemon_lands_in_tmux_scrollback_exactly_once() {
    let body = mock::text_stream(&mock::numbered_lines(850));
    let m = mock::serve(move |_, _| mock::Reply::paced(body.clone(), Duration::from_micros(100)));
    let d = Daemon::new("scroll");
    let Some(tm) = Tmux::start("dscroll", 100, 30, &d.sandbox.root.join("repo"), &d.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["write it all out", "Enter"]);
    assert!(tm.wait_for("tokens", Duration::from_secs(60)).is_some(), "the answer never finished:\n{}", tm.screen());
    let history = tm.history();
    let got: Vec<&str> = history.lines().map(str::trim).filter(|l| l.starts_with("line ")).collect();
    let want: Vec<String> = mock::numbered_lines(850).lines().map(String::from).collect();
    assert_eq!(got.len(), want.len(), "every line once, none twice");
    assert!(got.iter().zip(&want).all(|(g, w)| g == w), "in order, byte for byte");
    assert_eq!(history.matches("\nwrite it all out\n").count(), 1, "{history}");
    assert_eq!(history.matches("esc to interrupt").count(), 0, "a live row leaked into scrollback");
}

/// R-SUB-7, R-SUB-11 through the daemon: a second TUI resuming a session
/// whose Claude Code agent runs in the background, between turns, shows
/// that agent running — not done at its call's result — and follows it:
/// its end, which comes with no turn running, reaches that TUI too, then
/// the turn Claude Code begins to answer it, which one of the two runs and
/// the other is refused without a word (`session_busy`). That turn over,
/// it stops following: a later prompt of the first is not mirrored.
#[test]
fn r_sub_7_a_tui_resuming_a_session_follows_its_background_agent_between_turns() {
    let d = Daemon::new("bgfollow");
    let (a, parent) = background_agent_started(&d);
    let mut a = a;
    let secs = Duration::from_secs;
    let mut b = pty::Pty::spawn(claude_cmd(&d, &["--resume", &parent]), 160, 30);
    assert!(b.wait_for("[1 subagent]", secs(10)).is_some(), "running, not done at its result: {:?}", b.text());
    b.write(b"\x07");
    assert!(b.wait_for("survey the repo · general-purpose · in the background", secs(10)).is_some(), "{:?}", b.text());
    assert!(b.wait_for("done in", secs(15)).is_some(), "its end, between turns: {:?}", b.text());
    b.write(b"\x1b");
    for t in [&a, &b] {
        assert!(t.wait_for("The agent is done: the repo is small.", secs(15)).is_some(), "Claude Code's turn: {:?}", t.text());
    }
    std::thread::sleep(Duration::from_millis(1000));
    a.write(b"and one more thing\r");
    assert!(a.wait_for("and one more thing", secs(10)).is_some(), "{:?}", a.text());
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!b.text().contains("session_busy") && !a.text().contains("session_busy"), "{:?}", b.text());
    assert!(!b.text().contains("and one more thing"), "no longer following once that turn ran: {:?}", b.text());
    for t in [&mut a, &mut b] {
        t.write(b"\x04\x04");
        assert!(t.wait(secs(10)).is_some());
    }
}

/// R-SUB-7: the TUI that ran the session gone, the second, following it
/// for its background agent, runs the turn Claude Code begins to answer
/// it: it does not let go of the session first, which would let its
/// backend go with that turn.
#[test]
fn r_sub_7_a_tui_following_a_background_agent_alone_runs_the_turn_that_answers_it() {
    let d = Daemon::new("bgalone");
    let (mut a, parent) = background_agent_started(&d);
    let secs = Duration::from_secs;
    let mut b = pty::Pty::spawn(claude_cmd(&d, &["--resume", &parent]), 160, 30);
    assert!(b.wait_for("[1 subagent]", secs(10)).is_some(), "{:?}", b.text());
    a.write(b"\x04\x04");
    assert!(a.wait(secs(10)).is_some());
    assert!(b.wait_for("The agent is done: the repo is small.", secs(20)).is_some(), "run here: {:?}", b.text());
    b.write(b"\x04\x04");
    assert!(b.wait(secs(10)).is_some());
}

/// R-SUB-7: a background agent whose log never got an end — its process
/// killed, the machine restarted — is not shown running for ever by a TUI
/// resuming the session through a daemon that runs nothing of it: it ends
/// interrupted, as a local session's does.
#[test]
fn r_sub_7_a_resumed_background_agent_the_daemon_no_longer_runs_ends_interrupted() {
    let d = Daemon::new("bgdead");
    let (mut a, parent) = background_agent_started(&d);
    let secs = Duration::from_secs;
    assert!(a.wait_for("The agent is done: the repo is small.", secs(20)).is_some(), "{:?}", a.text());
    a.write(b"\x04\x04");
    assert!(a.wait(secs(10)).is_some());
    let _ = d.command("http://127.0.0.1:9", &["host", "stop", "--force"]).output();
    // As a process killed mid-agent leaves it: the child's end never written.
    let sessions = d.sandbox.root.join("home/.krowk/sessions");
    let kid = std::fs::read_dir(&sessions).unwrap().flatten().map(|e| e.path()).find(|p| std::fs::read_to_string(p.join("events.jsonl")).unwrap_or_default().lines().next().is_some_and(|l| l.contains(&format!("\"parentSessionId\":\"{parent}\""))));
    let log = kid.expect("the agent's child").join("events.jsonl");
    let kept: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().filter(|l| !l.contains("\"turn.completed\"")).map(|l| format!("{l}\n")).collect();
    std::fs::write(&log, kept.concat()).unwrap();
    let mut b = pty::Pty::spawn(claude_cmd(&d, &["--resume", &parent]), 160, 30);
    assert!(b.wait_for(" help", secs(10)).is_some(), "{:?}", b.text());
    b.write(b"\x07");
    assert!(b.wait_for("survey the repo · general-purpose · interrupted", secs(10)).is_some(), "{:?}", b.text());
    assert!(!b.text().contains("[1 subagent]"), "{:?}", b.text());
    b.write(b"\x1b\x04\x04");
    assert!(b.wait(secs(10)).is_some());
}

/// The fake `claude` playing its background-agent scenario, the agent's
/// lines a few seconds after its turn — time for a second TUI to resume
/// the session first — and its own turn a second after the agent's end,
/// as `args` asks.
fn claude_cmd(d: &Daemon, args: &[&str]) -> Command {
    let root = &d.sandbox.root;
    let scenario = root.join("background.jsonl");
    if !scenario.exists() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude");
        let s = std::fs::read_to_string(fixtures.join("background_agent.jsonl")).unwrap().replacen("@by-itself\n", "@by-itself\n@sleep 4\n", 1);
        // And a moment before the turn Claude Code begins, as its model call
        // takes: what a client that heard the agent end does meanwhile.
        let at = s.rfind("@begin\n").unwrap();
        let s = format!("{}@sleep 1\n{}", &s[..at], &s[at..]);
        std::fs::write(&scenario, s).unwrap();
        let mut add = d.command("http://127.0.0.1:9", &["providers", "add", "claude", "--json"]);
        assert!(add.output().unwrap().status.success());
        std::fs::write(root.join("home/.krowk/trusted.json"), serde_json::json!({"directories": [root.join("repo")]}).to_string()).unwrap();
    }
    let mut c = d.command("http://127.0.0.1:9", args);
    c.env("FAKE_CLAUDE_SCENARIO", &scenario);
    c
}

/// A TUI on the daemon that has started the background agent, and the
/// session's id (not its child's).
fn background_agent_started(d: &Daemon) -> (pty::Pty, String) {
    let secs = Duration::from_secs;
    let mut a = pty::Pty::spawn(claude_cmd(d, &["--model", "claude/sonnet"]), 160, 30);
    assert!(a.wait_for(" help", secs(10)).is_some(), "{:?}", a.text());
    a.write(b"survey the repo in the background\r");
    assert!(a.wait_for("launched", secs(10)).is_some(), "{:?}", a.text());
    let sessions = d.sandbox.root.join("home/.krowk/sessions");
    let parent = std::fs::read_dir(&sessions).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.len() == 36).find(|n| {
        let first = std::fs::read_to_string(sessions.join(n).join("events.jsonl")).unwrap_or_default();
        first.lines().next().is_some_and(|l| !l.contains("parentSessionId\":\""))
    });
    (a, parent.expect("the session"))
}
