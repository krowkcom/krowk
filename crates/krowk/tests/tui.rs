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
            std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(dir).join(bin), &at).unwrap();
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
    // pass through — no alternate screen, no mouse capture, no keyboard
    // protocol push, no full-screen clear.
    for bad in ["\x1b[?1049h", "\x1b[?47h", "\x1b[?1000h", "\x1b[?1002h", "\x1b[?1003h", "\x1b[?1006h", "\x1b[>1u", "\x1b[2J", "\x1b[3J"] {
        assert!(!out.contains(bad), "sent {bad:?}");
    }
    // R-TUI-1: every frame is bracketed, and brackets pair up.
    let (begins, ends) = (t.frames().len(), t.frame_ends().len());
    assert!(begins > 2 && begins == ends, "{begins} frames begun, {ends} ended");
    // The log is the session, and krowk.db lists it.
    let seen = m.seen.lock().unwrap();
    assert!(seen.iter().any(|s| s.body["messages"].as_array().is_some_and(|m| m.len() == 3)), "the tool loop ran");
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
    let config = b.root.join("home/.config/krowk");
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
    assert!(t.wait_for("anthropic:nokey/claude-sonnet-4-6", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    // Esc on its own, not the start of an Alt-/ chord.
    t.write(b"\x1b");
    std::thread::sleep(Duration::from_millis(300));
    t.write(b"/model anthropic:nokey/claude-sonnet-4-6\r");
    assert!(t.wait_for("NO_SUCH_KEY", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    assert!(t.wait_for("stays", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    t.write(b"\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()));
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
    assert!(t.wait_for("anthropic/claude-opus-5-5 |", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"wait forever\r");
    assert!(t.wait_for("esc to interrupt", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"\x03\x03");
    let st = t.wait(Duration::from_secs(10)).expect("krowk exits on the second Ctrl-C");
    assert_eq!(st.code(), Some(130), "{st}");
    let out = t.text();
    assert!(out.contains("krowk --resume ") && out.ends_with("\x1b[?2004l\x1b[?25h"), "{out:?}");
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
    assert_eq!(history.matches("❯ write it all out").count(), 1, "{history}");
    assert_eq!(history.matches("esc to interrupt").count(), 0, "a live row leaked into scrollback");
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
    let redrawn = |s: &str| s.lines().count() == 40 && s.lines().last().is_some_and(|l| l.contains("? help")) && s.matches("? help").count() == 1;
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
    assert_eq!(history.matches("❯ go").count(), 1, "{history}");
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
    // At 40 columns the status line is one row still: the device and the
    // cost gave way, the model is cut short, offline and the help stay.
    let screen = tm.screen();
    let bar = screen.lines().map(str::trim_end).rfind(|l| !l.is_empty()).unwrap_or_default();
    assert!(bar.starts_with("    anthropic/cl") && bar.ends_with(" | offline | ? help") && bar.chars().count() <= 40 && !bar.contains('$'), "{bar:?}\n{screen}");
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
    std::fs::create_dir_all(b.root.join("home/.config/krowk")).unwrap();
    std::fs::write(b.root.join("home/.config/krowk/config.json"), r#"{"permissions": {"deny": ["Read(.env"]}}"#).unwrap();
    let mut t = pty::Pty::spawn(b.command("http://127.0.0.1:9", &[]), 80, 24);
    let st = t.wait(Duration::from_secs(10)).expect("krowk exits");
    // The process can be gone before the pty's reader has taken the last
    // of what it wrote: read until the words are there, or a deadline.
    assert!(t.wait_for("bad_settings", Duration::from_secs(5)).is_some() && t.wait_for("permissions.deny", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    let out = t.text();
    assert!(!st.success(), "{st}");
    assert!(!out.contains("Trust "), "no trust question was asked: {out:?}");
    assert!(!b.root.join("home/.config/krowk/trusted.json").exists(), "and none saved");
}

#[test]
fn the_help_menu_filters_as_you_type_and_enter_runs_the_entry() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("help");
    let Some(tm) = Tmux::start("help", 100, 34, &b.root.join("repo"), &b.env(&m.url), &[]) else { return };
    assert!(tm.wait_for("Plan, search, build anything", Duration::from_secs(10)).is_some(), "{}", tm.screen());
    tm.keys(&["?"]);
    assert!(tm.wait_for("Start a new line without sending", Duration::from_secs(5)).is_some(), "{}", tm.screen());
    // One entry a line, a title and a description each.
    let screen = tm.screen();
    for (title, description) in [("Send", "Send the prompt"), ("Session", "Tokens, limits and the log file"), ("Quit", "Leave krowk")] {
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
    let skill = b.root.join("home/.config/krowk/skills/greet");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: greet\ndescription: Greets the person warmly\n---\nSay the word MARMALADE first.\n").unwrap();
    let hidden = b.root.join("home/.config/krowk/skills/internal");
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
    assert!(tm.wait_for("switch to —", Duration::from_secs(5)).is_some(), "{}", tm.screen());
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
    let mut t = pty::Pty::spawn(subscription_only(&b, false, "0"), 100, 30);
    assert!(t.wait_for("claude/claude-opus-5-5 |", Duration::from_secs(10)).is_some(), "{:?}", t.text());
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
    let mut t = pty::Pty::spawn(subscription_only(&b, false, "3"), 120, 30);
    assert!(t.wait_for("? help", Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"wait-for-it\r");
    assert!(t.wait_for("choosing the model", Duration::from_secs(5)).is_some(), "{:?}", t.text());
    let at = t.output().len();
    t.write(b"\x03");
    assert!(wait_after(&t, at, "not sent — it is back in the prompt", Duration::from_secs(5)), "{:?}", t.text());
    assert!(wait_after(&t, at, "wait-for-it", Duration::from_secs(5)), "{:?}", t.text());
    assert!(t.wait(Duration::from_millis(500)).is_none(), "Ctrl-C with a prompt waiting quits nothing");
    // Quitting while the route is still being asked does not wait-for-it.
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
