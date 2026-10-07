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
