//! `gruntas run <harness>` (Launcher), the built binary in a stand-in home
//! with fake `claude` and `codex` binaries that write down the arguments and
//! environment they were started with. LN01 lands the command shell: the
//! harness and account read, the account looked up, the mistakes refused.
//! Each later slice un-ignores its own tests below and fills them in.

#![cfg(all(feature = "harness", unix))]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// A home with `claude:work` and `codex:team` accounts on the fake
    /// binaries.
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("gruntas-launcher-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home/.gruntas", "data", "bin", "accounts/claude-work", "accounts/codex-team"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let b = Sandbox { root: root.canonicalize().unwrap() };
        for bin in ["claude", "codex"] {
            b.fake(bin);
        }
        let config = serde_json::json!({ "instances": {
            "claude:work": { "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("accounts/claude-work") },
            "codex:team": { "kind": "codex-app-server", "binary": b.root.join("bin/codex"), "codexHome": b.root.join("accounts/codex-team") },
        }});
        std::fs::write(b.home().join("config.json"), config.to_string()).unwrap();
        b
    }

    fn home(&self) -> PathBuf {
        self.root.join("home/.gruntas")
    }

    /// A stand-in `name` that writes its arguments, one per line, to
    /// `<name>.args` and its environment to `<name>.env`, then exits 0.
    fn fake(&self, name: &str) {
        let path = self.root.join("bin").join(name);
        let out = self.root.join(name);
        let script = format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{0}.args'\nenv > '{0}.env'\n", out.display());
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// What the fake `name` was started with: its arguments and environment.
    /// For the slices' tests below.
    #[allow(dead_code)]
    fn started(&self, name: &str) -> (Vec<String>, Vec<String>) {
        let read = |ext: &str| std::fs::read_to_string(self.root.join(format!("{name}.{ext}"))).unwrap_or_default().lines().map(String::from).collect();
        (read("args"), read("env"))
    }

    fn gruntas(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// `gruntas <args>` in the stand-in home, to run.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("GRUNTAS_API_URL", "http://127.0.0.1:9/v1")
            .current_dir(&self.root)
            .stdin(Stdio::null());
        c
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn failed(o: &Output, code: i32, says: &str) {
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(code), "{err}");
    assert!(err.contains(says), "{says:?} in {err}");
}

#[test]
fn run_refuses_what_it_cannot_launch_and_names_the_fix() {
    let b = Sandbox::new("refuse");
    failed(&b.gruntas(&["run", "gemini"]), 1, "claude or codex");
    failed(&b.gruntas(&["run", "claude", "--account", "nope"]), 2, "gruntas connect anthropic --method subscription --name nope");
    failed(&b.gruntas(&["run", "codex", "--account", "claude:work"]), 1, "not a codex account");
    failed(&b.gruntas(&["run", "claude", "--account", "work", "--unattended", "fix it", "--no-sandbox"]), 1, "drop --no-sandbox");
    failed(&b.gruntas(&["run", "claude", "--settings", "gone.json"]), 1, "--settings gone.json");
    // Before the harness, or anywhere else, gruntas's run flags are refused.
    failed(&b.gruntas(&["run", "--account", "work", "claude"]), 1, "right after `run`");
    failed(&b.gruntas(&["push", "a.png", "--account", "work"]), 1, "only a flag of `gruntas run`");
    assert!(!b.root.join("claude.args").exists(), "nothing started");
    let help = b.gruntas(&["help", "run"]);
    assert!(help.status.success() && String::from_utf8_lossy(&help.stdout).contains("gruntas run <claude|codex>"));
}

#[test]
#[ignore = "LN02: gruntas run claude attended execs claude per the plan"]
fn claude_attended_execs_claude_with_the_account_plugin_and_settings() {
    // Each account form (none, claude, work, claude:work) and --no-sandbox:
    // `started("claude")` has CLAUDE_CONFIG_DIR, --plugin-dir, one
    // --settings and the person's arguments.
    let _b = Sandbox::new("ln02");
}

/// R-RUN-3, against a daemon in this process: `--unattended` starts a
/// `--bg` session in the sandbox and prints its id, or refuses where the
/// sandbox cannot run; the daemon stops the session once its Task is
/// terminal and its turn over (`claude stop <id>` under the account's
/// config home), and not while it is unfinished. This test process plays
/// the session's harness, sending what its plugin's hooks would.
#[test]
fn claude_unattended_starts_in_the_background_and_prints_the_session() {
    use gruntas_harness::protocol::{Activity, PluginHarness, Registration};
    use std::os::unix::fs::PermissionsExt;
    let b = Sandbox::new("ln03");
    let script = |path: PathBuf, body: &str| {
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    // `claude --bg` says the id it backgrounded; `claude stop` writes down
    // what it stopped and under which config home. bubblewrap and socat
    // stand in as present.
    let out = b.root.display().to_string();
    script(b.root.join("bin/claude"), &format!("if [ \"$1\" = stop ]; then printf '%s %s\\n' \"$2\" \"$CLAUDE_CONFIG_DIR\" >> '{out}/claude.stop'; exit 0; fi\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{out}/claude.args'\necho 'Starting background service…'\necho 'backgrounded · 1234abcd'\n"));
    for bin in ["bwrap", "socat"] {
        script(b.root.join("bin").join(bin), "exit 0\n");
    }
    // Under /tmp, so the socket's path fits `sun_path`; gone with the test.
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let run = Gone(PathBuf::from("/tmp").join(format!("gruntas-ln03-{}", std::process::id())));
    let run = &run.0;
    std::fs::create_dir_all(run).unwrap();
    let (home, run_dir) = (b.root.join("home").display().to_string(), run.display().to_string());
    let env = move |k: &str| match k {
        "HOME" => home.clone(),
        "XDG_RUNTIME_DIR" => run_dir.clone(),
        _ => String::new(),
    };
    let socket = gruntas_harness::daemon::socket(&env).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let daemon_socket = socket.clone();
    std::thread::spawn(move || {
        use gruntas_harness::daemon::server;
        let factory: server::Factory = Box::new(|_, _| Err(gruntas_harness::engine::EngineError::new("no_sessions", "this test runs none")));
        server::run(server::Options { socket: daemon_socket, idle: None, gruntas_version: "test".into(), ..Default::default() }, factory)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(std::time::Instant::now() < deadline, "the daemon never listened");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let gruntas = |path: &str| b.command(&["run", "claude", "--account", "work", "--unattended", "fix it"]).env("XDG_RUNTIME_DIR", run).env("PATH", path).output().unwrap();

    // No sandbox, no session: on Linux, without bubblewrap.
    if cfg!(target_os = "linux") {
        std::fs::create_dir_all(b.root.join("bare")).unwrap();
        std::fs::copy(b.root.join("bin/claude"), b.root.join("bare/claude")).unwrap();
        failed(&gruntas(&b.root.join("bare").display().to_string()), 1, "cannot run here: the sandbox needs bubblewrap");
        assert!(!b.root.join("claude.args").exists(), "nothing started");
    }

    let o = gruntas(&b.root.join("bin").display().to_string());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "1234abcd\n", "its id");
    let (args, _) = b.started("claude");
    assert_eq!((args[0].as_str(), args.last().map(String::as_str)), ("--bg", Some("fix it")));
    let settings = &args[args.iter().position(|a| a == "--settings").unwrap() + 1];
    assert_eq!(serde_json::from_str::<serde_json::Value>(settings).unwrap()["sandbox"]["failIfUnavailable"], true, "the full block");

    // Its plugin's hooks, as the session runs: registered, a turn ended
    // while it is still working, then done and its turn over.
    let session = "1234abcd-5678-4000-8000-000000000000";
    let send = |a: Activity| gruntas_harness::hook::send(&socket, "/r", "test", a).unwrap();
    let config_home = b.root.join("accounts/claude-work").display().to_string();
    send(Activity::Register(Registration { session_id: session.into(), harness: PluginHarness::Claude, harness_pid: Some(std::process::id()), config_home: Some(config_home.clone()), cwd: None, repository: None, transcript_path: None, attendance: None }));
    send(Activity::TurnEnd { session_id: session.into() });
    // A stop is started as the activity is taken, and every sweep.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(!b.root.join("claude.stop").exists(), "an unfinished Task's session runs on");
    let report = gruntas_client::status::Report { state: Some(gruntas_client::status::TaskState::Completed), phase: None, blocked_on: None, pr: None, next: None, timestamp: "2026-11-24T00:00:00Z".into() };
    send(Activity::Report { session_id: session.into(), summary: "fixed".into(), report });
    send(Activity::TurnEnd { session_id: session.into() });
    let stopped = b.root.join("claude.stop");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !stopped.exists() {
        assert!(std::time::Instant::now() < deadline, "the finished session was never stopped");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(std::fs::read_to_string(&stopped).unwrap(), format!("1234abcd {config_home}\n"), "stopped once, under its account's config home");
}

#[test]
#[ignore = "LN04: gruntas run codex attended execs codex in the permissions profile"]
fn codex_attended_execs_codex_with_the_account_and_profile() {
    let _b = Sandbox::new("ln04");
}

/// R-RUN-2: `--account auto` ranks the harness's own accounts from the
/// usage record, as rollover does, measured from the default account's
/// login, and says which it picked and why.
#[test]
fn account_auto_picks_the_account_with_the_most_headroom() {
    let b = Sandbox::new("ln05");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(b.home().join("config.json")).unwrap()).unwrap();
    config["instances"]["claude:spare"] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("accounts/claude-work") });
    std::fs::write(b.home().join("config.json"), config.to_string()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let window = |used: f64| serde_json::json!({ "five_hour": { "status": "allowed", "usedPercent": used, "resetsAtMs": now + 3_600_000, "seenAtMs": now } });
    let login = |login: &str| serde_json::json!({ "login": login, "seenAtMs": now });
    // The default and claude:work share a login, so work is never a move;
    // codex:team has the most room of all, but is no claude account.
    let usage = serde_json::json!({
        "instances": { "claude": login("me"), "claude:work": login("me"), "claude:spare": login("spare"), "codex:team": login("team") },
        "logins": { "me": window(5.0), "spare": window(30.0), "team": window(0.0) },
    });
    std::fs::write(b.home().join("usage.json"), usage.to_string()).unwrap();
    let o = b.gruntas(&["run", "claude", "--account", "auto"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--account auto: claude:spare, the most headroom: 70% left as of") && err.contains("next reset"), "{err}");

    // Nothing else with room: the default account, saying so.
    let usage = serde_json::json!({ "instances": { "claude": login("me"), "claude:spare": login("spare"), "claude:work": login("me") }, "logins": { "spare": window(95.0) } });
    std::fs::write(b.home().join("usage.json"), usage.to_string()).unwrap();
    let err = String::from_utf8_lossy(&b.gruntas(&["run", "claude", "--account", "auto"]).stderr).into_owned();
    assert!(err.contains("--account auto: claude, your own account"), "{err}");
}
