//! `gruntas -p`, the built binary, against the fake `claude` (Claude Code) on
//! PATH: its flags, the record it leaves on a terminal (R-PRES-6), images
//! sent with the prompt, rollover across two accounts, and bare `gruntas` on
//! a terminal.

#![cfg(feature = "harness")]

#[cfg(unix)]
#[path = "../../gruntas-harness/tests/common/fake.rs"]
mod fake;
#[cfg(unix)]
#[path = "common/pty.rs"]
mod pty;

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// A home, a repository, and the fake `claude` and `codex` first on
    /// PATH, signed in to nothing: a bare model is routed, which asks every
    /// vendor there is, and never the real ones the machine may have.
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("gruntas-harness-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# gruntas\n\nPermalinks for agent output.\n").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for (dir, bin) in [("claude", "fake-claude"), ("codex", "fake-codex")] {
            let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures").join(dir).join(bin);
            // Linked, not copied: a copy is a file open for writing that a
            // test forking beside it can inherit ("Text file busy").
            #[cfg(unix)]
            std::os::unix::fs::symlink(from, root.join("bin").join(dir)).unwrap();
            #[cfg(not(unix))]
            std::fs::copy(from, root.join("bin").join(dir)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        Sandbox { root }
    }

    /// Signs the default Claude Code account (`~/.claude`) in, so `-p` runs
    /// on it, playing `scenario` (a plain `ok` a turn without one), in the
    /// repository trusted for good.
    #[cfg(unix)]
    fn claude(&self, scenario: Option<&str>) {
        std::fs::create_dir_all(self.root.join("home/.claude")).unwrap();
        std::fs::write(self.root.join("home/.claude/fake-login"), "").unwrap();
        std::fs::create_dir_all(self.root.join("home/.gruntas")).unwrap();
        std::fs::write(self.root.join("home/.gruntas/trusted.json"), serde_json::json!({"directories": [self.root.join("repo")]}).to_string()).unwrap();
        if let Some(s) = scenario {
            std::fs::write(self.root.join("scenario.jsonl"), s).unwrap();
        }
    }

    /// PATH with the fakes first.
    fn path(&self) -> String {
        format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default())
    }

    fn gruntas(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args)
            .env_clear()
            .env("PATH", self.path())
            .env("HOME", self.root.join("home"))
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        let scenario = self.root.join("scenario.jsonl");
        if scenario.exists() {
            c.env("FAKE_CLAUDE_SCENARIO", scenario);
        }
        c
    }

    #[cfg(unix)]
    fn ok(&self, args: &[&str]) -> String {
        let out = self.gruntas(args);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "gruntas {args:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        stdout
    }

    /// What the fake logged: every argv, and each line in and out.
    #[cfg(unix)]
    fn fake_log(&self) -> String {
        std::fs::read_to_string(self.root.join("fake.log")).unwrap_or_default()
    }

    /// The prompts Claude Code was sent: each `user` line gruntas wrote it.
    #[cfg(unix)]
    fn prompts(&self) -> Vec<Value> {
        self.fake_log().lines().filter_map(|l| l.strip_prefix("in ")).filter(|l| l.starts_with(r#"{"type":"user""#)).map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn p_flags_are_refused_elsewhere_and_bad_values_are_named() {
    let b = Sandbox::new("flags");
    let out = b.gruntas(&["sessions", "--model", "x"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("only a flag of `gruntas -p`"));
    let out = b.gruntas(&["-p", "hi", "--permission-mode", "yolo"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("bypassPermissions"));
    let out = b.gruntas(&["-p", "hi", "--output-format", "xml"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("stream-json"));
    let out = b.gruntas(&["-p"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a prompt"));
    // No account: refused before a session exists, exit 3, with D4's line.
    let out = b.gruntas(&["-p", "hi"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("connect a Claude Code or Codex account"));
    assert!(!b.root.join("home/.gruntas/sessions").exists(), "a refused prompt leaves no session behind");
}

// ---- R-PRES-6: the record `gruntas -p` leaves on the terminal --------------

/// What the fake model answers a summary with.
#[cfg(unix)]
const SUMMARY: &str = "gruntas turns agent output — screenshots, logs, diffs — into permalinks you can paste anywhere.";

/// The OSC 7501 reports in `out`, whole, in the order written.
#[cfg(unix)]
fn reports(out: &str) -> Vec<&str> {
    out.match_indices("\x1b]7501;").map(|(at, _)| &out[at..at + out[at..].find("\x1b\\").map_or(out.len() - at, |e| e + 2)]).collect()
}

#[cfg(unix)]
fn root(state: gruntas::osc7501::State, msg: &str) -> String {
    gruntas::osc7501::encode(&gruntas::osc7501::Report { state, msg, ..Default::default() }).unwrap()
}

/// `gruntas -p` with its stderr on a terminal and its stdout piped: the
/// reports the terminal got, stdout, and the exit.
#[cfg(unix)]
fn on_a_terminal(b: &Sandbox, args: &[&str]) -> (String, Vec<u8>, std::process::ExitStatus) {
    let mut c = b.command(args);
    c.stdout(Stdio::piped());
    let mut t = pty::Pty::spawn_stderr(c, 80, 24);
    // Read on a thread of its own, so a gruntas that never exits fails the
    // wait below rather than hanging the test.
    let mut pipe = t.child.stdout.take().unwrap();
    let stdout = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = std::io::Read::read_to_end(&mut pipe, &mut out);
        out
    });
    let st = t.wait(std::time::Duration::from_secs(30)).expect("gruntas -p exits");
    (t.text(), stdout.join().unwrap(), st)
}

/// R-PRES-6: on a terminal, `gruntas -p` reports working with the prompt's
/// first line (a blank one before it skipped), then done once it answers; a failed turn ends in error with
/// the failure's first line. The record is the root's, and nothing reaches
/// stdout but the answer.
#[cfg(unix)]
#[test]
fn r_pres_6_krowk_p_on_a_terminal_reports_working_then_done_or_error() {
    use gruntas::osc7501::State;
    let prompt = "\nread README.md and summarise it in one line\nand nothing else";
    let b = Sandbox::new("pres6");
    b.claude(Some(&fake::answer(SUMMARY)));
    let (err, stdout, st) = on_a_terminal(&b, &["-p", prompt, "--model", "claude/sonnet"]);
    assert!(st.success(), "{err}");
    let topic = "read README.md and summarise it in one line";
    assert_eq!(reports(&err), [root(State::Working, topic), root(State::Done, topic)], "{err:?}");
    assert_eq!(String::from_utf8_lossy(&stdout), format!("{SUMMARY}\n"));

    // Claude Code ends the turn in an error whose message breaks a line.
    let b = Sandbox::new("pres6err");
    b.claude(Some(r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"prompt is too long\nby a mile","session_id":"{{SESSION}}"}"#));
    let (err, stdout, st) = on_a_terminal(&b, &["-p", "hi", "--model", "claude/sonnet"]);
    assert!(!st.success(), "{err}");
    // The failure's first line: its headline, what gruntas prints first for
    // it on a terminal, cut where Claude Code's message broke the line.
    let failed = "Claude Code could not finish the turn: prompt is too long";
    assert_eq!(reports(&err), [root(State::Working, "hi"), root(State::Error, failed)], "{err:?}");
    assert!(stdout.is_empty(), "{stdout:?}");
}

/// R-PRES-6: with stderr in a file nothing is reported, and a terminal on
/// stderr changes nothing on stdout, in any format: text byte for byte,
/// JSON but for what differs between any two runs (ids, times).
#[cfg(unix)]
#[test]
fn r_pres_6_stderr_in_a_file_gets_no_report_and_stdout_is_the_same_either_way() {
    let b = Sandbox::new("pres6file");
    b.claude(Some(&fake::answer(SUMMARY)));
    for format in ["text", "json", "stream-json"] {
        let args = ["-p", "read README.md and summarise it in one line", "--model", "claude/sonnet", "--output-format", format];
        let file = b.root.join(format!("stderr-{format}"));
        let out = b.command(&args).stderr(std::fs::File::create(&file).unwrap()).output().unwrap();
        assert!(out.status.success(), "{format}");
        let err = std::fs::read(&file).unwrap();
        assert!(!err.windows(6).any(|w| w == b"\x1b]7501"), "{format}: {}", String::from_utf8_lossy(&err));
        let (on, stdout, st) = on_a_terminal(&b, &args);
        assert!(st.success() && reports(&on).len() == 2, "{format}: {on:?}");
        assert!(!stdout.contains(&0x1b), "{format}: nothing but the answer on stdout");
        assert!(!stdout.is_empty(), "{format}: the answer is on stdout");
        if format == "text" {
            assert_eq!(stdout, out.stdout, "text: byte for byte");
        } else {
            assert_eq!(steady(&String::from_utf8_lossy(&stdout)), steady(&String::from_utf8_lossy(&out.stdout)), "{format}");
        }
    }
}

/// JSON lines with what no two runs share masked.
#[cfg(unix)]
fn steady(out: &str) -> String {
    fn mask(v: &mut Value) {
        match v {
            Value::Object(o) => {
                for (k, v) in o.iter_mut() {
                    if k.ends_with("Ms") || k.ends_with("At") || k == "ts" {
                        *v = Value::Null;
                    }
                    mask(v);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(mask),
            // A UUID: a session's, a turn's, an item's.
            Value::String(s) if s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4 => *s = "id".into(),
            _ => {}
        }
    }
    out.lines()
        .map(|l| match serde_json::from_str::<Value>(l) {
            Ok(mut v) => {
                mask(&mut v);
                v.to_string()
            }
            Err(_) => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// R-PRES-6: Ctrl-C on the terminal during the turn leaves idle, not
/// error: the person stopped it. The interrupt reaches a turn waiting on
/// the model, with nothing arriving (Claude Code takes it over its control
/// protocol), and the way back in is named.
#[cfg(unix)]
#[test]
fn r_pres_6_ctrl_c_during_the_turn_reports_idle_before_exit() {
    use gruntas::osc7501::State;
    // A model that has begun its answer and holds the rest: the turn is
    // running once the prompt is seen.
    let b = Sandbox::new("pres6int");
    b.claude(Some(&std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/claude/interrupt.jsonl")).unwrap()));
    let mut c = b.command(&["-p", "wait forever", "--model", "claude/sonnet"]);
    c.stdout(Stdio::piped());
    let mut t = pty::Pty::spawn_stderr(c, 80, 24);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !b.fake_log().contains("1\\n2\\n3\\n") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(b.fake_log().contains("1\\n2\\n3\\n"), "the turn never began: {:?}", t.text());
    t.write(b"\x03");
    let st = t.wait(std::time::Duration::from_secs(10)).expect("gruntas -p exits on Ctrl-C");
    let err = t.text();
    assert!(!st.success() && err.contains("interrupted") && err.contains("--resume"), "{st}: {err:?}");
    assert!(b.fake_log().contains("\ninterrupt\n"), "the interrupt went to Claude Code");
    assert_eq!(reports(&err), [root(State::Working, "wait forever"), root(State::Idle, "wait forever")], "{err:?}");
}

/// `gruntas -p --image a --image b "…"` gives Claude Code both images, named
/// `[Image #1]` and `[Image #2]` in the prompt; a big one is shrunk to fit;
/// a missing or non-image file is refused naming it, before any turn.
#[cfg(unix)]
#[test]
fn krowk_p_image_sends_each_file_with_the_prompt_and_refuses_a_bad_one_first() {
    let b = Sandbox::new("images");
    b.claude(None);
    let png = |w: u32, h: u32, path: &std::path::Path| {
        // A PNG written without the image crate: 8-bit grey, stored deflate blocks.
        let mut raw = Vec::new();
        for y in 0..h {
            raw.push(0u8);
            raw.extend((0..w).map(|x| ((x ^ y) % 251) as u8));
        }
        let crc = |bytes: &[u8]| {
            let mut c = !0u32;
            for &x in bytes {
                c ^= x as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        };
        let adler = {
            let (mut a, mut s) = (1u32, 0u32);
            for &x in &raw {
                a = (a + x as u32) % 65521;
                s = (s + a) % 65521;
            }
            (s << 16) | a
        };
        let mut z = vec![0x78, 0x01];
        let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (k, c) in chunks.iter().enumerate() {
            z.push((k + 1 == chunks.len()) as u8);
            z.extend((c.len() as u16).to_le_bytes());
            z.extend((!(c.len() as u16)).to_le_bytes());
            z.extend(*c);
        }
        z.extend(adler.to_be_bytes());
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in [(&b"IHDR"[..], [&w.to_be_bytes()[..], &h.to_be_bytes(), &[8, 0, 0, 0, 0]].concat()), (&b"IDAT"[..], z), (&b"IEND"[..], Vec::new())] {
            out.extend((data.len() as u32).to_be_bytes());
            let body = [kind, &data].concat();
            out.extend(&body);
            out.extend(crc(&body).to_be_bytes());
        }
        std::fs::write(path, out).unwrap();
    };
    let (a, big, notes) = (b.root.join("a.png"), b.root.join("big.png"), b.root.join("notes.txt"));
    png(8, 8, &a);
    png(3000, 100, &big);
    std::fs::write(&notes, "hello").unwrap();
    let (a, big, notes) = (a.to_str().unwrap(), big.to_str().unwrap(), notes.to_str().unwrap());

    for bad in [notes, "/nonexistent/x.png"] {
        let out = b.gruntas(&["-p", "look", "--image", a, "--image", bad, "--model", "claude/sonnet"]);
        assert_eq!(out.status.code(), Some(1), "{bad}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(bad), "names the file: {}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(!b.fake_log().contains("argv -p"), "no turn started");

    b.ok(&["-p", "compare these", "--image", a, "--image", big, "--model", "claude/sonnet"]);
    let sent = b.prompts();
    assert_eq!(sent.len(), 1);
    let content = sent[0]["message"]["content"].as_array().unwrap().clone();
    let images: Vec<&Value> = content.iter().filter(|c| c["type"] == "image").collect();
    assert_eq!(images.len(), 2, "{content:?}");
    let text: String = content.iter().filter_map(|c| c["text"].as_str()).collect();
    assert!(text.contains("[Image #1]") && text.contains("[Image #2]") && text.contains("compare these"), "{text}");
    // Shrunk to 2000 px a side: far fewer bytes than the 3000 px original's.
    assert!(images[1]["source"]["data"].as_str().unwrap().len() < std::fs::metadata(big).unwrap().len() as usize * 4 / 3);
}

/// Two Claude Code accounts, each signed in in a directory of its own:
/// `claude:a` replaying `scenario`, `claude:b` answering, and `rollover`
/// when given — what a limit on `claude:a` may move on to.
fn two_claude_accounts(b: &Sandbox, scenario: &str, rollover: Option<&str>) {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/claude");
    let account = |name: &str, scenario: &str| {
        let dir = b.root.join("home/.gruntas/accounts").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fake-login"), "").unwrap();
        serde_json::json!({"kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": dir, "env": {"FAKE_CLAUDE_SCENARIO": fixtures.join(scenario)}})
    };
    let mut cfg = serde_json::json!({"instances": {"claude:a": account("claude-a", scenario), "claude:b": account("claude-b", "switch.jsonl")}});
    if let Some(r) = rollover {
        cfg["rollover"] = r.into();
    }
    std::fs::write(b.root.join("home/.gruntas/config.json"), cfg.to_string()).unwrap();
}

/// Saves `instance`'s five-hour window with `used` percent used, as seen
/// now, under a login of its own name, listing `haiku`.
fn seed_usage(home: &std::path::Path, instance: &str, used: f64) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let usage = gruntas_harness::usage::Usage::in_home(&home.join(".gruntas"));
    usage.record_instance(instance, instance, &["haiku".to_string()], now).unwrap();
    let limit = gruntas_harness::protocol::LimitStatus { status: gruntas_harness::protocol::LimitState::Allowed, window: Some("five_hour".into()), used_percent: Some(used), resets_at_ms: Some(now + 3_600_000) };
    usage.record_limit(instance, &limit, now).unwrap();
}

/// R-ROLL-11, D16: `gruntas -p` with `offer` ends a turn that left too little
/// by offering the next account in its JSON result, marked early, and
/// says how to continue there on stderr; the turn itself succeeded.
#[cfg(unix)]
#[test]
fn r_roll_11_krowk_p_shows_an_early_offer_in_json_and_hints_on_stderr() {
    let b = Sandbox::new("early");
    std::fs::create_dir_all(b.root.join("home/.gruntas")).unwrap();
    two_claude_accounts(&b, "low.jsonl", None);
    seed_usage(&b.root.join("home"), "claude:a", 90.0);
    seed_usage(&b.root.join("home"), "claude:b", 20.0);
    let out = b.gruntas(&["-p", "hi", "--model", "claude:a/haiku", "--trust", "--output-format", "json"]);
    let (stdout, err) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
    assert!(out.status.success(), "{stdout}{err}");
    let v: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}{err}"));
    assert_eq!((v["switchOffer"]["early"].as_bool(), v["switchOffer"]["to"]["instance"].as_str()), (Some(true), Some("claude:b")), "{stdout}");
    let id = v["sessionId"].as_str().unwrap();
    assert!(err.contains("continue on claude:b/haiku (80% left as of ") && err.contains(&format!("with `gruntas -p --resume {id} --model claude:b/haiku`")), "{err}");
    // Off: no offer, no hint.
    let out = b.gruntas(&["-p", "hi", "--model", "claude:a/haiku", "--trust", "--output-format", "json", "--rollover", "off"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
    assert!(v["switchOffer"].is_null() && !String::from_utf8_lossy(&out.stderr).contains("continue on"), "{v}");
}

/// R-ROLL-4..7: `rollover` is read from the global config, a trusted
/// repository's `.gruntas/config.json` and `--rollover`, most specific first;
/// `off` fails a limit with no offer; a resumed session keeps its value; any
/// other value is refused naming the three.
#[cfg(unix)]
#[test]
fn r_roll_5_krowk_p_resolves_rollover_per_session_repo_and_global() {
    let b = Sandbox::new("rollover");
    std::fs::create_dir_all(b.root.join("home/.gruntas")).unwrap();
    let global = |rollover: Option<&str>| two_claude_accounts(&b, "limited.jsonl", rollover);
    let repo = |rollover: &str| {
        std::fs::create_dir_all(b.root.join("repo/.gruntas")).unwrap();
        std::fs::write(b.root.join("repo/.gruntas/config.json"), serde_json::json!({"rollover": rollover}).to_string()).unwrap();
    };
    // Whether the limited turn was offered a way on, and its session. The
    // repository is trusted only when `--trust` is given.
    let offered = |extra: &[&str]| -> (bool, String) {
        let out = b.gruntas(&[&["-p", "hi", "--model", "claude:a/haiku", "--output-format", "json"], extra].concat());
        let (stdout, err) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        let v: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}{err}"));
        assert_eq!(v["error"]["code"].as_str(), Some("rate_limited"), "{stdout}");
        let offer = !v["switchOffer"].is_null();
        assert_eq!(offer, v["error"]["message"].as_str().unwrap_or_default().contains("continue on"), "the hint and the offer go together: {stdout}");
        (offer, v["sessionId"].as_str().unwrap_or_default().to_string())
    };

    global(None);
    assert!(offered(&["--trust"]).0, "offer is the default");
    global(Some("off"));
    assert!(!offered(&["--trust"]).0, "the global value");
    repo("offer");
    assert!(offered(&["--trust"]).0, "a trusted repository overrides the global value");
    assert!(!offered(&["--trust", "--rollover", "off"]).0, "--rollover overrides both");
    std::fs::remove_dir_all(b.root.join("repo/.gruntas")).unwrap();
    assert!(offered(&["--trust", "--rollover", "offer"]).0, "--rollover overrides the global value");

    // Resumed: the recorded value stands until a new one replaces it.
    let (_, id) = offered(&["--rollover", "off", "--trust"]);
    global(None);
    let resumed = |extra: &[&str]| offered(&[&["--resume", &id, "--trust"], extra].concat()).0;
    assert!(!resumed(&[]), "--resume keeps the recorded value");
    assert!(resumed(&["--rollover", "offer"]));
    assert!(resumed(&[]), "and the new one is recorded");

    let out = b.gruntas(&["-p", "hi", "--rollover", "sometimes"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("bad_flag") && err.contains("auto, offer, off"), "{err}");
    global(Some("sometimes"));
    let out = b.gruntas(&["-p", "hi"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("bad_config") && err.contains("auto, offer, off"), "{err}");
}

/// R-CUT-8, D9: bare `gruntas` on a terminal prints the overview it prints without
/// one; a flag that once opened an interactive client, given with no command and no `-p`,
/// is refused with the way to `-p`, and `--resume` with no value is refused
/// whatever else is given; the commands that own those flags still take them.
#[cfg(unix)]
#[test]
fn r_cut_8_bare_krowk_on_a_terminal_prints_the_overview_and_the_old_tui_flags_point_to_p() {
    let b = Sandbox::new("bare-tty");
    // A Claude Code account on an API key, ready here without a login, so
    // bare `gruntas` has no first-machine setup to ask.
    std::fs::create_dir_all(b.root.join("home/.gruntas")).unwrap();
    std::fs::write(b.root.join("home/.gruntas/config.json"), serde_json::json!({"instances": {"claude:console": {"kind": "claude-code", "apiKeyEnv": "ANTHROPIC_API_KEY"}}}).to_string()).unwrap();
    let on_a_tty = |args: &[&str]| {
        let mut c = b.command(args);
        c.env("NO_COLOR", "1").env("TERM", "xterm-256color").env("ANTHROPIC_API_KEY", "sk-test");
        let mut t = pty::Pty::spawn(c, 120, 40);
        let st = t.wait(std::time::Duration::from_secs(20)).expect("gruntas exits at once");
        (t.text().replace("\r\n", "\n"), st)
    };
    let mut piped = b.command(&[]);
    piped.env("NO_COLOR", "1").env("TERM", "xterm-256color").env("ANTHROPIC_API_KEY", "sk-test");
    let piped = piped.output().unwrap();
    assert!(piped.status.success());
    let (greeting, st) = on_a_tty(&[]);
    assert!(st.success(), "{greeting}");
    assert_eq!(greeting, String::from_utf8_lossy(&piped.stdout), "the same greeting on a terminal as off it");

    for args in [&["--model", "x"][..], &["--permission-mode", "plan"], &["--toolset", "none"], &["--effort", "low"], &["--resume", "abc"], &["--worktree"], &["--max-usd", "1"], &["--max-tokens", "10"]] {
        let (said, st) = on_a_tty(args);
        assert!(!st.success() && said.contains("-p") && said.contains(&format!("--{}", &args[0][2..])), "{args:?}: {said}");
    }
    for with_p in [false, true] {
        let (said, st) = on_a_tty(&[if with_p { &["-p", "hi"][..] } else { &[] }, &["--resume"]].concat());
        assert!(!st.success() && said.contains("--resume needs a value"), "-p {with_p}: {said}");
    }
    // Help is still help, and a refusal in JSON is a JSON refusal.
    assert!(b.gruntas(&["--help", "--model", "x"]).status.success());
    let out = b.gruntas(&["--json", "--model", "x"]);
    let said: Value = serde_json::from_slice(&out.stderr).expect("a JSON refusal");
    assert!(!out.status.success() && said["error"]["error"] == "bad_flag", "{said}");
    // The commands that own the flags are unchanged.
    let out = b.gruntas(&["sessions", "--worktree", "/nowhere", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = b.gruntas(&["sessions", "budget", "--max-usd", "1", "--json"]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("only a flag"), "{}", String::from_utf8_lossy(&out.stderr));
}
