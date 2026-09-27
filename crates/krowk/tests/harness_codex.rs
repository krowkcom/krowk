//! `krowk providers add codex` and `krowk -p` on a Codex instance, the
//! built binary against a fake `codex` on PATH (a script replaying recorded
//! JSON-RPC; no real login anywhere): two accounts signed in through
//! `codex login`, each with its own home sharing the person's Codex
//! configuration, each running a session; a tool-using turn that lands in
//! the log and in `krowk sessions`; the native `openai` key kept out of
//! Codex; Ctrl-C mid-turn and a resume; a login that fails; and the trust
//! prompt's headless refusal.

#![cfg(all(feature = "harness", unix))]

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[path = "common/pty.rs"]
mod pty;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/codex").join(name)
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-codex-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/.codex")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n").unwrap();
        // The person's own Codex, which a new account shares.
        std::fs::write(root.join("home/.codex/config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        std::fs::write(root.join("home/.codex/AGENTS.md"), "Be brief.\n").unwrap();
        let bin = root.join("bin/codex");
        std::fs::copy(fixture("fake-codex"), &bin).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            // The fake comes first on PATH: it is the `codex` krowk finds.
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("FAKE_CODEX_LOG", self.root.join("fake.log"))
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
fn r_inst_2_two_codex_accounts_sign_in_through_codex_login_and_each_runs_a_session() {
    let b = Sandbox::new("instances");
    for (name, email) in [("team", "team@example.com"), ("personal", "me@example.com")] {
        let added = b.json(&["providers", "add", "codex", "--name", name, "--json"], &[("FAKE_CODEX_EMAIL", email)]);
        let dir = b.data().join("codex").join(format!("codex-{name}"));
        assert_eq!(added["data"]["instance"], format!("codex:{name}"));
        assert_eq!(added["data"]["kind"], "codex-app-server");
        assert_eq!(added["data"]["definition"]["codexHome"], dir.display().to_string());
        assert_eq!((added["data"]["signed_in"].as_bool(), added["data"]["login"].as_str()), (Some(true), Some("signed in with ChatGPT")));
        assert_eq!(added["data"]["shared"], serde_json::json!(["config.toml", "AGENTS.md"]), "the person's Codex configuration, shared");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_link(dir.join("config.toml")).unwrap(), b.root.join("home/.codex/config.toml"));
    }
    // Codex's own login ran once per account, each with its own CODEX_HOME,
    // after app-server's `account/read` said there was none.
    let fake = b.fake_log();
    let lines: Vec<&str> = fake.lines().collect();
    for name in ["team", "personal"] {
        let dir = b.data().join("codex").join(format!("codex-{name}"));
        let at = lines.iter().position(|l| *l == format!("home {}", dir.display())).expect("ran with the instance's CODEX_HOME");
        assert_eq!(lines[at - 1], "argv app-server --listen stdio://");
    }
    assert_eq!(fake.lines().filter(|l| *l == "argv login").count(), 2);

    // Adding it again finds the login and does not ask again.
    b.json(&["providers", "add", "codex", "--name", "team", "--json"], &[]);
    assert_eq!(b.fake_log().lines().filter(|l| *l == "argv login").count(), 2);

    // Status comes from `account/read`, per instance.
    let before = b.fake_log().lines().count();
    let listed = b.json(&["providers", "list", "--json"], &[]);
    let rows = listed["data"]["instances"].as_array().unwrap();
    let row = |n: &str| rows.iter().find(|r| r["instance"] == n).unwrap_or_else(|| panic!("{n} not listed: {rows:?}")).clone();
    for n in ["codex:team", "codex:personal"] {
        let r = row(n);
        let home = b.data().join("codex").join(n.replace(':', "-"));
        assert_eq!((r["state"].as_str(), r["source"].as_str(), r["wire_api"].as_str()), (Some("ready"), Some(format!("Codex's own login in {} (signed in with ChatGPT)", home.display()).as_str()), Some("codex-app-server")), "{r}");
        assert_eq!(r["binary"], b.root.join("bin/codex").display().to_string());
    }
    assert_eq!(row("codex")["state"], "not_signed_in", "the implicit instance is the person's own Codex, not signed in here");
    // Asked structured, of app-server's account/read — `codex login status`'s
    // words are only the fallback.
    let listing: Vec<String> = b.fake_log().lines().skip(before).map(String::from).collect();
    assert_eq!(listing.iter().filter(|l| l.starts_with("in ") && l.contains(r#""method":"account/read""#)).count(), 3, "{listing:?}");
    assert!(!listing.iter().any(|l| l == "argv login status"), "{listing:?}");

    // A session on each account, with the native openai instance's key in
    // krowk's environment: it never reaches Codex.
    let listed_up_to = b.fake_log().lines().count();
    let mut sessions = Vec::new();
    for name in ["team", "personal"] {
        let model = format!("codex:{name}/gpt-5.5");
        let out = b.json(&["-p", "what is here?", "--model", &model, "--trust", "--output-format", "json"], &[("FAKE_CODEX_SCENARIO", &scenario("tool_use.jsonl")), ("OPENAI_API_KEY", "sk-native-openai")]);
        assert_eq!((out["status"].as_str(), out["result"].as_str(), out["model"]["instance"].as_str()), (Some("completed"), Some("There is one file, README.md."), Some(format!("codex:{name}").as_str())), "{out}");
        let session = out["sessionId"].as_str().unwrap().to_string();
        let events = b.events(&session);
        let backend = events.iter().find(|e| e["type"] == "backend.session").unwrap();
        let home = b.data().join("codex").join(format!("codex-{name}"));
        assert!(backend["transcriptPath"].as_str().unwrap().starts_with(&home.display().to_string()), "{backend}");
        assert_eq!(backend["billing"], "subscription");
        assert!(events.iter().any(|e| e["type"] == "item.completed" && e["item"]["name"] == "mcp__krowk__session_info"), "krowk's tool ran");
        sessions.push(session);
    }
    let fake = b.fake_log();
    assert!(!fake.contains("sk-native-openai"), "the native instance's key reached Codex");
    assert!(fake.contains("env OPENAI_API_KEY=<unset>"));
    // Each session's account is asked twice — by the readiness check before
    // the session exists, and by the backend before its thread — and
    // always of that account's own home.
    let accounts: Vec<&str> = fake.lines().skip(listed_up_to).filter(|l| l.starts_with("out ") && l.contains(r#""account":{"#)).collect();
    assert_eq!(accounts.len(), 4, "{accounts:?}");
    assert!(accounts[..2].iter().all(|a| a.contains("team@example.com")) && accounts[2..].iter().all(|a| a.contains("me@example.com")), "each session ran under its own account: {accounts:?}");

    // krowk.db lists both.
    let listed = b.json(&["sessions", "--json"], &[]);
    let ids: Vec<String> = listed["data"]["sessions"].as_array().unwrap().iter().filter_map(|s| s["foreign_session_id"].as_str().map(String::from)).collect();
    for s in &sessions {
        assert!(ids.contains(s), "{s} not in {ids:?}");
    }

    // Removing an account leaves Codex's login to Codex.
    let removed = b.json(&["providers", "remove", "codex:personal", "--json"], &[]);
    assert_eq!(removed["data"]["config_dir_kept"], b.data().join("codex/codex-personal").display().to_string());
    assert!(b.data().join("codex/codex-personal/fake-login").exists());
}

#[test]
fn r_inst_2_a_codex_login_that_fails_adds_nothing() {
    let b = Sandbox::new("login-fails");
    let out = b.krowk(&["providers", "add", "codex", "--name", "team", "--json"], &[("FAKE_CODEX_LOGIN", "fail")]);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("not_authenticated") && said.contains("krowk providers add codex --name team"), "{said}");
    assert!(!b.data().join("codex/codex-team").exists(), "the home it made is gone");
    let listed = b.json(&["providers", "list", "--json"], &[]);
    assert!(!listed["data"]["instances"].as_array().unwrap().iter().any(|r| r["instance"] == "codex:team"), "no definition was written");
    // A router is signed in by its key: no login is run for it.
    let added = b.json(&["providers", "add", "codex", "--name", "router", "--api-key-env", "ROUTER_KEY", "--json"], &[]);
    assert_eq!(added["data"]["definition"]["apiKeyEnv"], "ROUTER_KEY");
    assert_eq!(b.fake_log().lines().filter(|l| *l == "argv login").count(), 1, "only the failed one");
}

#[test]
fn r_back_6_a_codex_turn_is_not_started_headless_in_an_untrusted_repository() {
    let b = Sandbox::new("trust");
    b.json(&["providers", "add", "codex", "--name", "team", "--json"], &[]);
    let added = b.fake_log().len();
    let out = b.krowk(&["-p", "hi", "--model", "codex:team/gpt-5.5", "--output-format", "json"], &[]);
    assert_eq!(out.status.code(), Some(4), "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("untrusted_directory") && said.contains("--trust"), "{said}");
    assert!(!b.fake_log()[added..].contains("argv "), "nothing was spawned, not even a status check");
    assert!(!b.data().join("sessions").exists() || std::fs::read_dir(b.data().join("sessions")).unwrap().next().is_none(), "no session was left behind");
}

#[test]
fn r_back_3_ctrl_c_interrupts_a_codex_turn_and_the_session_resumes() {
    let b = Sandbox::new("interrupt");
    b.json(&["providers", "add", "codex", "--name", "team", "--json"], &[]);
    let mut child = b
        .command(&["-p", "count for a long time", "--model", "codex:team/gpt-5.5", "--trust", "--output-format", "stream-json"], &[("FAKE_CODEX_SCENARIO", &scenario("interrupt.jsonl"))])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut session = String::new();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    for line in lines.by_ref() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if let Some(s) = v["sessionId"].as_str() {
            session = s.to_string();
        }
        if v["type"] == "item.delta" {
            // SAFETY: a signal to the child this test started.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
            break;
        }
    }
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    let status = child.wait().unwrap();
    assert!(!status.success(), "an interrupted turn is not a success");
    let result: Value = serde_json::from_str(rest.iter().rev().find(|l| l.contains(r#""type":"result""#)).expect("a result line")).unwrap();
    assert_eq!(result["status"], "interrupted");
    assert!(b.fake_log().lines().any(|l| l == "interrupt"), "turn/interrupt reached Codex");
    let events = b.events(&session);
    assert!(events.iter().any(|e| e["type"] == "item.completed" && e["item"]["text"] == "1\n2\n3\n"), "what arrived is kept");
    // A new krowk resumes the thread and goes on.
    let out = b.json(&["-p", "go on", "--resume", &session, "--trust", "--output-format", "json"], &[("FAKE_CODEX_SCENARIO", &scenario("interrupt.jsonl"))]);
    assert_eq!(out["result"], "Picking up where we left off.");
    assert!(b.fake_log().lines().any(|l| l.starts_with("resume ")));
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Whether `pid` is gone within a couple of seconds.
fn gone(pid: i32) -> bool {
    (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        !alive(pid)
    })
}

fn logged_pid(log: &str, prefix: &str) -> i32 {
    log.lines().rev().find_map(|l| l.strip_prefix(prefix)).unwrap_or_else(|| panic!("no {prefix:?} in {log}")).trim().parse().unwrap()
}

/// A second Ctrl-C leaves at once — and takes the backend's whole process
/// group with it, the app-server and what it started, headless or not.
#[test]
fn r_back_3_a_second_ctrl_c_leaves_at_once_and_kills_codexs_process_group() {
    let b = Sandbox::new("ctrlc2");
    b.json(&["providers", "add", "codex", "--name", "team", "--json"], &[]);
    let mut child = b
        .command(&["-p", "work for a long time", "--model", "codex:team/gpt-5.5", "--trust", "--output-format", "stream-json"], &[("FAKE_CODEX_SCENARIO", &scenario("hang.jsonl"))])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    for line in lines.by_ref() {
        if line.unwrap().contains(r#""type":"item.delta""#) {
            break;
        }
    }
    // The grandchild is started right after the delta.
    let log = (0..100).find_map(|_| {
        let l = b.fake_log();
        if l.contains("grandchild ") { Some(l) } else { std::thread::sleep(std::time::Duration::from_millis(20)); None }
    }).expect("the fake started its command");
    let (server, grandchild) = (logged_pid(&log, "pid "), logged_pid(&log, "grandchild "));
    for _ in 0..2 {
        // SAFETY: a signal to the child this test started.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGINT);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let started = std::time::Instant::now();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130), "{status}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "it waited {:?}", started.elapsed());
    assert!(gone(server), "codex app-server ({server}) outlived krowk");
    assert!(gone(grandchild), "what Codex started ({grandchild}) outlived krowk");
}

/// The same from the TUI: a second Ctrl-C during a Codex turn does not wait
/// on the turn's backend, and the process group goes with it.
#[test]
fn r_back_3_a_second_ctrl_c_in_the_tui_during_a_codex_turn_leaves_at_once() {
    let b = Sandbox::new("tui-ctrlc2");
    b.json(&["providers", "add", "codex", "--name", "team", "--json"], &[]);
    let trusted = b.root.join("home/.config/krowk/trusted.json");
    std::fs::create_dir_all(trusted.parent().unwrap()).unwrap();
    std::fs::write(&trusted, serde_json::json!({"directories": [b.root.join("repo")]}).to_string()).unwrap();
    let cmd = b.command(&["--model", "codex:team/gpt-5.5"], &[("TERM", "xterm-256color"), ("FAKE_CODEX_SCENARIO", &scenario("hang.jsonl"))]);
    let mut t = pty::Pty::spawn(cmd, 200, 30);
    assert!(t.wait_for("anything", std::time::Duration::from_secs(10)).is_some(), "{:?}", t.text());
    t.write(b"work for a long time\r");
    assert!(t.wait_for("Responding", std::time::Duration::from_secs(10)).is_some(), "the answer streams: {:?}", t.text());
    let log = (0..100).find_map(|_| {
        let l = b.fake_log();
        if l.contains("grandchild ") { Some(l) } else { std::thread::sleep(std::time::Duration::from_millis(20)); None }
    }).expect("the fake started its command");
    let (server, grandchild) = (logged_pid(&log, "pid "), logged_pid(&log, "grandchild "));
    t.write(b"\x03");
    std::thread::sleep(std::time::Duration::from_millis(300));
    t.write(b"\x03");
    let st = t.wait(std::time::Duration::from_secs(5)).expect("krowk leaves on the second Ctrl-C, not after the backend");
    assert_eq!(st.code(), Some(130), "{st}");
    assert!(gone(server) && gone(grandchild), "the backend's group outlived the TUI");
}
