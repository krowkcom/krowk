//! Bare `krowk` on a terminal, the built binary, against the stand-in
//! Anthropic API. Two kinds of terminal:
//!
//! - a bare pseudo-terminal (`common/pty.rs`), where every byte the TUI
//!   writes is seen and each frame's synchronized-update bracket is
//!   timestamped — for what it sends and how often;
//! - tmux, a real terminal emulator with a scrollback, for what a person
//!   ends up looking at: `capture-pane` of the whole history.
//!
//! The tmux cases need tmux on PATH. CI installs it on Linux; a machine
//! without it skips them with a line saying so, except CI on Linux, where a
//! skip would hide a broken acceptance check and fails instead.

#![cfg(all(feature = "harness", unix))]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;
#[path = "common/pty.rs"]
mod pty;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn env(&self, url: &str) -> Vec<(String, String)> {
        let home = self.root.join("home");
        vec![
            ("PATH".into(), format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default())),
            ("HOME".into(), home.display().to_string()),
            ("TERM".into(), "xterm-256color".into()),
            ("KROWK_NO_UPDATE_CHECK".into(), "1".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-test".into()),
            ("ANTHROPIC_BASE_URL".into(), url.into()),
        ]
    }

    /// config.json laying out at the terminal's whole width, for a test
    /// that reads rows wider than `prose`'s 80 columns.
    fn full_width(&self) {
        std::fs::create_dir_all(self.root.join("home/.krowk")).unwrap();
        std::fs::write(self.root.join("home/.krowk/config.json"), r#"{"tui": {"contentWidth": "full-width"}}"#).unwrap();
    }

    fn command(&self, url: &str, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args).env_clear().envs(self.env(url)).current_dir(self.root.join("repo"));
        c
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---- a bare pseudo-terminal -------------------------------------------------

#[test]
fn r_pkg_1_r_tui_3_bare_krowk_on_a_terminal_opens_the_prompt_with_only_portable_sequences() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("opens");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 80, 24);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "no prompt: {:?}", t.text());
    t.write(b"read README.md and summarise it in one line\r");
    // Blank cells are skipped, not written, so only a word is sure to
    // arrive whole.
    assert!(t.wait_for("anywhere.", Duration::from_secs(10)).is_some(), "no answer: {:?}", t.text());
    assert!(t.wait_for("tokens", Duration::from_secs(5)).is_some());
    // Ctrl-D on an empty prompt quits, and says how to come back.
    t.write(b"\x04");
    let st = t.wait(Duration::from_secs(10)).expect("krowk exits on Ctrl-D");
    assert!(st.success(), "{st}");
    let out = t.text();
    assert!(out.contains("krowk --resume "), "{out:?}");
    // R-TUI-3: nothing a phone terminal, tmux or an SSH hop would not
    // pass through — no alternate screen, no mouse capture, no full-screen
    // clear.
    for bad in ["\x1b[?1049h", "\x1b[?47h", "\x1b[?1000h", "\x1b[?1002h", "\x1b[?1003h", "\x1b[?1006h", "\x1b[2J", "\x1b[3J"] {
        assert!(!out.contains(bad), "sent {bad:?}");
    }
    // The one keyboard protocol level, for shift-enter: pushed once, and
    // popped after it.
    assert_eq!(out.matches("\x1b[>1u").count(), 1, "{out:?}");
    assert_eq!(out.matches("\x1b[<u").count(), 1, "{out:?}");
    assert!(out.rfind("\x1b[<u") > out.find("\x1b[>1u"), "the keyboard protocol was never popped: {out:?}");
    // R-TUI-1: every frame is bracketed, and brackets pair up.
    let (begins, ends) = (t.frames().len(), t.frame_ends().len());
    assert!(begins > 2 && begins == ends, "{begins} frames begun, {ends} ended");
    // The log is the session, and krowk.db lists it.
    let seen = m.seen.lock().unwrap();
    assert!(seen.iter().any(|s| s.body["messages"].as_array().is_some_and(|m| m.len() == 3)), "the tool loop ran");
}

#[test]
fn shift_enter_starts_a_new_line_and_enter_sends_both() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("shiftenter");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 80, 24);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "no prompt: {:?}", t.text());
    // What a terminal that took the keyboard protocol push sends for
    // shift-enter (CSI 13;2u), with plain enter still a CR.
    t.write(b"read README.md\x1b[13;2uand summarise it\r");
    assert!(t.wait_for("anywhere.", Duration::from_secs(10)).is_some(), "no answer: {:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let seen = m.seen.lock().unwrap();
    assert!(seen.iter().any(|s| s.body["messages"][0].to_string().contains("read README.md\\nand summarise it")), "no prompt of two lines was sent");
}

#[test]
fn ctrl_z_pops_the_keyboard_protocol_for_the_shell_and_pushes_it_again_on_the_way_back() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("ctrlzkeys");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 80, 24);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "no prompt: {:?}", t.text());
    // Ctrl-Z. krowk leads its own session here, an orphaned process group,
    // so the kernel drops the SIGTSTP it raises and it takes the terminal
    // back at once: the whole give-up and take-back, with no shell.
    t.write(b"\x1a");
    let back = || t.text().matches("\x1b[>1u").count() == 2;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !back() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let out = t.text();
    let keys: std::collections::BTreeMap<usize, &str> = out.match_indices("\x1b[>1u").chain(out.match_indices("\x1b[<u")).collect();
    let keys: Vec<&str> = keys.into_values().collect();
    assert_eq!(keys, ["\x1b[>1u", "\x1b[<u", "\x1b[>1u", "\x1b[<u"], "pushed, popped for the shell, pushed on the way back, popped on quit");
}

#[test]
fn r_perf_4_a_500_token_a_second_stream_redraws_at_most_60_times_a_second() {
    // ~1,500 tokens, one delta every 2 ms: three seconds of streaming.
    let body = mock::text_stream(&mock::numbered_lines(125));
    let m = mock::serve(move |_, _| mock::Reply::paced(body.clone(), Duration::from_millis(2)));
    let b = Sandbox::new("fps");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 100, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some());
    let before = t.frames().len();
    t.write(b"stream\r");
    // The turn's end is its token line; a loaded macOS runner has taken
    // longer than 20 s to get there, so the bound is generous and the wait
    // is on the turn finishing, not on the last line alone. The last line
    // must still be there, whole.
    assert!(t.wait_for("tokens", Duration::from_secs(90)).is_some(), "the stream never finished: {:?}", t.text().len());
    assert!(t.text().contains("00125:"), "the turn ended without its last line");
    let frames: Vec<Instant> = t.frames()[before..].to_vec();
    let peak = pty::peak_fps(&frames);
    assert!(frames.len() >= 30, "it redrew while streaming: {} frames", frames.len());
    assert!(peak <= 60, "{peak} frames inside one second");
}

#[test]
fn a_link_in_an_answer_is_shown_by_its_text_and_an_arrow_and_opens_its_url() {
    let body = mock::text_stream("Read [the docs](https://krowk.com/docs) or https://krowk.com/faq.\n");
    let m = mock::serve(move |_, _| mock::Reply::sse(&body));
    let b = Sandbox::new("links");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 100, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some());
    t.write(b"links\r");
    assert!(t.wait_for("tokens", Duration::from_secs(30)).is_some(), "{:?}", t.text());
    let out = t.text();
    let link = |url: &str, sgr: &str, text: &str| format!("\x1b]8;;{url}\x1b\\\x1b[{sgr}m{text}\x1b[0m\x1b]8;;\x1b\\");
    for want in [link("https://krowk.com/docs", "4;36", "the docs"), link("https://krowk.com/docs", "36", "\u{a0}↗"), link("https://krowk.com/faq", "4;36", "https://krowk.com/faq")] {
        assert!(out.contains(&want), "{want:?} in {out:?}");
    }
    assert!(!out.contains("](https"), "the markdown is not shown: {out:?}");
}

#[test]
fn r_perf_2_nothing_is_drawn_while_idle() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("idle");
    let t = pty::Pty::spawn(b.command(&m.url, &[]), 80, 24);
    // The first frame, status line and all, is the last one there is
    // reason to draw: the start-up probe answering online changes nothing
    // on screen, so it draws nothing.
    assert!(t.wait_for("? help", Duration::from_secs(10)).is_some(), "no status line: {:?}", t.text());
    std::thread::sleep(Duration::from_millis(300));
    let before = t.output().len();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(t.output().len(), before, "an idle TUI wrote {:?}", String::from_utf8_lossy(&t.output()[before..]));
}

/// R-INST-7 and R-SWITCH-4 in the TUI itself: an instance at its rate
/// limit offers the next one as one question, `y` continues the prompt
/// there, `/model` opens the picker, and a switch to an instance with no key
/// is refused with its fix while the session stays where it was.
#[test]
fn r_inst_7_the_tui_offers_the_next_instance_and_y_continues_there() {
    let limited = mock::serve(|_, _| mock::Reply { headers: vec![("retry-after".into(), "0".into())], ..mock::Reply::json(429, &serde_json::json!({"type": "error", "error": {"type": "rate_limit_error", "message": "rate limited"}})) });
    let ok = mock::serve(mock::readme_script);
    let b = Sandbox::new("offer");
    let config = b.root.join("home/.krowk");
    std::fs::create_dir_all(&config).unwrap();
    let instances = serde_json::json!({"instances": {
        "anthropic:personal": {"kind": "anthropic-api", "apiKeyEnv": "ANTHROPIC_API_KEY", "baseUrl": ok.url},
        "anthropic:nokey": {"kind": "anthropic-api", "apiKeyEnv": "NO_SUCH_KEY", "baseUrl": ok.url},
    }});
    std::fs::write(config.join("config.json"), instances.to_string()).unwrap();
    let mut t = pty::Pty::spawn(b.command(&limited.url, &["--model", "anthropic/claude-sonnet-4-6"]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"read README.md and summarise it\r");
    assert!(t.wait_for("[y/N]", Duration::from_secs(20)).is_some(), "no offer: {:?}", t.text());
    assert!(t.text().contains("anthropic:personal?"), "{:?}", t.text());
    t.write(b"y");
    assert!(t.wait_for("anywhere.", Duration::from_secs(20)).is_some(), "the prompt did not continue there: {:?}", t.text());
    assert!(ok.seen.lock().unwrap().iter().any(|s| s.body.to_string().contains("read README.md and summarise it")), "sent to the instance it moved to");
    // The picker, and a switch refused with its fix.
    t.write(b"/model\r");
    assert!(t.wait_for("switch model", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    assert!(t.wait_for("codex/gpt-5.5", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    assert!(!t.text().contains("anthropic:nokey/"), "an instance with no key is not listed: {:?}", t.text());
    // Esc on its own, not the start of an Alt-/ chord.
    t.write(b"\x1b");
    std::thread::sleep(Duration::from_millis(300));
    t.write(b"/model anthropic:nokey/claude-sonnet-4-6\r");
    assert!(t.wait_for("NO_SUCH_KEY", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    assert!(t.wait_for("stays", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
}

/// `/mode` picks the permission mode the session's next turn runs in, and
/// `/permission-mode <name>` names one; a name that is not a mode is refused.
#[test]
fn the_mode_picker_sets_the_mode_the_next_turn_runs_in() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("mode");
    b.full_width();
    let mut t = pty::Pty::spawn(b.command(&m.url, &["--model", "anthropic/claude-sonnet-4-6"]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"/permission-mode nope\r");
    assert!(t.wait_for("nope is not a permission mode", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    // The bare alias opens the picker, as `/mode` does: its header, drawn
    // after each is sent.
    let picker = |t: &pty::Pty, from: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if String::from_utf8_lossy(&t.output()[from..]).contains("or /mode <name>") {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    };
    let from = t.output().len();
    t.write(b"/permission-mode\r");
    assert!(picker(&t, from), "{:?}", t.text());
    t.write(b"\x1b");
    std::thread::sleep(Duration::from_millis(300));
    let from = t.output().len();
    t.write(b"/mode\r");
    assert!(picker(&t, from), "{:?}", t.text());
    // From default, two rows down is plan.
    t.write(b"\x1b[B");
    t.write(b"\x1b[B");
    t.write(b"\r");
    assert!(t.wait_for("permission mode plan", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"read README.md and summarise it\r");
    assert!(t.wait_for("anywhere.", Duration::from_secs(20)).is_some(), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let logs = walk(&b.root.join("home"));
    assert!(logs.iter().any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains("\"turn.started\"") && s.contains("\"permissionMode\":\"plan\""))), "no turn ran in plan: {logs:?}");
}

/// `/sessions` (here by its alias, `/resume`) lists the sessions started
/// here, and enter continues one in
/// place of the new session: its conversation is replayed, and the next
/// prompt goes to it, the earlier turns and all.
#[test]
fn resume_continues_an_earlier_session_from_the_slash_menu() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("resume");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"read README.md and summarise it\r");
    assert!(t.wait_for("anywhere.", Duration::from_secs(20)).is_some(), "{:?}", t.text());
    assert!(t.wait_for("tokens", Duration::from_secs(5)).is_some());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let out = t.text();
    let at = out.find("krowk --resume ").expect("the resume line") + "krowk --resume ".len();
    let id = out[at..at + 36].to_string();

    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    // An id that is not one is refused, never sent to the model.
    t.write(b"/sessions nope\r");
    assert!(t.wait_for("\"nope\" is not a krowk session id", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    // A session of its own first, left for the earlier one: krowk.db still
    // lists it once krowk is gone.
    let from = t.output().len();
    t.write(b"summarise the README again\r");
    assert!(wait_after(&t, from, "tokens", Duration::from_secs(20)), "{:?}", t.text());
    t.write(b"/resume\r");
    assert!(t.wait_for("enter continues it", Duration::from_secs(10)).is_some(), "no picker: {:?}", t.text());
    assert!(t.wait_for("read README.md and summarise it", Duration::from_secs(5)).is_some(), "the session is not listed: {:?}", t.text());
    t.write(b"\r");
    assert!(t.wait_for(&format!("continuing session {id}"), Duration::from_secs(10)).is_some(), "{:?}", t.text());
    let from = t.output().len();
    t.write(b"and what else is in it?\r");
    assert!(wait_after(&t, from, "tokens", Duration::from_secs(20)), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    assert!(t.text().contains(&format!("krowk --resume {id}")), "{:?}", t.text());
    // One request carried both prompts: the turn continued the session.
    let seen = m.seen.lock().unwrap();
    assert!(!seen.iter().any(|s| s.body["messages"].to_string().contains("/sessions nope")));
    assert!(seen.iter().any(|s| { let b = s.body["messages"].to_string(); b.contains("read README.md and summarise it") && b.contains("and what else is in it?") }), "the earlier turn was not sent");
    drop(seen);
    let log = |session: &str| std::fs::read_to_string(b.root.join("home/.krowk/sessions").join(session).join("events.jsonl")).unwrap();
    assert!(log(&id).contains("and what else is in it?"), "the prompt went to the session resumed");
    let sessions: Vec<PathBuf> = std::fs::read_dir(b.root.join("home/.krowk/sessions")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    assert_eq!(sessions.len(), 2, "the earlier one and the one left for it: {sessions:?}");
    assert_eq!(krowk_sessions(&b, &m.url), 2, "krowk.db lists the session left too");
}

/// `/new` (here by its alias, `/clear`) leaves the session shown for a
/// fresh one: the next prompt goes without the earlier turns, and both
/// sessions are kept.
#[test]
fn new_starts_a_fresh_session_and_keeps_the_one_left() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("new");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    // Words after it are refused, never sent as a prompt.
    t.write(b"/clear kumquat\r");
    assert!(t.wait_for("/new takes nothing after it", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"read README.md and summarise it\r");
    assert!(t.wait_for("tokens", Duration::from_secs(20)).is_some(), "{:?}", t.text());
    let from = t.output().len();
    t.write(b"/clear\r");
    assert!(wait_after(&t, from, "Directory:", Duration::from_secs(10)), "the header again: {:?}", t.text());
    let from = t.output().len();
    t.write(b"summarise the README again\r");
    assert!(wait_after(&t, from, "tokens", Duration::from_secs(20)), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let seen = m.seen.lock().unwrap();
    assert!(!seen.iter().any(|s| s.body["messages"].to_string().contains("/clear") || s.body["messages"].to_string().contains("kumquat")), "the command went to the model");
    assert!(!seen.iter().any(|s| { let b = s.body["messages"].to_string(); b.contains("read README.md and summarise it") && b.contains("summarise the README again") }), "the earlier turn was sent to the new session");
    drop(seen);
    assert_eq!(krowk_sessions(&b, &m.url), 2, "krowk.db lists the session left too");
}

/// After `krowk --resume`, `/new` keeps to the directory the session was
/// resumed for: one begun elsewhere is refused, its trust and settings
/// being that directory's; one begun here is left for a fresh session.
#[test]
fn new_after_a_resume_at_start_keeps_to_the_directory() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("new-resume");
    let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"read README.md and summarise it\r");
    assert!(t.wait_for("tokens", Duration::from_secs(20)).is_some(), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let out = t.text();
    let at = out.find("krowk --resume ").expect("the resume line") + "krowk --resume ".len();
    let id = out[at..at + 36].to_string();

    let elsewhere = b.root.join("elsewhere");
    std::fs::create_dir_all(elsewhere.join(".git")).unwrap();
    let mut c = b.command(&m.url, &["--resume", &id]);
    c.current_dir(&elsewhere);
    let mut t = pty::Pty::spawn(c, 120, 30);
    assert!(t.wait_for(&format!("resumed session {id}"), Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"/new\r");
    assert!(t.wait_for("for a new one", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));

    let mut t = pty::Pty::spawn(b.command(&m.url, &["--resume", &id]), 120, 30);
    assert!(t.wait_for(&format!("resumed session {id}"), Duration::from_secs(10)).is_some(), "{:?}", t.text());
    let from = t.output().len();
    t.write(b"/new\r");
    assert!(wait_after(&t, from, "Model:", Duration::from_secs(10)), "the header names the model: {:?}", t.text());
    let from = t.output().len();
    t.write(b"summarise the README again\r");
    assert!(wait_after(&t, from, "tokens", Duration::from_secs(20)), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let seen = m.seen.lock().unwrap();
    let last = seen.iter().rev().find(|s| s.body["messages"].to_string().contains("summarise the README again")).expect("the prompt was sent");
    assert!(!last.body["messages"].to_string().contains("read README.md and summarise it"), "the resumed turns went to the new session");
    assert_eq!(last.body["model"], seen[0].body["model"], "on the model the resumed session was on");
}

/// `/config` (or `/settings`) cycles the default permission mode and saves
/// it to config.json, and the next session starts in it; ↓ chooses the
/// content width, saved the same way.
#[test]
fn settings_saves_the_default_permission_mode_the_next_session_starts_in() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("settings");
    let mut t = pty::Pty::spawn(b.command(&m.url, &["--model", "anthropic/claude-sonnet-4-6"]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"/config\r\x1b[C");
    assert!(t.wait_for("Default permission mode", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    // A key typed ahead of the overlay changes nothing; once it is up,
    // what is typed never reaches the prompt and only the arrows change
    // the setting. The keys are taken in order, so once config.json has
    // the change, `zq` has been taken too.
    let path = b.root.join("home/.krowk/config.json");
    let saved = || std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| v["permissions"]["defaultMode"].as_str().map(String::from));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(saved(), None, "the → typed ahead changed the setting");
    // → held down chooses unhinged, and again unhinged.
    t.write(b"zq\x1b[C\x1b[C");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while saved().as_deref() != Some("unhinged") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(saved().as_deref(), Some("unhinged"), "{:?}", t.text());
    assert!(!t.text().contains("zq"), "typed into the prompt: {:?}", t.text());
    // ↓ chooses the content width, → makes it prose-wide.
    let width = || std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| v["tui"]["contentWidth"].as_str().map(String::from));
    t.write(b"\x1b[B\x1b[C");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while width().as_deref() != Some("prose-wide") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(width().as_deref(), Some("prose-wide"), "{:?}", t.text());
    assert_eq!(saved().as_deref(), Some("unhinged"), "the mode stays as chosen");
    t.write(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(b.root.join("home/.krowk/config.json")).unwrap()).unwrap();
    assert_eq!(config["permissions"]["defaultMode"], "unhinged", "{config}");
    let mut t = pty::Pty::spawn(b.command(&m.url, &["--model", "anthropic/claude-sonnet-4-6"]), 120, 30);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"read README.md and summarise it\r");
    assert!(t.wait_for("anywhere.", Duration::from_secs(20)).is_some(), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let logs = walk(&b.root.join("home"));
    assert!(logs.iter().any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains("\"turn.started\"") && s.contains("\"permissionMode\":\"unhinged\""))), "no turn ran unhinged: {logs:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.flatten().flat_map(|e| if e.path().is_dir() { walk(&e.path()) } else { vec![e.path()] }).collect()
}

/// A provider that takes the request and never answers: a turn that waits.
fn silent_provider() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for c in l.incoming().flatten() {
            held.push(c);
        }
    });
    url
}

fn krowk_sessions(b: &Sandbox, url: &str) -> usize {
    let out = b.command(url, &["sessions", "--json"]).stdin(std::process::Stdio::null()).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v["data"]["sessions"].as_array().map_or(0, Vec::len)
}

#[test]
fn sigterm_and_sighup_restore_the_terminal_and_record_the_session() {
    for (name, sig) in [("term", libc::SIGTERM), ("hup", libc::SIGHUP)] {
        let m = mock::serve(mock::readme_script);
        let b = Sandbox::new(&format!("sig{name}"));
        let mut t = pty::Pty::spawn(b.command(&m.url, &[]), 80, 24);
        assert!(t.wait_for("anything", Duration::from_secs(10)).is_some());
        t.write(b"read README.md and summarise it in one line\r");
        assert!(t.wait_for("tokens", Duration::from_secs(10)).is_some(), "{:?}", t.text());
        // SAFETY: a signal to our own child.
        unsafe {
            libc::kill(t.child.id() as i32, sig);
        }
        let st = t.wait(Duration::from_secs(10)).expect("krowk exits on the signal");
        assert!(st.success(), "{name}: {st}");
        let out = t.text();
        assert!(out.contains("krowk --resume "), "{name}: the resume line: {out:?}");
        assert!(out.ends_with("\x1b[?2004l\x1b[?25h"), "{name}: bracketed paste off and the cursor back, last: {out:?}");
        assert!(out.matches("\x1b[>1u").count() == 1 && out.matches("\x1b[<u").count() == 1, "{name}: the keyboard protocol pushed and popped once: {out:?}");
        assert_eq!(krowk_sessions(&b, &m.url), 1, "{name}: the session is in krowk.db");
    }
}

#[test]
fn a_second_ctrl_c_leaves_at_once_but_still_records_the_session_and_exits_130() {
    let url = silent_provider();
    let b = Sandbox::new("ctrlc2");
    let mut t = pty::Pty::spawn(b.command(&url, &[]), 80, 24);
    // With no model asked, the TUI routes one once it is up: the key's
    // instance, named in the status line once chosen.
    assert!(t.wait_for("Claude Opus 5.5 (anthropic) |", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"wait forever\r");
    assert!(t.wait_for("esc to interrupt", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    // The prompt drawn back as the log's `▎` band: the turn has started,
    // so the TUI knows the session it is to record. The working line
    // alone comes before that, and two Ctrl-Cs sent then, on a loaded
    // machine, left before any session existed.
    assert!(t.wait_for("▎ ", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"\x03\x03");
    let st = t.wait(Duration::from_secs(10)).expect("krowk exits on the second Ctrl-C");
    assert_eq!(st.code(), Some(130), "{st}");
    let out = t.text();
    assert!(out.contains("krowk --resume ") && out.ends_with("\x1b[?2004l\x1b[?25h"), "{out:?}");
    assert!(out.matches("\x1b[>1u").count() == 1 && out.matches("\x1b[<u").count() == 1, "the keyboard protocol pushed and popped once: {out:?}");
    assert_eq!(krowk_sessions(&b, &url), 1, "the session is in krowk.db");
}

#[test]
fn steering_an_interrupted_turn_never_read_goes_back_into_the_prompt_not_sent() {
    let url = silent_provider();
    let b = Sandbox::new("steerback");
    let mut t = pty::Pty::spawn(b.command(&url, &[]), 80, 24);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some());
    t.write(b"wait forever\r");
    assert!(t.wait_for("esc to interrupt", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"also this\r");
    assert!(t.wait_for("steer queued", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    t.write(b"\x1b");
    assert!(t.wait_for("back in the prompt", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    std::thread::sleep(Duration::from_millis(300));
    let out = t.text();
    let tail = &out[out.rfind("back in the prompt").unwrap()..];
    // Blank cells are skipped, not written: the words arrive apart.
    assert!(tail.contains("also") && tail.contains("this"), "the steer is in the prompt again: {tail:?}");
    assert!(!tail.contains("esc to interrupt"), "and no new turn was started with it: {tail:?}");
}

// ---- tmux ---------------------------------------------------------------------

struct Tmux {
    socket: String,
}

impl Tmux {
    /// None when tmux is not installed and this is not CI.
    fn start(name: &str, cols: u16, rows: u16, cwd: &Path, env: &[(String, String)], args: &[&str]) -> Option<Tmux> {
        Tmux::start_after(name, cols, rows, cwd, env, args, "")
    }

    /// As `start`, with `before` run in the shell first — output a person's
    /// terminal already holds when they type `krowk`.
    fn start_after(name: &str, cols: u16, rows: u16, cwd: &Path, env: &[(String, String)], args: &[&str], before: &str) -> Option<Tmux> {
        if Command::new("tmux").arg("-V").output().is_err() {
            assert!(!cfg!(target_os = "linux") || std::env::var_os("CI").is_none(), "tmux is not installed, and CI on Linux must run this check");
            eprintln!("skipped: tmux is not installed");
            return None;
        }
        let socket = format!("krowk-tui-{name}-{}", std::process::id());
        let conf = std::env::temp_dir().join(format!("{socket}.conf"));
        std::fs::write(&conf, "set -g history-limit 100000\nset -g status off\n").unwrap();
        let mut line = format!("cd '{}' && {before} exec env -i", cwd.display());
        for (k, v) in env {
            line += &format!(" {k}='{v}'");
        }
        line += &format!(" '{}'", env!("CARGO_BIN_EXE_krowk"));
        for a in args {
            line += &format!(" '{a}'");
        }
        let st = Command::new("tmux")
            .args(["-L", &socket, "-f"])
            .arg(&conf)
            .args(["new-session", "-d", "-s", "t", "-x", &cols.to_string(), "-y", &rows.to_string(), &line])
            .status()
            .unwrap();
        assert!(st.success(), "tmux new-session: {st}");
        Some(Tmux { socket })
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux").args(["-L", &self.socket]).args(args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn keys(&self, keys: &[&str]) {
        let mut a = vec!["send-keys", "-t", "t"];
        a.extend_from_slice(keys);
        self.tmux(&a);
    }

    fn screen(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "t"])
    }

    fn history(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "t", "-S", "-", "-E", "-"])
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if self.screen().contains(needle) {
                return Some(t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        None
    }

    /// As `wait_for`, over the whole history: a fast stream scrolls a line
    /// past the screen before a poll of the screen alone can see it.
    fn wait_in_history(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if self.history().contains(needle) {
                return Some(t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        None
    }

    /// The whole history once the TUI has stopped drawing: the same across
    /// two polls, with `ready` true of the screen.
    fn wait_still(&self, ready: impl Fn(&str) -> bool, timeout: Duration) -> Option<String> {
        let t0 = Instant::now();
        let mut last = self.history();
        while t0.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(80));
            let now = self.history();
            if now == last && ready(&self.screen()) {
                return Some(now);
            }
            last = now;
        }
        None
    }

    fn wait_gone(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if !self.screen().contains(needle) {
                return Some(t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        None
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        let _ = std::fs::remove_file(std::env::temp_dir().join(format!("{}.conf", self.socket)));
    }
}

fn streamed(lines: usize, pace: Duration) -> mock::Mock {
    let body = mock::text_stream(&mock::numbered_lines(lines));
    mock::serve(move |_, _| mock::Reply::paced(body.clone(), pace))
}

#[test]
fn r_tui_1_a_10k_token_answer_lands_in_tmux_scrollback_exactly_once() {
    // 850 lines of about twelve tokens: a 10k-token answer.
    let m = streamed(850, Duration::from_micros(100));
    let b = Sandbox::new("scrollback");
    let Some(tm) = Tmux::start("scrollback", 100, 30, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["write it all out", "Enter"]);
    assert!(tm.wait_for("tokens", Duration::from_secs(60)).is_some(), "the answer never finished:\n{}", tm.screen());
    let history = tm.history();
    // Two columns of padding in front of every row, never written.
    assert!(history.lines().filter(|l| !l.trim().is_empty()).all(|l| l.starts_with("  ")), "every row padded:\n{history}");
    let got: Vec<&str> = history.lines().map(str::trim).filter(|l| l.starts_with("line ")).collect();
    let want: Vec<String> = mock::numbered_lines(850).lines().map(String::from).collect();
    assert_eq!(got.len(), want.len(), "every line once, none twice");
    assert!(got.iter().zip(&want).all(|(g, w)| g == w), "in order, byte for byte");
    // The prompt line is in scrollback once too, and the live region is not.
    assert_eq!(history.matches("▎ write it all out").count(), 1, "{history}");
    assert_eq!(history.matches("esc to interrupt").count(), 0, "a live row leaked into scrollback");
}

/// `/new`, as `/clear` in Claude Code: the screen and its scrollback are
/// cleared, and the header is all there is, the prompt under it.
#[test]
fn new_clears_the_screen_and_scrollback_down_to_the_header() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("new-screen");
    let Some(tm) = Tmux::start("new-screen", 100, 30, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["read README.md and summarise it", "Enter"]);
    assert!(tm.wait_for("tokens", Duration::from_secs(20)).is_some(), "{}", tm.screen());
    tm.keys(&["/new", "Enter"]);
    let history = tm.wait_still(|s| !s.contains("tokens") && s.contains("Directory:"), Duration::from_secs(10)).unwrap_or_else(|| panic!("no clean header screen after /new:\n{}", tm.history()));
    assert!(!history.contains("summarise it"), "the earlier session is gone, scrollback too:\n{history}");
    assert_eq!(history.matches("Directory:").count(), 1, "one header:\n{history}");
    assert!(history.lines().count() <= 30, "nothing in scrollback, not even blank rows:\n{history}");
    assert!(history.lines().find(|l| !l.trim().is_empty()).is_some_and(|l| l.contains('▀')), "the header opens the screen:\n{history}");
}

#[test]
fn r_tui_3_a_phone_width_terminal_wraps_and_still_keeps_every_line_once() {
    let m = streamed(200, Duration::from_micros(100));
    let b = Sandbox::new("phone");
    let Some(tm) = Tmux::start("phone", 40, 20, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("→", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["go", "Enter"]);
    assert!(tm.wait_for("tokens", Duration::from_secs(60)).is_some(), "{}", tm.screen());
    let history = tm.history();
    assert!(history.lines().all(|l| l.trim_end().chars().count() <= 38), "a row into the right padding:\n{history}");
    // krowk wrapped the answer inside the padding, so each streamed line is
    // its first row and the rows after it, up to the next line: joined
    // back, every line exactly as it was streamed, once and in order.
    let mut got: Vec<String> = Vec::new();
    for row in history.lines().map(str::trim) {
        if row.starts_with("line ") {
            got.push(row.to_string());
        } else if let Some(last) = got.last_mut()
            && !row.is_empty()
            && !last.ends_with("again")
        {
            last.push(' ');
            last.push_str(row);
        }
    }
    let want: Vec<String> = mock::numbered_lines(200).lines().map(String::from).collect();
    assert_eq!(got, want, "the answer, wrapped by krowk, is in scrollback once and in order");
}

#[test]
fn r_tui_3_a_widened_terminal_keeps_the_answer_in_scrollback_once() {
    // Printed at 40 columns, the answer's lines are wrapped by krowk inside
    // its padding; widened to 100 they stay as they were printed, each once
    // (Ctrl-Y copies the answer unwrapped).
    let m = streamed(20, Duration::from_micros(100));
    let b = Sandbox::new("widen");
    let Some(tm) = Tmux::start("widen", 40, 30, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("→", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["go", "Enter"]);
    assert!(tm.wait_for("tokens", Duration::from_secs(30)).is_some(), "{}", tm.screen());
    tm.tmux(&["resize-window", "-t", "t", "-x", "100", "-y", "30"]);
    std::thread::sleep(Duration::from_millis(500));
    let history = tm.history();
    let firsts = history.lines().filter(|l| l.trim().starts_with("line ")).count();
    let lasts = history.lines().filter(|l| l.trim_end().ends_with("dog again")).count();
    assert_eq!((firsts, lasts), (20, 20), "every line once, start and end:\n{history}");
}

#[test]
fn r_tui_1_a_menu_opened_and_closed_leaves_no_gap_in_scrollback_and_no_space_under_the_prompt() {
    // Enough of an answer that the logo is part in scrollback, part on
    // screen: a menu that pushed rows up and then moved what was left down
    // again split it, and every line under it, with blank rows.
    let m = streamed(12, Duration::from_micros(100));
    let b = Sandbox::new("menus");
    let Some(tm) = Tmux::start("menus", 80, 24, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["go", "Enter"]);
    assert!(tm.wait_for("tokens", Duration::from_secs(30)).is_some(), "{}", tm.screen());
    let at_bottom = |s: &str| s.lines().count() == 24 && s.lines().nth_back(1).is_some_and(|l| l.contains("? help")) && s.lines().last().is_some_and(|l| l.contains("$0.00"));
    // The slash menu closes as its slash is deleted, the help on Esc.
    for (open, close) in [("/", "BSpace"), ("?", "Escape")] {
        for _ in 0..3 {
            tm.keys(&[open]);
            std::thread::sleep(Duration::from_millis(300));
            tm.keys(&[close]);
            std::thread::sleep(Duration::from_millis(300));
            let screen = tm.screen();
            assert!(at_bottom(&screen), "the status line on the last rows after {open}:\n{screen}");
        }
    }
    tm.keys(&["again", "Enter"]);
    assert!(tm.wait_for("▎ again", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    let history = tm.wait_still(|s| s.split("▎ again").nth(1).is_some_and(|after| after.contains("tokens")) && at_bottom(s), Duration::from_secs(30)).unwrap_or_else(|| panic!("the second turn never finished:\n{}", tm.screen()));
    let rows: Vec<&str> = history.lines().collect();
    let logo: Vec<usize> = rows.iter().enumerate().filter(|(_, l)| l.contains('▀')).map(|(i, _)| i).collect();
    assert!(logo.len() > 1 && logo.windows(2).all(|w| w[1] == w[0] + 1), "the logo in one piece:\n{history}");
    let lines: Vec<usize> = rows.iter().enumerate().filter(|(_, l)| l.trim().starts_with("line ")).map(|(i, _)| i).collect();
    assert_eq!(lines.len(), 24, "both answers, every line once:\n{history}");
    assert!(lines[..12].windows(2).all(|w| w[1] == w[0] + 1) && lines[12..].windows(2).all(|w| w[1] == w[0] + 1), "each answer without a gap in it:\n{history}");
    // Between the first answer and the second prompt: its token line, set
    // off by one blank row each side, and nothing else.
    let between: Vec<&str> = rows[lines[11] + 1..].iter().take_while(|l| !l.contains("▎ again")).map(|l| l.trim()).collect();
    assert_eq!(between.iter().filter(|l| l.is_empty()).count(), 2, "no blank rows the menus left behind: {between:?}");
}

#[test]
fn r_tui_3_a_resize_mid_stream_never_repeats_a_line_or_leaves_the_live_region_behind() {
    // A narrower and shorter window mid-stream, then a larger one after.
    let m = streamed(300, Duration::from_millis(1));
    let b = Sandbox::new("resize");
    let Some(tm) = Tmux::start("resize", 100, 30, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["go", "Enter"]);
    assert!(tm.wait_in_history("line 00020", Duration::from_secs(30)).is_some(), "{}", tm.screen());
    tm.tmux(&["resize-window", "-t", "t", "-x", "70", "-y", "20"]);
    assert!(tm.wait_for("tokens", Duration::from_secs(60)).is_some(), "{}", tm.screen());
    tm.tmux(&["resize-window", "-t", "t", "-x", "120", "-y", "40"]);
    // Redrawn at 120x40: the status bar on the last of forty rows.
    let redrawn = |s: &str| s.lines().count() == 40 && s.lines().nth_back(1).is_some_and(|l| l.contains("? help")) && s.matches("? help").count() == 1;
    let history = tm.wait_still(redrawn, Duration::from_secs(10)).unwrap_or_else(|| panic!("never redrawn after the resize:\n{}", tm.screen()));
    // A frame already on its way when the terminal changes size is read at
    // the new size; it moves from the caret, so it still lands where it was
    // meant to (crates/krowk-tui/tests/resize.rs makes that race happen
    // every time). Nothing is ever there twice, nothing of the live region
    // is left behind, and every line is there, in order — but a resize the
    // terminal takes in the middle of one frame's bytes, which the terminal
    // alone decides, can still cost the line in flight.
    let seen: Vec<u32> = history.lines().filter_map(|l| l.trim_start().strip_prefix("line ")?.get(..5)?.parse().ok()).collect();
    let mut sorted = seen.clone();
    sorted.dedup();
    assert_eq!(sorted, seen, "a line twice, or out of order:\n{history}");
    // At most the one line in flight at each of the two resizes, that way.
    let missing: Vec<u32> = (1..=300).filter(|n| !seen.contains(n)).collect();
    assert!(missing.len() <= 2, "more than a line lost per resize: {missing:?}\n{history}");
    assert!(seen.contains(&300), "the end of the answer is there:\n{history}");
    for live in ["esc to interrupt", "type to steer"] {
        assert!(!history.contains(live), "the old live region was left in scrollback:\n{history}");
    }
    assert_eq!(history.matches("▎ go").count(), 1, "{history}");
    let bars = tm.screen().matches("? help").count();
    assert_eq!(bars, 1, "one status bar on screen after two resizes:\n{}", tm.screen());
}

/// The live region at its widest — the offline notice, the keys overlay,
/// the prompt and the status bar, every row split two or three ways by a
/// reflow at 40 columns — then narrowed by `steps`. Each of those rows must
/// be in scrollback exactly once afterwards.
fn narrowing(name: &str, before: &str, steps: &[&str]) {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let b = Sandbox::new(name);
    let Some(tm) = Tmux::start_after(name, 100, 30, &b.root.join("repo"), &b.env(&format!("http://127.0.0.1:{port}")), &[], before) else { return };
    assert!(tm.wait_for("no network connectivity", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["quit"]);
    tm.wait_still(|s: &str| s.contains("→ quit"), Duration::from_secs(5)).unwrap_or_else(|| panic!("never typed:\n{}", tm.screen()));
    // Back to back in one tmux command: no frame in between.
    let mut args: Vec<&str> = Vec::new();
    for (i, w) in steps.iter().enumerate() {
        if i > 0 {
            args.push(";");
        }
        args.extend(["resize-window", "-t", "t", "-x", w, "-y", "30"]);
    }
    tm.tmux(&args);
    std::thread::sleep(Duration::from_millis(800));
    let history = tm.history();
    for row in ["⚠ no network connectivity", "→ quit", "offline | ? help"] {
        assert_eq!(history.matches(row).count(), 1, "{row:?} is in scrollback twice — the old live region was left behind:\n{history}");
    }
    assert_eq!(history.matches("Model:     anthropic/claude-opus-5-5").count(), 1, "the header is still there, once:\n{history}");
    let rules = history.lines().filter(|l| l.trim().len() > 3 && l.trim().chars().all(|c| c == '─')).count();
    assert_eq!(rules, 2, "one prompt, its two rules once each:\n{history}");
    // At 40 columns the status line is two rows still: the device gave
    // way, the model is cut short, offline and the help stay, and the cost
    // is under them.
    let screen = tm.screen();
    let mut rows = screen.lines().map(str::trim_end).filter(|l| !l.is_empty()).rev();
    let (cost, bar) = (rows.next().unwrap_or_default(), rows.next().unwrap_or_default());
    assert!(bar.starts_with("    Claude Opus") && bar.ends_with(" | offline | ? help") && bar.chars().count() <= 40 && !bar.contains('$'), "{bar:?}\n{screen}");
    assert_eq!(cost, "    $0.00", "{screen}");
    if !before.is_empty() {
        // What was on the terminal is kept: the open scrolls it into
        // scrollback, the way a clear that keeps scrollback does, and the
        // header comes after it and the blank screen it left.
        let lines: Vec<&str> = history.lines().collect();
        let last = lines.iter().rposition(|l| l.starts_with("earlier output")).expect("the earlier output is kept");
        let n: usize = lines[last].trim_start_matches("earlier output ").trim().parse().unwrap();
        assert_eq!(lines.iter().filter(|l| l.starts_with("earlier output")).count(), n, "every earlier line, once:\n{history}");
        let header = lines.iter().position(|l| l.trim_start().starts_with("▀▀▀▀▀▀")).expect("the header");
        assert!(header > last && lines[last + 1..header].iter().all(|l| l.trim().is_empty()), "only the cleared screen between the earlier output and the header:\n{history}");
    }
}

#[test]
fn r_tui_3_narrowing_a_fresh_terminal_leaves_no_reflowed_live_region_in_scrollback() {
    narrowing("narrow-fresh", "", &["40"]);
}

#[test]
fn r_tui_3_narrowing_under_a_screenful_leaves_no_reflowed_live_region_in_scrollback() {
    narrowing("narrow-full", "for i in $(seq 1 40); do echo \"earlier output $i\"; done;", &["40"]);
}

#[test]
fn r_tui_3_narrowing_under_a_few_lines_leaves_no_reflowed_live_region_in_scrollback() {
    narrowing("narrow-few", "for i in $(seq 1 5); do echo \"earlier output $i\"; done;", &["40"]);
}

#[test]
fn r_tui_3_two_narrowings_back_to_back_leave_no_fragment() {
    narrowing("narrow-twice", "", &["70", "40"]);
}

#[test]
fn ctrl_z_suspends_to_the_shell_and_fg_brings_the_prompt_back() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("ctrlz");
    // A job-control shell, as a person has, and krowk typed into it.
    if Command::new("tmux").arg("-V").output().is_err() {
        assert!(!cfg!(target_os = "linux") || std::env::var_os("CI").is_none(), "tmux is not installed, and CI on Linux must run this check");
        return;
    }
    let socket = format!("krowk-tui-ctrlz-sh-{}", std::process::id());
    let tmux = |args: &[&str]| String::from_utf8_lossy(&Command::new("tmux").args(["-L", &socket]).args(args).output().unwrap().stdout).into_owned();
    let mut envs = String::new();
    for (k, v) in b.env(&m.url) {
        envs += &format!(" {k}='{v}'");
    }
    let shell = format!("cd '{}' && exec env -i{envs} PS1='sh$ ' bash --norc --noprofile -i", b.root.join("repo").display());
    tmux(&["-f", "/dev/null", "new-session", "-d", "-s", "t", "-x", "100", "-y", "30", &shell]);
    let screen = || tmux(&["capture-pane", "-p", "-t", "t"]);
    let wait = |needle: &str| (0..250).any(|_| screen().contains(needle) || { std::thread::sleep(Duration::from_millis(40)); false });
    assert!(wait("sh$"), "{}", screen());
    tmux(&["send-keys", "-t", "t", &format!("'{}'", env!("CARGO_BIN_EXE_krowk")), "Enter"]);
    assert!(wait("Plan, search, build anything"), "{}", screen());
    tmux(&["send-keys", "-t", "t", "C-z"]);
    assert!(wait("Stopped"), "Ctrl-Z did not stop krowk:\n{}", screen());
    let stopped = screen();
    assert!(!stopped.contains("Plan, search, build anything"), "the live region was cleared before stopping:\n{stopped}");
    tmux(&["send-keys", "-t", "t", "fg", "Enter"]);
    assert!(wait("Plan, search, build anything"), "fg did not bring the prompt back:\n{}", screen());
    tmux(&["send-keys", "-t", "t", "C-d"]);
    assert!(wait("krowk --resume") || wait("sh$"), "{}", screen());
    tmux(&["kill-server"]);
}

/// A TCP relay in front of the stand-in API that can be cut: new
/// connections refused (its port closed) and open ones left hanging with
/// nothing moving — what a dropped Wi-Fi looks like from here.
struct Relay {
    port: u16,
    cut: Arc<AtomicBool>,
}

impl Relay {
    fn new(upstream: &str) -> Relay {
        let upstream = upstream.trim_start_matches("http://").to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cut = Arc::new(AtomicBool::new(false));
        let c = cut.clone();
        std::thread::spawn(move || {
            let mut listener = Some(listener);
            loop {
                if c.load(Ordering::SeqCst) {
                    listener = None;
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let l = listener.get_or_insert_with(|| TcpListener::bind(("127.0.0.1", port)).expect("the relay's port back"));
                l.set_nonblocking(true).unwrap();
                match l.accept() {
                    Ok((down, _)) => {
                        down.set_nonblocking(false).unwrap();
                        let Ok(up) = TcpStream::connect(&upstream) else { continue };
                        pipe(down.try_clone().unwrap(), up.try_clone().unwrap(), c.clone());
                        pipe(up, down, c.clone());
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Relay { port, cut }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn pipe(mut from: TcpStream, mut to: TcpStream, cut: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match from.read(&mut buf) {
                // One side closed: so is the other, as a real hop would.
                Ok(0) | Err(_) => {
                    let _ = to.shutdown(std::net::Shutdown::Write);
                    return;
                }
                Ok(n) => n,
            };
            // Cut: whatever is in flight is dropped, and the connection
            // stays open and silent.
            while cut.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            if to.write_all(&buf[..n]).is_err() {
                return;
            }
        }
    });
}

#[test]
fn r_off_1_a_cut_network_shows_the_notice_within_two_seconds_and_nothing_hangs() {
    // A slow answer: fifty tokens a second, for long enough to cut it.
    let m = streamed(400, Duration::from_millis(20));
    let relay = Relay::new(&m.url);
    let b = Sandbox::new("offline");
    let Some(tm) = Tmux::start("offline", 100, 30, &b.root.join("repo"), &b.env(&relay.url()), &[]) else { return };
    assert!(tm.wait_for("? help", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    // The start-up probe, answered: nothing on screen says so.
    std::thread::sleep(Duration::from_millis(300));
    tm.keys(&["tell me everything", "Enter"]);
    assert!(tm.wait_in_history("line 00003", Duration::from_secs(10)).is_some(), "{}", tm.screen());

    relay.cut.store(true, Ordering::SeqCst);
    let shown = tm.wait_for("no network connectivity", Duration::from_secs(5)).unwrap_or_else(|| panic!("no notice:\n{}", tm.screen()));
    assert!(shown <= Duration::from_secs(2), "the notice took {shown:?}");
    assert!(tm.screen().contains("| offline | ? help"), "the status line says so too, just before the help:\n{}", tm.screen());

    // Nothing hangs: Esc stops the stalled turn, and what arrived is kept.
    tm.keys(&["Escape"]);
    assert!(tm.wait_for("interrupted", Duration::from_secs(5)).is_some(), "the turn did not stop:\n{}", tm.screen());
    std::thread::sleep(Duration::from_millis(1500));
    assert!(tm.screen().contains("no network connectivity"), "the notice is persistent:\n{}", tm.screen());

    relay.cut.store(false, Ordering::SeqCst);
    let cleared = tm.wait_gone("no network connectivity", Duration::from_secs(15)).unwrap_or_else(|| panic!("the notice never cleared:\n{}", tm.screen()));
    assert!(cleared <= Duration::from_secs(12), "{cleared:?}");
    assert!(tm.wait_gone("offline |", Duration::from_secs(2)).is_some(), "the row under the prompt says so too:\n{}", tm.screen());
}

/// A settings file that does not load is named before the trust question
/// is asked, so no answer is saved for a TUI that then refuses to run.
#[test]
fn r_perm_1_a_settings_error_is_named_before_the_trust_question() {
    let b = Sandbox::new("settings-before-trust");
    // The repository's allow rule would put the trust question; the deny
    // rule in the person's krowk config does not parse.
    std::fs::create_dir_all(b.root.join("repo/.claude")).unwrap();
    std::fs::write(b.root.join("repo/.claude/settings.json"), r#"{"permissions": {"allow": ["Bash(npm test)"]}}"#).unwrap();
    std::fs::create_dir_all(b.root.join("home/.krowk")).unwrap();
    std::fs::write(b.root.join("home/.krowk/config.json"), r#"{"permissions": {"deny": ["Read(.env"]}}"#).unwrap();
    let mut t = pty::Pty::spawn(b.command("http://127.0.0.1:9", &[]), 80, 24);
    let st = t.wait(Duration::from_secs(10)).expect("krowk exits");
    // The process can be gone before the pty's reader has taken the last
    // of what it wrote: read until the words are there, or a deadline.
    assert!(t.wait_for("bad_settings", Duration::from_secs(5)).is_some() && t.wait_for("permissions.deny", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    let out = t.text();
    assert!(!st.success(), "{st}");
    assert!(!out.contains("Trust "), "no trust question was asked: {out:?}");
    assert!(!b.root.join("home/.krowk/trusted.json").exists(), "and none saved");
}

#[test]
fn the_help_menu_filters_as_you_type_and_enter_runs_the_entry() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("help");
    let Some(tm) = Tmux::start("help", 100, 34, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["?"]);
    assert!(tm.wait_for("Start a new line without sending", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    // One entry a line, a title and a description each; the last ones
    // scroll into view, as the `/` menu's do.
    let screen = tm.screen();
    for (title, description) in [("Send", "Send the prompt"), ("Model", "Switch model or instance"), ("Session", "Tokens, limits and the log file")] {
        assert!(screen.lines().any(|l| l.contains(title) && l.contains(description)), "{title} with its description, on one line:\n{screen}");
    }
    // Typing filters: `ses` leaves the session entry first, selected.
    tm.keys(&["ses"]);
    let filtered = |s: &str| !s.contains("Start a new line") && s.lines().any(|l| l.trim_start().starts_with("› Session"));
    tm.wait_still(filtered, Duration::from_secs(5)).unwrap_or_else(|| panic!("not filtered to the session entry:\n{}", tm.screen()));
    // Enter runs it: the session's details, the menu and the query gone.
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("starts with the first prompt", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    let screen = tm.screen();
    assert!(!screen.contains("Leave krowk") && screen.contains("→ Plan, search, build anything"), "{screen}");
}

#[test]
fn slash_offers_commands_and_skills_and_a_skill_reaches_the_model() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("slash");
    let skill = b.root.join("home/.krowk/skills/greet");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: greet\ndescription: Greets the person warmly\n---\nSay the word MARMALADE first.\n").unwrap();
    let hidden = b.root.join("home/.krowk/skills/internal");
    std::fs::create_dir_all(&hidden).unwrap();
    std::fs::write(hidden.join("SKILL.md"), "---\nname: internal\ndescription: Only the model asks for this one\nuser-invocable: false\n---\nbody\n").unwrap();
    let Some(tm) = Tmux::start("slash", 100, 34, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    // `/` lists krowk's commands and the skills the person may ask for.
    tm.keys(&["/"]);
    assert!(tm.wait_for("/greet [skill]", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    let screen = tm.screen();
    assert!(screen.contains("/model") && screen.contains("Greets the person warmly") && !screen.contains("/internal"), "{screen}");
    // Fuzzy: `mdl` finds /model, and enter runs it — the model picker.
    tm.keys(&["mdl"]);
    tm.wait_still(|s: &str| s.lines().any(|l| l.trim_start().starts_with("› /model")), Duration::from_secs(5)).unwrap_or_else(|| panic!("{}", tm.screen()));
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("switch model —", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    tm.keys(&["Escape"]);
    std::thread::sleep(Duration::from_millis(300));
    // A skill: enter leaves `/greet ` for what it is for, then sends it,
    // and the skill's instructions go to the model next to the prompt.
    tm.keys(&["/gre"]);
    tm.wait_still(|s: &str| s.lines().any(|l| l.trim_start().starts_with("› /greet")), Duration::from_secs(5)).unwrap_or_else(|| panic!("{}", tm.screen()));
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("→ /greet", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    tm.keys(&["the team", "Enter"]);
    assert!(tm.wait_for("Loaded the greet skill", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    assert!(tm.wait_for("tokens", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    let seen = m.seen.lock().unwrap();
    let first = seen.iter().find(|s| s.body["messages"].is_array()).expect("the model was asked").body["messages"].to_string();
    assert!(first.contains("/greet the team") && first.contains("MARMALADE"), "the prompt and the skill's body: {first}");
}

#[test]
fn r_tui_1_a_menu_opened_and_closed_on_a_short_session_puts_nothing_in_scrollback() {
    // The session all on screen: a menu takes the blank rows above it and
    // gives them back, and none of them is scrolled into history on the way.
    let m = streamed(2, Duration::from_micros(100));
    let b = Sandbox::new("short");
    let Some(tm) = Tmux::start_after("short", 80, 40, &b.root.join("repo"), &b.env(&m.url), &[], "seq 1 50;") else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    let gap = |h: &str| {
        let rows: Vec<&str> = h.lines().collect();
        let logo = rows.iter().position(|l| l.contains('▀')).unwrap_or_else(|| panic!("no logo:\n{h}"));
        logo - rows.iter().position(|l| l.trim() == "50").unwrap_or_else(|| panic!("no shell output:\n{h}"))
    };
    let before = gap(&tm.history());
    for _ in 0..3 {
        tm.keys(&["?"]);
        std::thread::sleep(Duration::from_millis(300));
        tm.keys(&["Escape"]);
        std::thread::sleep(Duration::from_millis(300));
    }
    let history = tm.history();
    assert_eq!(gap(&history), before, "rows between the shell's output and the logo:\n{history}");
    let screen = tm.screen();
    assert!(screen.lines().nth_back(1).is_some_and(|l| l.contains("? help")), "the status line on the last rows:\n{screen}");
}

/// Whether `needle` is in what the TUI wrote after byte `from`, within
/// `timeout`.
fn wait_after(t: &pty::Pty, from: usize, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if String::from_utf8_lossy(&t.output()[from..]).contains(needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The trust question's own words, which only it says.
const TRUST_ASKED: &str = "n or esc does not";

/// A sandbox with only a signed-in Claude subscription — no key unless
/// `key` — whose status check takes `delay` seconds, logging to fake.log.
fn subscription_only(b: &Sandbox, key: bool, delay: &str) -> Command {
    std::fs::create_dir_all(b.root.join("home/.claude")).unwrap();
    std::fs::write(b.root.join("home/.claude/fake-login"), "").unwrap();
    let mut c = b.command("http://127.0.0.1:9", &[]);
    if !key {
        c.env_remove("ANTHROPIC_API_KEY");
    }
    c.env("FAKE_CLAUDE_STATUS_DELAY", delay).env("FAKE_CLAUDE_LOG", b.root.join("fake.log"));
    c
}

fn fake_turns(b: &Sandbox) -> String {
    std::fs::read_to_string(b.root.join("fake.log")).unwrap_or_default().lines().filter(|l| l.starts_with("argv -p")).collect::<Vec<_>>().join("\n")
}

/// R-PERF-1 with no key: the only instance is a Claude subscription whose
/// status check takes three seconds. The first frame does not wait for it;
/// the TUI routes once it is up, then asks the trust question itself —
/// the model was not known before it took the terminal — for the prompt
/// sent meanwhile. Keys typed ahead as the question comes up are no
/// answer; `y` on an empty prompt, once it has settled, is.
#[test]
fn with_no_key_the_first_frame_never_waits_on_a_vendor_and_the_routed_backend_asks_trust_in_the_tui() {
    let b = Sandbox::new("nokey");
    let c = subscription_only(&b, false, "3");
    let started = Instant::now();
    let mut t = pty::Pty::spawn(c, 100, 30);
    assert!(t.wait_for("? help", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    let first = started.elapsed();
    assert!(first < Duration::from_millis(2500), "the first frame waited {first:?} on a vendor's status check");
    assert!(!t.text().contains("claude/claude-opus-5-5"), "not routed yet: {:?}", t.text());
    t.write(b"hello there\r");
    assert!(t.wait_for(TRUST_ASKED, Duration::from_secs(15)).is_some(), "no trust question: {:?}", t.text());
    assert!(t.text().contains("claude/claude-opus-5-5 runs Claude Code"), "{:?}", t.text());
    // Typed ahead as it came up: "yes" lands in the prompt, and trusts
    // nothing — not then, nor once the question has settled.
    t.write(b"yes");
    std::thread::sleep(Duration::from_millis(800));
    t.write(b"y");
    std::thread::sleep(Duration::from_millis(300));
    assert!(fake_turns(&b).is_empty(), "typed-ahead keys answered the trust question: {}", fake_turns(&b));
    // Ctrl-C clears what was typed; then `y` on the empty prompt answers.
    t.write(b"\x03");
    std::thread::sleep(Duration::from_millis(100));
    t.write(b"y");
    let deadline = Instant::now() + Duration::from_secs(15);
    while fake_turns(&b).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(fake_turns(&b).contains("--model claude-opus-5-5"), "the held prompt ran on the routed backend once trusted: {}", fake_turns(&b));
}

/// The route landing asks nothing by itself; the question comes with a
/// send, `n` puts the prompt back unsent, and the next send asks again.
#[test]
fn trust_is_asked_on_send_and_a_no_puts_the_prompt_back_to_be_asked_again() {
    let b = Sandbox::new("trustno");
    b.full_width();
    let mut t = pty::Pty::spawn(subscription_only(&b, false, "0"), 100, 30);
    assert!(t.wait_for("Claude Opus 5.5 (claude) |", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    assert!(!t.text().contains(TRUST_ASKED), "asked before anything was sent: {:?}", t.text());
    t.write(b"first-try\r");
    assert!(t.wait_for(TRUST_ASKED, Duration::from_secs(10)).is_some(), "{:?}", t.text());
    std::thread::sleep(Duration::from_millis(600));
    let at = t.output().len();
    t.write(b"n");
    assert!(wait_after(&t, at, "not trusted, so nothing ran", Duration::from_secs(5)), "{:?}", t.text());
    assert!(wait_after(&t, at, "first-try", Duration::from_secs(5)), "the prompt is back in the editor: {:?}", String::from_utf8_lossy(&t.output()[at..]));
    let at = t.output().len();
    t.write(b"\r");
    assert!(wait_after(&t, at, TRUST_ASKED, Duration::from_secs(5)), "asked again on the next send: {:?}", t.text());
    assert!(fake_turns(&b).is_empty(), "nothing ran: {}", fake_turns(&b));
}

/// Prompts sent while the route is pending are all kept, joined; a route
/// that fails puts them back unsent, and so does Ctrl-C while they wait —
/// which quits nothing.
#[test]
fn prompts_held_for_the_route_are_joined_and_come_back_on_a_failed_route_or_ctrl_c() {
    // A key and a subscription: the route is ambiguous, after two seconds.
    let b = Sandbox::new("held");
    let mut t = pty::Pty::spawn(subscription_only(&b, true, "2"), 120, 30);
    assert!(t.wait_for("? help", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"held-one\r");
    t.write(b"held-two\r");
    let at = t.output().len();
    assert!(wait_after(&t, at, "(ambiguous_model)", Duration::from_secs(15)), "{:?}", t.text());
    assert!(wait_after(&t, at, "held-one", Duration::from_secs(5)) && wait_after(&t, at, "held-two", Duration::from_secs(5)), "both came back: {:?}", t.text());

    let b = Sandbox::new("heldctrlc");
    // Five seconds to answer: the route is still asking when krowk quits.
    let mut t = pty::Pty::spawn(subscription_only(&b, false, "5"), 120, 30);
    assert!(t.wait_for("? help", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"wait-for-it\r");
    assert!(t.wait_for("choosing the model", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"\x03");
    assert!(wait_after(&t, at, "not sent — it is back in the prompt", Duration::from_secs(5)), "{:?}", t.text());
    assert!(wait_after(&t, at, "wait-for-it", Duration::from_secs(5)), "{:?}", t.text());
    std::thread::sleep(Duration::from_millis(300));
    assert!(t.alive(), "Ctrl-C with a prompt waiting quits nothing");
    assert!(!t.text().contains("claude/claude-opus-5-5"), "the route is still being asked: {:?}", t.text());
    // Quitting while the route is still being asked does not wait for it.
    t.write(b"\x03");
    t.write(b"\x03");
    let asked = Instant::now();
    assert!(t.wait(Duration::from_secs(10)).is_some(), "krowk quits");
    assert!(asked.elapsed() < Duration::from_secs(2), "quitting waited {:?} for the route", asked.elapsed());
}

/// With an API key and a signed-in Claude subscription, nothing the TUI
/// routed for itself counts as the session's instance: no model at start
/// and `/model sonnet` are both refused as ambiguous, never sent to the
/// key.
#[test]
fn with_a_key_and_a_subscription_model_sonnet_is_refused_as_ambiguous_in_the_tui() {
    let b = Sandbox::new("ambiguous");
    std::fs::create_dir_all(b.root.join("home/.claude")).unwrap();
    std::fs::write(b.root.join("home/.claude/fake-login"), "").unwrap();
    let mut t = pty::Pty::spawn(b.command("http://127.0.0.1:9", &[]), 120, 30);
    assert!(t.wait_for("(ambiguous_model)", Duration::from_secs(15)).is_some(), "{:?}", t.text());
    t.write(b"/model sonnet\r");
    assert!(t.wait_for("could run \"sonnet\"", Duration::from_secs(15)).is_some(), "{:?}", t.text());
    let text = t.text();
    assert!(text.contains("claude, Claude subscription: --model claude/sonnet") && !text.contains("now on anthropic"), "{text:?}");
}

// ---- /connect, /disconnect and the first-run card ------------------------------

/// What the TUI wrote after byte `from`, as words: a blank cell is skipped,
/// not written — the next word starts with a move to its column — so each
/// such move reads as the space it stands for, and styles as nothing.
fn words(t: &pty::Pty, from: usize) -> String {
    let raw = String::from_utf8_lossy(&t.output()[from..]).into_owned();
    let mut out = String::new();
    let mut rest = raw.as_str();
    while let Some(i) = rest.find('\x1b') {
        out.push_str(&rest[..i].replace('\r', ""));
        let seq = &rest[i + 1..];
        let end = if let Some(csi) = seq.strip_prefix('[') { csi.find(|c: char| c.is_ascii_alphabetic()).map(|e| e + 2) } else { Some(1) };
        let Some(end) = end.filter(|e| *e <= seq.len()) else { break };
        if seq[..end].ends_with('C') && rest[..i].ends_with('\r') {
            out.push(' ');
        }
        rest = &seq[end..];
    }
    out.push_str(rest);
    out
}

/// Whether `needle`, as words, is in what the TUI wrote after `from`,
/// within `timeout`.
fn says(t: &pty::Pty, from: usize, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if words(t, from).contains(needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
/// No key, no config, and a `claude` signed in to nothing: nothing here
/// can run a model. `FAKE_CLAUDE_LOGIN=ask` makes its login wait for a line
/// on the terminal, as the real one waits on a person.
fn fresh(b: &Sandbox, url: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = b.env(url).into_iter().filter(|(k, _)| k != "ANTHROPIC_API_KEY").collect();
    env.push(("FAKE_CLAUDE_LOG".into(), b.root.join("fake.log").display().to_string()));
    env.push(("FAKE_CLAUDE_LOGIN".into(), "ask".into()));
    env
}

fn fresh_command(b: &Sandbox, url: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.env_clear().envs(fresh(b, url)).current_dir(b.root.join("repo"));
    c
}

/// The first run: with nothing ready, the TUI opens "Connect a provider"
/// instead of letting the first prompt fail. Claude subscription, the
/// built-in account: Claude Code's own login runs on the real terminal —
/// the TUI gave it up, so the Enter typed reaches the vendor, not the
/// prompt — and once it is signed in the session is on it and a prompt
/// runs there.
#[test]
fn the_first_run_card_connects_a_claude_subscription_and_the_prompt_runs_on_it() {
    let b = Sandbox::new("firstrun");
    b.full_width();
    let mut t = pty::Pty::spawn(fresh_command(&b, "http://127.0.0.1:9"), 110, 34);
    assert!(says(&t, 0, "Nothing here can run a model yet", Duration::from_secs(15)), "no first-run card: {:?}", t.text());
    assert!(!t.text().contains("none_ready"), "the failure is not shown, the card is: {:?}", t.text());
    assert!(says(&t, 0, "Connect which provider?", Duration::from_secs(5)), "{:?}", t.text());
    let at = t.output().len();
    settle();
    t.write(b"\r");
    assert!(says(&t, at, "How do you connect anthropic?", Duration::from_secs(5)), "{:?}", t.text());
    let at = t.output().len();
    settle();
    t.write(b"\r");
    assert!(says(&t, at, "not signed in, reconnect", Duration::from_secs(10)), "the account, with its readiness: {:?}", t.text());
    let at = t.output().len();
    settle();
    t.write(b"\r");
    assert!(says(&t, at, "Press Enter to sign in to Claude", Duration::from_secs(10)), "the vendor's login never ran: {:?}", t.text());
    let at = t.output().len();
    t.write(b"\r");
    assert!(says(&t, at, "Connected claude", Duration::from_secs(10)), "{:?}", t.text());
    assert!(says(&t, 0, "now on claude/claude-opus-5-5", Duration::from_secs(5)), "{:?}", t.text());
    let log = std::fs::read_to_string(b.root.join("fake.log")).unwrap_or_default();
    assert!(log.lines().any(|l| l == "login-read "), "the Enter went to the vendor's login: {log}");
    let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(b.root.join("home/.krowk/config.json")).unwrap()).unwrap();
    assert_eq!(config["defaultModel"], "claude/claude-opus-5-5", "{config}");
    // The prompt: the trust question for the repository, then the turn.
    let at = t.output().len();
    t.write(b"hello there\r");
    assert!(says(&t, at, TRUST_ASKED, Duration::from_secs(10)), "{:?}", t.text());
    std::thread::sleep(Duration::from_millis(600));
    let at = t.output().len();
    t.write(b"y");
    assert!(says(&t, at, "Worked for", Duration::from_secs(15)), "the prompt did not run: {:?}", t.text());
    assert!(fake_turns(&b).contains("--model claude-opus-5-5"), "{}", fake_turns(&b));
    // A /connect cancelled at its first pick changes nothing: the next turn
    // runs on the same Claude process.
    let at = t.output().len();
    t.write(b"/connect\r");
    assert!(says(&t, at, "Connect which provider?", Duration::from_secs(5)), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"\x1b");
    assert!(says(&t, at, "nothing was chosen", Duration::from_secs(5)), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"once more\r");
    assert!(says(&t, at, "Worked for", Duration::from_secs(15)), "{:?}", t.text());
    assert_eq!(fake_turns(&b).lines().count(), 1, "one Claude process for both turns: {}", fake_turns(&b));
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
}

/// A key pasted at `/connect`'s question is stored, never drawn back — not
/// in the overlay, not in scrollback, not in the prompt's history — and the
/// next prompt runs on it against the stand-in API.
#[test]
fn a_key_pasted_in_connect_is_never_shown_and_the_prompt_runs_on_it() {
    const KEY: &str = "sk-ant-pasted-4242-secret";
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("pastekey");
    let mut t = pty::Pty::spawn(fresh_command(&b, &m.url), 110, 34);
    assert!(says(&t, 0, "Nothing here can run a model yet", Duration::from_secs(15)), "{:?}", t.text());
    assert!(says(&t, 0, "Connect which provider?", Duration::from_secs(5)), "{:?}", t.text());
    // Put away, the card says how to come back.
    let at = t.output().len();
    t.write(b"\x1b");
    assert!(says(&t, at, "/connect when you are ready", Duration::from_secs(5)), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"/connect\r");
    assert!(says(&t, at, "Connect which provider?", Duration::from_secs(5)), "{:?}", t.text());
    let at = t.output().len();
    settle();
    t.write(b"\r");
    assert!(says(&t, at, "How do you connect anthropic?", Duration::from_secs(5)), "{:?}", t.text());
    t.write(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(150));
    let at = t.output().len();
    settle();
    t.write(b"\r");
    assert!(says(&t, at, "key not set, reconnect", Duration::from_secs(10)), "{:?}", t.text());
    let at = t.output().len();
    settle();
    t.write(b"\r");
    assert!(says(&t, at, "Paste a key", Duration::from_secs(10)), "{:?}", t.text());
    let at = t.output().len();
    settle();
    t.write(format!("\x1b[200~{KEY}\x1b[201~").as_bytes());
    assert!(says(&t, at, "•••••••••••••••••••••••••", Duration::from_secs(5)), "one bullet a character: {:?}", t.text());
    let at = t.output().len();
    settle();
    t.write(b"\r");
    assert!(says(&t, at, "Connected anthropic", Duration::from_secs(10)), "{:?}", t.text());
    assert!(says(&t, 0, "in krowk's credentials file", Duration::from_secs(5)), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"read README.md and summarise it in one line\r");
    assert!(says(&t, at, "anywhere.", Duration::from_secs(15)), "no answer: {:?}", t.text());
    let sent = m.seen.lock().unwrap().iter().filter_map(|s| s.header("x-api-key").map(String::from)).collect::<Vec<_>>();
    assert!(!sent.is_empty() && sent.iter().all(|k| k == KEY), "the stored key went to the API: {sent:?}");
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    assert!(!t.text().contains(KEY) && !t.text().contains("4242"), "the key was drawn");
    let history = std::fs::read_to_string(b.root.join("home/.krowk/sessions/tui-history.jsonl")).unwrap_or_default();
    assert!(!history.contains("4242"), "the key is in the prompt's history: {history}");
    let config = std::fs::read_to_string(b.root.join("home/.krowk/config.json")).unwrap();
    assert!(!config.contains("4242"), "the key is in config.json: {config}");
}

/// `/connect`'s account picker renames an account — the one with a name
/// of its own, asked nothing more — and the session goes on on it: the
/// next prompt runs there, and config.json has it under the new name.
#[test]
fn rename_in_the_tui_moves_the_session_onto_the_new_name() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("rename");
    let config = b.root.join("home/.krowk");
    std::fs::create_dir_all(&config).unwrap();
    let instances = serde_json::json!({"instances": {"anthropic:work": {"kind": "anthropic-api", "apiKeyEnv": "ANTHROPIC_API_KEY", "baseUrl": m.url}}});
    std::fs::write(config.join("config.json"), instances.to_string()).unwrap();
    let mut t = pty::Pty::spawn(b.command(&m.url, &["--model", "anthropic:work/claude-sonnet-4-6"]), 110, 34);
    assert!(t.wait_for("anything", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"read README.md and summarise it in one line\r");
    assert!(says(&t, at, "anywhere.", Duration::from_secs(15)), "no answer: {:?}", t.text());
    let at = t.output().len();
    t.write(b"/connect\r");
    assert!(says(&t, at, "Connect which provider?", Duration::from_secs(10)), "{:?}", t.text());
    settle();
    let at = t.output().len();
    // Anthropic, then its API key.
    t.write(b"\r");
    assert!(says(&t, at, "How do you connect", Duration::from_secs(5)), "{:?}", t.text());
    settle();
    let at = t.output().len();
    t.write(b"\x1b[B\r");
    assert!(says(&t, at, "Rename an account", Duration::from_secs(10)), "{:?}", t.text());
    settle();
    let at = t.output().len();
    // Past `anthropic`, `anthropic:work` and "+ Add account…".
    t.write(b"\x1b[B\x1b[B\x1b[B\r");
    assert!(says(&t, at, "New name for anthropic:work", Duration::from_secs(5)), "{:?}", t.text());
    settle();
    let at = t.output().len();
    // `anthropic:` is typed already: `job` finishes the whole name.
    t.write(b"job\r");
    assert!(says(&t, at, "Renamed anthropic:work to anthropic:job", Duration::from_secs(10)), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"what language is it written in?\r");
    assert!(says(&t, at, "It is written in Rust.", Duration::from_secs(15)), "no answer on the new name: {:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(config.join("config.json")).unwrap()).unwrap();
    assert!(cfg["instances"].get("anthropic:job").is_some() && cfg["instances"].get("anthropic:work").is_none(), "{cfg}");
    let log = std::fs::read_dir(config.join("sessions")).unwrap().flatten().filter(|e| e.path().is_dir()).map(|e| std::fs::read_to_string(e.path().join("events.jsonl")).unwrap_or_default()).collect::<String>();
    assert!(log.contains(r#""instance":"anthropic:job""#), "the second turn ran on the new name: {log}");
}

/// A vendor's login has the terminal while it runs: the TUI clears its
/// live region first and draws nothing meanwhile, and takes the terminal
/// back after — at the size it is by then — with nothing of the old
/// region left in scrollback.
#[test]
fn a_suspended_vendor_login_gives_the_terminal_back_whole_after_a_resize() {
    let b = Sandbox::new("suspend");
    let Some(tm) = Tmux::start("suspend", 100, 30, &b.root.join("repo"), &fresh(&b, "http://127.0.0.1:9"), &[]) else { return };
    assert!(tm.wait_for("Connect which provider?", Duration::from_secs(15)).is_some(), "{}", tm.screen());
    settle();
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("How do you connect", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    settle();
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("Which account?", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    settle();
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("Press Enter to sign in to Claude", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    let during = tm.screen();
    for live in ["Plan, search, build anything", "Connect a provider", "? help"] {
        assert!(!during.contains(live), "the live region was left on screen for the vendor: {live:?}\n{during}");
    }
    tm.tmux(&["resize-window", "-t", "t", "-x", "72", "-y", "24"]);
    std::thread::sleep(Duration::from_millis(300));
    tm.keys(&["Enter"]);
    let back = |s: &str| s.contains("now on claude/claude-opus-5-5") && s.contains("→ Plan, search, build anything");
    let history = tm.wait_still(back, Duration::from_secs(15)).unwrap_or_else(|| panic!("never back:\n{}", tm.screen()));
    assert_eq!(history.matches("Press Enter to sign in to Claude").count(), 1, "{history}");
    for gone in ["Connect a provider", "Nothing here can run a model yet", "Which account?"] {
        assert!(!history.contains(gone), "the old live region was left in scrollback: {gone:?}\n{history}");
    }
    assert_eq!(history.matches("? help").count(), 1, "one status line:\n{history}");
    let rules = history.lines().filter(|l| l.trim().len() > 3 && l.trim().chars().all(|c| c == '─')).count();
    assert_eq!(rules, 2, "one prompt box, drawn at the new width:\n{history}");
    let screen = tm.screen();
    assert!(screen.lines().all(|l| l.chars().count() <= 72), "{screen}");
}

/// `/model` lists only what can run here, from checks that never hold a
/// key up — a vendor whose status takes three seconds leaves the picker
/// moving meanwhile, and drops out once it says no — and what is typed
/// filters it. `/disconnect` of the built-in `claude` asks before signing
/// the person out of Claude Code itself.
#[test]
fn model_lists_what_is_ready_in_the_background_and_disconnect_asks_first() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("marks");
    let mut env = b.env(&m.url);
    env.push(("FAKE_CLAUDE_STATUS_DELAY".into(), "3".into()));
    let Some(tm) = Tmux::start("marks", 110, 34, &b.root.join("repo"), &env, &["--model", "anthropic/claude-sonnet-4-6"]) else { return };
    assert!(tm.wait_for("? help", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["/model", "Enter"]);
    assert!(tm.wait_for("switch model", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    let row = |s: &str, name: &str| s.lines().find(|l| l.trim_start().trim_start_matches("› ").starts_with(&format!("{name}/"))).unwrap_or_default().to_string();
    // The header can be drawn a frame before the rows under it.
    tm.wait_still(|s| !row(s, "claude").is_empty(), Duration::from_secs(5));
    let screen = tm.screen();
    assert!(row(&screen, "claude").contains("checking…"), "claude is still being asked: {screen}");
    assert!(!row(&screen, "anthropic").is_empty() && row(&screen, "openai").is_empty(), "a key is checked at once, and one not set is not listed: {screen}");
    // The arrows answer while `claude` is still being asked.
    let chosen = |s: &str| s.lines().find(|l| l.trim_start().starts_with("› ")).unwrap_or_default().to_string();
    let before = chosen(&screen);
    tm.keys(&["Down"]);
    let moved = |s: &str| chosen(s) != before;
    assert!(tm.wait_still(moved, Duration::from_secs(1)).is_some() || moved(&tm.screen()), "a key waited on a vendor check: {}", tm.screen());
    assert!(row(&tm.screen(), "claude").contains("checking…"), "and it was still being asked: {}", tm.screen());
    tm.wait_still(|s| row(s, "claude").is_empty(), Duration::from_secs(15)).unwrap_or_else(|| panic!("claude dropped once its check is back: {}", tm.screen()));
    // What is typed filters the rows.
    tm.keys(&["zzz"]);
    assert!(tm.wait_for("nothing matches · enter runs /model zzz", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    tm.keys(&["Escape"]);
    std::thread::sleep(Duration::from_millis(300));
    // /disconnect of the built-in asks before it signs the person out of
    // Claude Code itself, and no keeps the login.
    std::fs::create_dir_all(b.root.join("home/.claude")).unwrap();
    std::fs::write(b.root.join("home/.claude/fake-login"), "").unwrap();
    tm.keys(&["/disconnect claude", "Enter"]);
    assert!(tm.wait_for("signs you out of Claude Code itself", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    assert!(tm.wait_for("❯ No — keep that login", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    settle();
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("nothing was signed out", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    assert!(b.root.join("home/.claude/fake-login").exists(), "the person's own login is kept");
}

/// Longer than the moment a `/connect` question takes no key in (the
/// approval's settle): a key sent sooner was typed ahead, and is dropped.
fn settle() {
    std::thread::sleep(Duration::from_millis(500));
}

/// Ctrl-C on the terminal a vendor's login has is that login's: it stops
/// the login, not krowk, which takes the terminal back and says the
/// connection was not made.
#[test]
fn ctrl_c_during_a_vendor_login_stops_the_login_and_krowk_takes_the_terminal_back() {
    let b = Sandbox::new("loginctrlc");
    b.full_width();
    let mut t = pty::Pty::spawn(fresh_command(&b, "http://127.0.0.1:9"), 110, 34);
    for question in ["Connect which provider?", "How do you connect anthropic?", "not signed in, reconnect"] {
        assert!(says(&t, 0, question, Duration::from_secs(15)), "{question}: {:?}", t.text());
        settle();
        let at = t.output().len();
        t.write(b"\r");
        if question.starts_with("not signed") {
            assert!(says(&t, at, "Press Enter to sign in to Claude", Duration::from_secs(10)), "{:?}", t.text());
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let at = t.output().len();
    t.write(b"\x03");
    assert!(says(&t, at, "was not connected", Duration::from_secs(10)), "the failed login is said: {:?}", String::from_utf8_lossy(&t.output()[at..]));
    assert!(says(&t, at, "? help", Duration::from_secs(5)), "the TUI is drawn again: {:?}", String::from_utf8_lossy(&t.output()[at..]));
    assert!(t.alive(), "Ctrl-C ended krowk with the login");
    assert!(!b.root.join("home/.krowk/config.json").exists() || !std::fs::read_to_string(b.root.join("home/.krowk/config.json")).unwrap().contains("claude"), "nothing was written");
    // Raw again: Ctrl-D is a key, and quits cleanly.
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
}

/// While `/connect`'s overlay is up, what is typed or pasted is its
/// question's answer or nothing: a key pasted before the question is up,
/// or at a pick, never reaches the prompt, a turn or the history, and
/// Enter never sends the prompt.
#[test]
fn nothing_typed_or_pasted_at_the_connect_overlay_reaches_the_prompt() {
    const KEY: &str = "sk-ant-early-7777-secret";
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("leak");
    // Each vendor check takes two seconds: the overlay is busy meanwhile.
    let mut c = fresh_command(&b, &m.url);
    c.env("FAKE_CLAUDE_STATUS_DELAY", "2");
    let mut t = pty::Pty::spawn(c, 110, 34);
    assert!(says(&t, 0, "Connect which provider?", Duration::from_secs(20)), "{:?}", t.text());
    settle();
    t.write(b"\r");
    assert!(says(&t, 0, "How do you connect anthropic?", Duration::from_secs(5)), "{:?}", t.text());
    settle();
    let at = t.output().len();
    t.write(b"\r");
    // Busy, asking claude for two seconds: a paste, typing and Enter go
    // nowhere.
    std::thread::sleep(Duration::from_millis(300));
    t.write(format!("\x1b[200~{KEY}\x1b[201~").as_bytes());
    t.write(b"typed\r");
    // At the pick, a paste goes nowhere either.
    assert!(says(&t, at, "Which account?", Duration::from_secs(10)), "{:?}", t.text());
    settle();
    t.write(format!("\x1b[200~{KEY}\x1b[201~").as_bytes());
    std::thread::sleep(Duration::from_millis(200));
    // Esc at the pick cancels it, and nothing was connected.
    let at = t.output().len();
    t.write(b"\x1b");
    assert!(says(&t, at, "nothing connected", Duration::from_secs(5)), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
    let out = t.text();
    assert!(!out.contains(KEY) && !out.contains("7777") && !out.contains("typed"), "it reached the screen: {out:?}");
    assert!(m.seen.lock().unwrap().is_empty(), "a prompt was sent");
    let history = std::fs::read_to_string(b.root.join("home/.krowk/sessions/tui-history.jsonl")).unwrap_or_default();
    assert!(!history.contains("7777") && !history.contains("typed"), "{history}");
}

/// A question that comes up with the overlay hidden does not take the
/// keys: what is being typed stays the prompt's, and `/connect` opens it.
#[test]
fn a_connect_question_asked_while_hidden_waits_to_be_opened() {
    let b = Sandbox::new("hidden");
    let mut env = fresh(&b, "http://127.0.0.1:9");
    env.push(("FAKE_CLAUDE_STATUS_DELAY".into(), "2".into()));
    let Some(tm) = Tmux::start("hidden", 110, 34, &b.root.join("repo"), &env, &[]) else { return };
    assert!(tm.wait_for("Connect which provider?", Duration::from_secs(20)).is_some(), "{}", tm.screen());
    settle();
    tm.keys(&["Enter"]);
    assert!(tm.wait_for("How do you connect", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    settle();
    tm.keys(&["Enter"]);
    // Hidden while claude is asked; the person starts a prompt.
    std::thread::sleep(Duration::from_millis(300));
    tm.keys(&["Escape"]);
    std::thread::sleep(Duration::from_millis(200));
    tm.keys(&["-l", "half a prompt"]);
    assert!(tm.wait_for("/connect is waiting for an answer", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["-l", " more"]);
    let typed = |s: &str| s.contains("→ half a prompt more");
    let screen = tm.wait_still(typed, Duration::from_secs(5)).map(|_| tm.screen()).unwrap_or_else(|| panic!("the keys did not stay the prompt's:\n{}", tm.screen()));
    assert!(!screen.contains("Which account?"), "the question took the screen:\n{screen}");
    // Cleared, and opened.
    tm.keys(&["C-u"]);
    tm.keys(&["/connect", "Enter"]);
    assert!(tm.wait_for("Which account?", Duration::from_secs(5)).is_some(), "{}", tm.screen());
}
