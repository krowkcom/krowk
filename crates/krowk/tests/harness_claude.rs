//! `krowk providers add claude` and `krowk -p` on a Claude Code instance,
//! the built binary against a fake `claude` on PATH (a script replaying
//! recorded stream-json; no real login anywhere): two accounts signed in
//! through Claude's own flow, each running a session; a tool-using turn
//! that lands in the log and in `krowk sessions`; Ctrl-C mid-turn and a
//! resume; and the trust prompt's headless refusal.

#![cfg(all(feature = "harness", unix))]

#[path = "common/pty.rs"]
mod pty;

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude").join(name)
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-claude-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n").unwrap();
        let bin = root.join("bin/claude");
        std::fs::copy(fixture("fake-claude"), &bin).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            // The fake comes first on PATH: it is the `claude` krowk finds.
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    fn krowk(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    fn json(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = self.krowk(args, env);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "krowk {args:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("krowk {args:?}: {e}: {stdout}"))
    }

    fn fake_log(&self) -> String {
        std::fs::read_to_string(self.root.join("fake.log")).unwrap_or_default()
    }

    fn data(&self) -> PathBuf {
        self.root.join("home/.local/share/krowk")
    }

    fn events(&self, session: &str) -> Vec<Value> {
        std::fs::read_to_string(self.data().join("sessions").join(session).join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn scenario(name: &str) -> String {
    fixture(name).display().to_string()
}

#[test]
fn r_inst_2_two_claude_accounts_sign_in_through_claudes_own_flow_and_each_runs_a_session() {
    let b = Sandbox::new("instances");
    for name in ["personal", "work"] {
        let added = b.json(&["providers", "add", "claude", "--name", name, "--json"], &[]);
        let dir = b.data().join("claude").join(format!("claude-{name}"));
        assert_eq!(added["data"]["instance"], format!("claude:{name}"));
        assert_eq!(added["data"]["kind"], "claude-code");
        assert_eq!(added["data"]["definition"]["configDir"], dir.display().to_string());
        assert_eq!((added["data"]["signed_in"].as_bool(), added["data"]["login"].as_str()), (Some(true), Some("signed in with a Claude max subscription")));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    }
    // Claude's own login ran once per account, each in its own directory,
    // after `claude auth status` said there was none.
    let fake = b.fake_log();
    let logins: Vec<&str> = fake.lines().collect();
    for name in ["personal", "work"] {
        let dir = b.data().join("claude").join(format!("claude-{name}"));
        let at = logins.iter().position(|l| *l == format!("config {}", dir.display())).expect("ran with the instance's CLAUDE_CONFIG_DIR");
        assert_eq!(logins[at - 1], "argv auth status --json");
    }
    assert_eq!(fake.lines().filter(|l| *l == "argv auth login").count(), 2);
    assert!(!fake.contains("email"), "nothing personal is kept");

    // Adding it again finds the login and does not ask again.
    b.json(&["providers", "add", "claude", "--name", "work", "--json"], &[]);
    assert_eq!(b.fake_log().lines().filter(|l| *l == "argv auth login").count(), 2);

    // Status comes from `claude auth status`, per instance, through the
    // readiness check `krowk status` prints.
    let listed = b.json(&["providers", "list", "--json"], &[]);
    let rows = listed["data"]["instances"].as_array().unwrap();
    let row = |n: &str| rows.iter().find(|r| r["instance"] == n).unwrap_or_else(|| panic!("{n} in {rows:?}")).clone();
    for n in ["claude:personal", "claude:work"] {
        let dir = b.data().join("claude").join(n.replace(':', "-"));
        assert_eq!((row(n)["state"].as_str(), row(n)["source"].as_str()), (Some("ready"), Some(format!("Claude Code's own login in {} (signed in with a Claude max subscription)", dir.display()).as_str())));
    }
    assert_eq!(row("claude")["state"], "not_signed_in", "the default account, in the sandbox's ~/.claude, has no login");
    assert_eq!(row("claude")["fix"], "sign in with `krowk providers add claude`, which runs Claude's own login");

    // Each account runs a session, in its own config directory.
    for name in ["personal", "work"] {
        let model = format!("claude:{name}/sonnet");
        let r = b.json(&["-p", "hello", "--model", &model, "--trust", "--output-format", "json"], &[]);
        assert_eq!((r["status"].as_str(), r["model"]["instance"].as_str(), r["result"].as_str()), (Some("completed"), Some(format!("claude:{name}").as_str()), Some("ok")));
        assert!(b.data().join("claude").join(format!("claude-{name}")).join("projects").is_dir(), "Claude Code kept the transcript in the account's directory");
    }
    let sessions = b.json(&["sessions", "--json"], &[]);
    assert_eq!(sessions["data"]["sessions"].as_array().unwrap().iter().filter(|s| s["harness"] == "krowk").count(), 2);

    // A login given up on adds nothing and leaves no directory behind.
    let out = b.krowk(&["providers", "add", "claude", "--name", "broken"], &[("FAKE_CLAUDE_LOGIN", "fail")]);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("claude auth login"));
    assert!(!b.data().join("claude/claude-broken").exists());
    let cfg: Value = serde_json::from_str(&std::fs::read_to_string(b.root.join("home/.config/krowk/config.json")).unwrap()).unwrap();
    assert!(cfg["instances"].get("claude:broken").is_none() && cfg["instances"].get("claude:work").is_some());

    // Removing one keeps Claude's own login where Claude keeps it.
    let removed = b.json(&["providers", "remove", "claude:work", "--json"], &[]);
    assert_eq!(removed["data"]["config_dir_kept"], b.data().join("claude/claude-work").display().to_string());
    assert!(b.data().join("claude/claude-work/fake-login").exists());
}

#[test]
fn r_back_1_a_tool_using_claude_turn_is_in_the_log_and_in_krowk_sessions() {
    let b = Sandbox::new("tool");
    b.json(&["providers", "add", "claude", "--json"], &[]);
    // The native instance's key and base URL are exported, as on a machine
    // set up for the API: they are not Claude Code's, and stay out of it.
    let out = b.krowk(
        &["-p", "what session is this?", "--model", "claude/sonnet", "--trust", "--output-format", "stream-json"],
        &[("FAKE_CLAUDE_SCENARIO", &scenario("session_info.jsonl")), ("ANTHROPIC_API_KEY", "sk-ant-api-test"), ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let result = lines.last().unwrap();
    assert_eq!((result["type"].as_str(), result["status"].as_str()), (Some("result"), Some("completed")));
    let session = result["sessionId"].as_str().unwrap();
    // The injected tool ran inside krowk and answered for this session.
    let tool_result = lines.iter().find(|l| l["type"] == "item.completed" && l["item"]["kind"] == "toolResult").unwrap();
    assert!(tool_result["item"]["output"].as_str().unwrap().contains(&format!("krowk session: {session}")), "{tool_result}");
    // R-BACK-5, R-INST-3: the Claude session and its transcript, and what
    // it is billed to, are on the session's record.
    let backend = lines.iter().find(|l| l["type"] == "backend.session").unwrap();
    assert_eq!((backend["backend"].as_str(), backend["billing"].as_str()), (Some("claude-code"), Some("subscription")), "an exported API key does not move the subscription onto it");
    assert!(b.fake_log().contains("env ANTHROPIC_API_KEY= ANTHROPIC_BASE_URL=\n"), "{}", b.fake_log());
    assert!(Path::new(backend["transcriptPath"].as_str().unwrap()).starts_with(b.root.join("home/.claude/projects")));
    assert_eq!(b.events(session).iter().filter(|e| e["type"] == "turn.completed").count(), 1);

    let listed = b.json(&["sessions", "--json"], &[]);
    let ours = listed["data"]["sessions"].as_array().unwrap().iter().find(|s| s["harness"] == "krowk").unwrap().clone();
    assert_eq!(ours["foreign_session_id"], session);
    let shown = b.json(&["sessions", "show", session, "--json"], &[]);
    let text = shown["data"]["messages"].to_string();
    assert!(text.contains("mcp__krowk__session_info") && text.contains("The session_info tool answered."), "{text}");
}

#[test]
fn r_back_1_ctrl_c_interrupts_a_claude_turn_and_the_session_resumes() {
    let b = Sandbox::new("interrupt");
    b.json(&["providers", "add", "claude", "--json"], &[]);
    let mut child = b
        .command(&["-p", "count to a thousand", "--model", "claude/haiku", "--trust", "--output-format", "stream-json"], &[("FAKE_CLAUDE_SCENARIO", &scenario("interrupt.jsonl"))])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut lines = Vec::new();
    loop {
        let mut l = String::new();
        if out.read_line(&mut l).unwrap() == 0 {
            break;
        }
        let v: Value = serde_json::from_str(&l).unwrap();
        let delta = v["type"] == "item.delta";
        lines.push(v);
        if delta && lines.iter().filter(|l| l["type"] == "item.delta").count() == 1 {
            // SAFETY: SIGINT to the child this test spawned.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
        }
    }
    let status = child.wait().unwrap();
    let result = lines.last().unwrap();
    assert_eq!((result["type"].as_str(), result["status"].as_str(), result["result"].as_str()), (Some("result"), Some("interrupted"), Some("1\n2\n3\n")));
    assert_eq!(status.code(), Some(1), "an interrupted turn is not a success");
    assert!(b.fake_log().contains("\ninterrupt\n"), "the interrupt went over the control protocol, not as a signal");
    let session = result["sessionId"].as_str().unwrap().to_string();

    // The session continues: a new krowk, a new claude on --resume.
    let r = b.json(&["-p", "go on", "--resume", &session, "--trust", "--output-format", "json"], &[]);
    assert_eq!((r["status"].as_str(), r["sessionId"].as_str()), (Some("completed"), Some(session.as_str())));
    assert_eq!(r["model"]["instance"], "claude", "a resumed session keeps its instance");
    assert!(b.fake_log().contains("resume fa4e0000-0000-4000-8000-000000000001"));
    let turns: Vec<String> = b.events(&session).iter().filter(|e| e["type"] == "turn.completed").map(|e| e["status"].as_str().unwrap().to_string()).collect();
    assert_eq!(turns, ["interrupted", "completed"]);
}

#[test]
fn r_back_6_headless_refuses_an_untrusted_repository_before_claude_is_spawned() {
    let b = Sandbox::new("trust");
    b.json(&["providers", "add", "claude", "--json"], &[]);
    std::fs::write(b.root.join("repo/.mcp.json"), "{}").unwrap();
    let out = b.krowk(&["-p", "hello", "--model", "claude/sonnet"], &[]);
    assert_eq!(out.status.code(), Some(4), "refused");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("untrusted_directory") && err.contains(".mcp.json") && err.contains("--trust"), "{err}");
    assert!(!b.fake_log().contains("argv -p"), "claude was never started");
    assert!(!b.data().join("sessions").exists() || std::fs::read_dir(b.data().join("sessions")).unwrap().next().is_none(), "no session was created");
    // --trust runs it, for this run only.
    b.json(&["-p", "hello", "--model", "claude/sonnet", "--trust", "--output-format", "json"], &[]);
    assert!(!b.root.join("home/.config/krowk/trusted.json").exists(), "--trust is not remembered");
    // A repository trusted before (a yes at the prompt) needs no flag, and
    // covers its subdirectories.
    std::fs::create_dir_all(b.root.join("home/.config/krowk")).unwrap();
    std::fs::write(b.root.join("home/.config/krowk/trusted.json"), serde_json::json!({"directories": [b.root.join("repo")]}).to_string()).unwrap();
    std::fs::create_dir_all(b.root.join("repo/src")).unwrap();
    let out = b.command(&["-p", "hello", "--model", "claude/sonnet", "--output-format", "json"], &[]).current_dir(b.root.join("repo/src")).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // A home kept in git for its dotfiles: a directory in it resolves to
    // the home, which is never trusted for good — only --trust runs there.
    std::fs::create_dir_all(b.root.join("home/.git")).unwrap();
    std::fs::create_dir_all(b.root.join("home/notes")).unwrap();
    std::fs::write(b.root.join("home/.config/krowk/trusted.json"), serde_json::json!({"directories": [b.root.join("home"), b.root.join("repo")]}).to_string()).unwrap();
    let out = b.command(&["-p", "hello", "--model", "claude/sonnet"], &[]).current_dir(b.root.join("home/notes")).output().unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&out.stderr).contains("home directory"), "{}", String::from_utf8_lossy(&out.stderr));
    let out = b.command(&["-p", "hello", "--model", "claude/sonnet", "--trust"], &[]).current_dir(b.root.join("home/notes")).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // A repository inside a trusted one is asked about on its own.
    std::fs::create_dir_all(b.root.join("repo/vendor/lib/.git")).unwrap();
    let out = b.command(&["-p", "hello", "--model", "claude/sonnet"], &[]).current_dir(b.root.join("repo/vendor/lib")).output().unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", String::from_utf8_lossy(&out.stderr));
    // The native engine runs nothing of the repository's and never asks.
    let out = b.krowk(&["sessions", "--trust"], &[]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("only a flag of `krowk -p`"));
}

#[test]
fn r_inst_1_providers_add_claude_with_a_router_hands_it_the_named_key_and_nothing_ambient() {
    let b = Sandbox::new("router");
    let added = b.json(&["providers", "add", "claude", "--name", "router", "--base-url", "https://router.example/api", "--api-key-env", "ROUTER_KEY", "--json"], &[]);
    assert_eq!(added["data"]["definition"]["apiKeyEnv"], "ROUTER_KEY", "the variable's name is stored");
    assert_eq!(added["data"]["definition"]["env"]["ANTHROPIC_BASE_URL"], "https://router.example/api");
    assert!(!b.fake_log().contains("argv auth login"), "a router signs in with its key, not a Claude login");
    let cfg = std::fs::read_to_string(b.root.join("home/.config/krowk/config.json")).unwrap();
    assert!(!cfg.contains("sk-or-"), "never the key");

    let listed = b.json(&["providers", "list", "--json"], &[("ROUTER_KEY", "sk-or-live")]);
    let row = listed["data"]["instances"].as_array().unwrap().iter().find(|r| r["instance"] == "claude:router").unwrap().clone();
    assert_eq!((row["state"].as_str(), row["source"].as_str()), (Some("ready"), Some("$ROUTER_KEY, handed to Claude Code")));

    // The ambient native key is exported too, and must not reach Claude Code.
    let env = [("ROUTER_KEY", "sk-or-live"), ("ANTHROPIC_API_KEY", "sk-ant-api-ambient"), ("ANTHROPIC_AUTH_TOKEN", "ambient-token")];
    let r = b.json(&["-p", "hello", "--model", "claude:router/anthropic/claude-sonnet-4.5", "--trust", "--output-format", "json"], &env);
    assert_eq!(r["status"], "completed");
    let fake = b.fake_log();
    assert!(fake.contains("env ANTHROPIC_AUTH_TOKEN=sk-or-live\n") && fake.contains("env ANTHROPIC_API_KEY= ANTHROPIC_BASE_URL=https://router.example/api\n"), "{fake}");
    assert!(!fake.contains("ambient"), "{fake}");
    // Without the router's variable, it says which one to set.
    let out = b.krowk(&["-p", "hello", "--model", "claude:router/x", "--trust"], &[]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ROUTER_KEY"));
    // A plain Claude account never sees the ambient key either.
    b.json(&["providers", "add", "claude", "--json"], &[]);
    b.json(&["-p", "hello", "--model", "claude/sonnet", "--trust", "--output-format", "json"], &env);
    let last = b.fake_log().lines().rfind(|l| l.starts_with("env ANTHROPIC_AUTH_TOKEN=")).unwrap().to_string();
    assert_eq!(last, "env ANTHROPIC_AUTH_TOKEN=");
}

/// Bare `krowk` on a terminal, on a Claude Code instance, in a repository
/// nobody trusted: the trust prompt comes before the TUI takes the
/// terminal, a yes is remembered and the session runs on the fake claude;
/// a no opens the TUI anyway, and the first prompt is refused with why,
/// with nothing spawned.
#[test]
fn r_back_6_the_tui_asks_before_claude_runs_in_an_untrusted_repository() {
    for yes in [true, false] {
        let b = Sandbox::new(if yes { "tui-yes" } else { "tui-no" });
        b.json(&["providers", "add", "claude", "--json"], &[]);
        let cmd = b.command(&["--model", "claude/sonnet"], &[("TERM", "xterm-256color")]);
        // Wide enough that a long temporary path does not wrap the question.
        let mut t = pty::Pty::spawn(cmd, 400, 30);
        assert!(t.wait_for("remember", std::time::Duration::from_secs(10)).is_some(), "no trust prompt: {:?}", t.text());
        assert!(t.text().contains("Trust ") && t.text().contains("Claude Code runs a repository's own hooks"), "{:?}", t.text());
        assert!(!b.fake_log().contains("argv -p"), "nothing spawned before the answer");
        t.write(if yes { b"y" } else { b"n" });
        assert!(t.wait_for("anything", std::time::Duration::from_secs(10)).is_some(), "the TUI opens: {:?}", t.text());
        t.write(b"hello\r");
        let trusted = b.root.join("home/.config/krowk/trusted.json");
        if yes {
            assert!(t.wait_for("tokens", std::time::Duration::from_secs(10)).is_some(), "the turn ran on Claude Code: {:?}", t.text());
            assert!(std::fs::read_to_string(&trusted).unwrap().contains(&b.root.join("repo").display().to_string()), "a yes is remembered");
            assert!(b.fake_log().contains("argv -p"));
        } else {
            assert!(t.wait_for("trust prompt", std::time::Duration::from_secs(10)).is_some(), "the refusal says why: {:?}", t.text());
            assert!(!b.fake_log().contains("argv -p"), "claude never ran");
            assert!(!trusted.exists());
        }
        t.write(b"\x04");
        let st = t.wait(std::time::Duration::from_secs(10)).expect("krowk exits on Ctrl-D");
        assert!(st.success(), "{st}");
        if yes {
            assert!(b.fake_log().lines().last() == Some("eof"), "the TUI let Claude Code go cleanly: {}", b.fake_log());
        }
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn gone(pid: i32) -> bool {
    (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        !alive(pid)
    })
}

/// The pids the fake logged once it started its background command.
fn claude_pids(b: &Sandbox) -> (i32, i32) {
    let log = (0..200)
        .find_map(|_| {
            let l = b.fake_log();
            if l.contains("grandchild ") {
                Some(l)
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            }
        })
        .expect("the fake started its command");
    let pid = |prefix: &str| -> i32 { log.lines().rev().find_map(|l| l.strip_prefix(prefix)).unwrap().trim().parse().unwrap() };
    (pid("pid "), pid("grandchild "))
}

/// A second Ctrl-C leaves at once, headless or in the TUI, and takes Claude
/// Code's whole process group with it.
#[test]
fn r_back_1_a_second_ctrl_c_leaves_at_once_and_kills_claude_codes_process_group() {
    let b = Sandbox::new("ctrlc2");
    b.json(&["providers", "add", "claude", "--json"], &[]);
    let mut child = b
        .command(&["-p", "work for a long time", "--model", "claude/sonnet", "--trust", "--output-format", "stream-json"], &[("FAKE_CLAUDE_SCENARIO", &scenario("hang.jsonl"))])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (server, grandchild) = claude_pids(&b);
    for _ in 0..2 {
        // SAFETY: a signal to the child this test started.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGINT);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let started = std::time::Instant::now();
    assert_eq!(child.wait().unwrap().code(), Some(130));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(gone(server) && gone(grandchild), "Claude Code's group outlived krowk -p");

    // The TUI.
    let _ = std::fs::remove_file(b.root.join("fake.log"));
    let trusted = b.root.join("home/.config/krowk/trusted.json");
    std::fs::create_dir_all(trusted.parent().unwrap()).unwrap();
    std::fs::write(&trusted, serde_json::json!({"directories": [b.root.join("repo")]}).to_string()).unwrap();
    let cmd = b.command(&["--model", "claude/sonnet"], &[("TERM", "xterm-256color"), ("FAKE_CLAUDE_SCENARIO", &scenario("hang.jsonl"))]);
    let mut t = pty::Pty::spawn(cmd, 200, 30);
    assert!(t.wait_for("anything", std::time::Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"work for a long time\r");
    let (server, grandchild) = claude_pids(&b);
    t.write(b"\x03");
    std::thread::sleep(std::time::Duration::from_millis(300));
    t.write(b"\x03");
    let st = t.wait(std::time::Duration::from_secs(5)).expect("the TUI leaves on the second Ctrl-C, not after Claude Code");
    assert_eq!(st.code(), Some(130), "{st}");
    assert!(gone(server) && gone(grandchild), "Claude Code's group outlived the TUI");
}
