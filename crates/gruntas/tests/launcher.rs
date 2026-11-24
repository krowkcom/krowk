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

/// Each account form and --no-sandbox: `started("claude")` has the
/// account's CLAUDE_CONFIG_DIR, --plugin-dir once the plugin is there, one
/// --settings and the person's arguments; where Claude Code's sandbox
/// cannot run, only --no-sandbox starts it.
#[test]
fn claude_attended_execs_claude_with_the_account_plugin_and_settings() {
    let b = Sandbox::new("ln02");
    // The default account configured with Claude Code's own home, which it
    // must still find with no CLAUDE_CONFIG_DIR.
    let path = b.home().join("config.json");
    let mut config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config["instances"]["claude"] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("home/.claude") });
    std::fs::write(&path, config.to_string()).unwrap();
    // Claude Code's sandbox runs here: bubblewrap and socat are on PATH.
    for bin in ["bwrap", "socat"] {
        b.fake(bin);
    }
    let home = b.home().display().to_string();
    let settings = |args: &[String]| -> serde_json::Value {
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1, "{args:?}");
        let i = args.iter().position(|a| a == "--settings").unwrap();
        serde_json::from_str(&args[i + 1]).unwrap()
    };
    let work = format!("CLAUDE_CONFIG_DIR={}", b.root.join("accounts/claude-work").display());
    let plugin = b.root.join("data/gruntas/plugin");
    for account in [&[][..], &["--account", "claude"], &["--account", "work"], &["--account=claude:work"]] {
        let _ = std::fs::remove_file(b.root.join("claude.args"));
        let o = b.gruntas(&[&["run", "claude"][..], account, &["--model", "opus", "fix it"]].concat());
        assert!(o.status.success(), "{account:?}: {}", String::from_utf8_lossy(&o.stderr));
        let (args, env) = b.started("claude");
        assert_eq!(args[..2], ["--plugin-dir".to_string(), plugin.display().to_string()], "the plugin, written there and passed");
        let s = settings(&args);
        assert_eq!(s["sandbox"]["enabled"], true);
        assert_eq!(s["sandbox"]["filesystem"]["denyRead"][0], serde_json::json!(home), "the home first, then the rest the fence hides");
        assert!(s["hooks"]["WorktreeCreate"].is_array());
        assert!(args.ends_with(&["--model".into(), "opus".into(), "fix it".into()]), "{args:?}");
        let named = account.iter().any(|a| a.contains("work"));
        assert_eq!(env.contains(&work), named, "{account:?}: {env:?}");
        assert_eq!(env.iter().any(|e| e.starts_with("CLAUDE_CONFIG_DIR=")), named, "the default keeps Claude's own home");
    }
    // --no-sandbox: the hooks, no sandbox parts.
    assert!(b.gruntas(&["run", "claude", "--account", "work", "--no-sandbox", "-c"]).status.success());
    let (args, env) = b.started("claude");
    assert_eq!(args[..2], ["--plugin-dir".to_string(), plugin.display().to_string()]);
    let s = settings(&args);
    assert!(s.get("sandbox").is_none() && s.get("permissions").is_none() && s["hooks"]["WorktreeRemove"].is_array(), "{s}");
    assert_eq!(args.last().map(String::as_str), Some("-c"));
    assert!(env.contains(&work));
    // No bubblewrap or socat: refused, and --no-sandbox still starts it.
    if cfg!(target_os = "linux") {
        for bin in ["bwrap", "socat"] {
            std::fs::remove_file(b.root.join("bin").join(bin)).unwrap();
        }
        // The fake claude's own `env`, on the PATH that has nothing else.
        std::os::unix::fs::symlink("/usr/bin/env", b.root.join("bin/env")).unwrap();
        let _ = std::fs::remove_file(b.root.join("claude.args"));
        let bare = |extra: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_gruntas"))
                .args([&["run", "claude"][..], extra].concat())
                .env_clear()
                .env("PATH", b.root.join("bin"))
                .env("HOME", b.root.join("home"))
                .env("XDG_DATA_HOME", b.root.join("data"))
                .env("GRUNTAS_NO_UPDATE_CHECK", "1")
                .env("GRUNTAS_API_URL", "http://127.0.0.1:9/v1")
                .current_dir(&b.root)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        failed(&bare(&[]), 1, "--no-sandbox");
        assert!(!b.root.join("claude.args").exists(), "nothing started");
        assert!(bare(&["--no-sandbox"]).status.success());
        assert!(settings(&b.started("claude").0).get("sandbox").is_none());
    }
}

/// D5: the plugin this binary carries, written into gruntas's own plugin
/// directory, only what differs rewritten; the default account keeps an
/// inherited CLAUDE_CONFIG_DIR, as `claude` in that shell would, and a
/// named account sets its own over it.
#[test]
fn claude_writes_its_plugin_and_the_default_keeps_an_inherited_config_dir() {
    let b = Sandbox::new("ln06");
    for bin in ["bwrap", "socat"] {
        b.fake(bin);
    }
    let plugin = b.root.join("data/gruntas/plugin");
    let work = format!("CLAUDE_CONFIG_DIR={}", b.root.join("accounts/claude-work").display());
    assert!(b.gruntas(&["run", "claude", "--account", "work"]).status.success());
    // The plugin as this binary carries it, and only what differs rewritten.
    let hooks = plugin.join("hooks/hooks.json");
    assert_eq!(std::fs::read_to_string(&hooks).unwrap(), include_str!("../../../hooks/hooks.json"));
    for f in [".claude-plugin/plugin.json", "hooks/usage.ts", "skills/gruntas/SKILL.md"] {
        assert!(plugin.join(f).is_file(), "{f}");
    }
    std::fs::write(&hooks, "{}").unwrap();
    let before = std::fs::metadata(plugin.join("hooks/usage.ts")).unwrap().modified().unwrap();
    // The default account from a shell that exports CLAUDE_CONFIG_DIR: kept,
    // as `claude` there would have it.
    let elsewhere = format!("CLAUDE_CONFIG_DIR={}", b.root.join("accounts/elsewhere").display());
    assert!(b.command(&["run", "claude"]).env("CLAUDE_CONFIG_DIR", b.root.join("accounts/elsewhere")).status().unwrap().success());
    assert!(b.started("claude").1.contains(&elsewhere), "the inherited one kept");
    assert_eq!(std::fs::read_to_string(&hooks).unwrap(), include_str!("../../../hooks/hooks.json"), "put back");
    assert_eq!(std::fs::metadata(plugin.join("hooks/usage.ts")).unwrap().modified().unwrap(), before, "an unchanged file is left alone");
    // A named account sets its own over the inherited one.
    assert!(b.command(&["run", "claude", "--account", "work"]).env("CLAUDE_CONFIG_DIR", b.root.join("accounts/elsewhere")).status().unwrap().success());
    assert!(b.started("claude").1.contains(&work));
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

    // The default account, from a shell that exports CLAUDE_CONFIG_DIR: it
    // runs there, so the daemon, started without it, stops it there.
    let elsewhere = b.root.join("accounts/elsewhere").display().to_string();
    let o = b.command(&["run", "claude", "--unattended", "fix it"]).env("XDG_RUNTIME_DIR", run).env("CLAUDE_CONFIG_DIR", &elsewhere).output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("Stop it sooner with `claude stop 1234abcd`"), "{}", String::from_utf8_lossy(&o.stderr));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::read_to_string(&stopped).unwrap().lines().count() < 2 {
        assert!(std::time::Instant::now() < deadline, "the finished session, launched again, was never stopped");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(&stopped).unwrap().lines().nth(1), Some(format!("1234abcd {elsewhere}").as_str()));
}

/// LN04: `codex` replaces gruntas on the account's CODEX_HOME, in the
/// profile that hides the gruntas home, the person's arguments after it;
/// how to install the plugin is said once per account.
#[test]
fn codex_attended_execs_codex_with_the_account_and_profile() {
    let b = Sandbox::new("ln04");
    let o = b.gruntas(&["run", "codex", "--account", "team", "--model", "gpt-5.5", "resume"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (args, env) = b.started("codex");
    let profile = format!(r#"permissions.gruntas.filesystem={{":root"="read",":project_roots"="write","{}"="none","#, b.home().display());
    assert_eq!(args.len(), 7, "{args:?}");
    assert_eq!((&args[..3], &args[4..]), (&["-c", r#"default_permissions="gruntas""#, "-c"].map(String::from)[..], &["--model", "gpt-5.5", "resume"].map(String::from)[..]));
    assert!(args[3].starts_with(&profile), "the home first, then the rest the fence hides: {}", args[3]);
    assert!(env.contains(&format!("CODEX_HOME={}", b.root.join("accounts/codex-team").display())), "{env:?}");
    let hint = "codex plugin add gruntas@gruntas";
    assert!(String::from_utf8_lossy(&o.stderr).contains(hint), "the plugin hint, the first time");
    let o = b.gruntas(&["run", "codex", "--account", "codex:team", "--no-sandbox"]);
    assert!(o.status.success() && !String::from_utf8_lossy(&o.stderr).contains(hint), "said once per account");
    assert!(b.started("codex").0.is_empty(), "--no-sandbox: no profile");
    // Installed in the account: nothing to say.
    std::fs::create_dir_all(b.root.join("home/.codex/plugins/cache/gruntas/gruntas")).unwrap();
    let o = b.gruntas(&["run", "codex"]);
    assert!(o.status.success() && !String::from_utf8_lossy(&o.stderr).contains(hint), "{}", String::from_utf8_lossy(&o.stderr));
    let (args, env) = b.started("codex");
    assert_eq!(args[..2], ["-c", r#"default_permissions="gruntas""#]);
    assert!(!env.iter().any(|v| v.starts_with("CODEX_HOME=")), "the default account keeps Codex's own home");
}

/// R-RUN-2: `--account auto` ranks the harness's own accounts from the
/// usage record, as rollover does, measured from the default account's
/// login, and says which it picked and why.
#[test]
fn account_auto_picks_the_account_with_the_most_headroom() {
    let b = Sandbox::new("ln05");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(b.home().join("config.json")).unwrap()).unwrap();
    for name in ["claude:apart", "claude:spare"] {
        let dir = b.root.join("accounts").join(name.replace(':', "-"));
        std::fs::create_dir_all(&dir).unwrap();
        config["instances"][name] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": dir });
    }
    std::fs::write(b.home().join("config.json"), config.to_string()).unwrap();
    // `claude auth status`: signed in, unless the account says otherwise.
    let claude = b.root.join("bin/claude");
    std::fs::write(&claude, "#!/bin/sh\nif [ \"$1\" = auth ]; then\n  if [ -e \"$CLAUDE_CONFIG_DIR/signed-out\" ]; then echo '{\"loggedIn\":false}'; else echo '{\"loggedIn\":true}'; fi\nfi\n").unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let window = |used: f64| serde_json::json!({ "five_hour": { "status": "allowed", "usedPercent": used, "resetsAtMs": now + 3_600_000, "seenAtMs": now } });
    let login = |login: &str| serde_json::json!({ "login": login, "seenAtMs": now });
    // The default and claude:work share a login, so work is never a move;
    // claude:apart, first by name, has less room than claude:spare;
    // codex:team has the most of all, but is no claude account.
    let usage = serde_json::json!({
        "instances": { "claude": login("me"), "claude:work": login("me"), "claude:apart": login("apart"), "claude:spare": login("spare"), "codex:team": login("team") },
        "logins": { "me": window(5.0), "apart": window(60.0), "spare": window(30.0), "team": window(0.0) },
    });
    std::fs::write(b.home().join("usage.json"), usage.to_string()).unwrap();
    let o = b.gruntas(&["run", "claude", "--account", "auto"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--account auto: claude:spare, first by rollover's ranking: 70% left as of") && err.contains("next reset"), "{err}");
    // Signed out, it is passed over for the next, as rollover passes it.
    std::fs::write(b.root.join("accounts/claude-spare/signed-out"), "").unwrap();
    let err = String::from_utf8_lossy(&b.gruntas(&["run", "claude", "--account", "auto"]).stderr).into_owned();
    assert!(err.contains("--account auto: claude:apart, first by rollover's ranking: 40% left"), "{err}");

    // Nothing else with room: the default account, saying so.
    let usage = serde_json::json!({ "instances": { "claude": login("me"), "claude:apart": login("apart"), "claude:spare": login("spare"), "claude:work": login("me") }, "logins": { "apart": window(92.0), "spare": window(95.0) } });
    std::fs::write(b.home().join("usage.json"), usage.to_string()).unwrap();
    let err = String::from_utf8_lossy(&b.gruntas(&["run", "claude", "--account", "auto"]).stderr).into_owned();
    assert!(err.contains("--account auto: claude, your own account"), "{err}");
}

/// `name`'s fake replaced by `script`, a shell script's body.
fn script(b: &Sandbox, name: &str, script: &str) {
    std::fs::write(b.root.join("bin").join(name), format!("#!/bin/sh\n{script}")).unwrap();
}

/// RL03 (D5): an attended harness runs as gruntas's child. A SIGINT sent
/// to gruntas alone is not passed on (the terminal sends Ctrl-C to the
/// harness itself); a SIGTERM is, and the harness's exit code is gruntas's.
#[test]
fn rl03_attended_signals_and_exit_code_pass_through() {
    let b = Sandbox::new("rl03-signals");
    let ready = b.root.join("ready");
    std::fs::create_dir_all(b.root.join("run")).unwrap();
    // A next account queued, as after a notice: told to end, gruntas starts
    // no harness again.
    let queued = r#"{"session":"0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e5f","account":"claude:work","rung":"same-harness"}"#;
    script(&b, "claude", &format!("echo run >> {0}.runs\ni=0\nuntil a=$(ls -d {1}/gruntas/*/attended 2>/dev/null) && [ -e \"$a/$$.json\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.01; done\necho '{2}' > $a/$$.next.json\ntrap 'echo int >> {0}.got' INT\ntrap 'exit 42' TERM\ntouch '{0}'\nwhile :; do sleep 0.05; done\n", ready.display(), b.root.join("run").display(), queued));
    let mut run = b.command(&["run", "claude", "--account", "work", "--no-sandbox"]).env("XDG_RUNTIME_DIR", b.root.join("run")).stderr(Stdio::piped()).spawn().unwrap();
    let pid = run.id().to_string();
    let kill = |sig: &str| assert!(Command::new("kill").args([sig, &pid]).status().unwrap().success());
    for _ in 0..200 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready.exists(), "the harness started");
    kill("-INT");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(run.try_wait().unwrap().is_none(), "gruntas ignores a SIGINT of its own");
    assert!(!b.root.join("ready.got").exists(), "and does not pass it on");
    kill("-TERM");
    assert_eq!(run.wait().unwrap().code(), Some(42), "SIGTERM passed on, the harness's code passed back");
    assert_eq!(std::fs::read_to_string(b.root.join("ready.runs")).unwrap(), "run\n", "no harness started again after a SIGTERM");
}

/// RL03 (D5, R-RL-5): the Launcher marks the harness's pid for the daemon;
/// exiting with nothing queued ends the run with the harness's code; with
/// an account queued, the same session resumes there, its transcript
/// copied into that account's home.
#[test]
fn rl03_exit_after_a_notice_resumes_the_session_on_the_next_account() {
    let b = Sandbox::new("rl03-relaunch");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(b.home().join("config.json")).unwrap()).unwrap();
    std::fs::create_dir_all(b.root.join("accounts/claude-spare")).unwrap();
    config["instances"]["claude:spare"] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("accounts/claude-spare") });
    std::fs::write(b.home().join("config.json"), config.to_string()).unwrap();
    let (runs, queue) = (b.root.join("runs"), b.root.join("queue"));
    std::fs::create_dir_all(b.root.join("run")).unwrap();
    // Each run writes down its account, its arguments and the Launcher's
    // mark; a first run with something to queue queues it, as the daemon
    // would, and exits 0; a resumed run exits 3.
    script(&b, "claude", &format!("i=0\nuntil a=$(ls -d {1}/gruntas/*/attended 2>/dev/null) && [ -e \"$a/$$.json\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.01; done\necho \"$CLAUDE_CONFIG_DIR | $* | $(cat $a/$$.json)\" >> '{0}'\ncase \"$*\" in *--resume*) exit 3;; esac\n[ -e '{2}' ] && cp '{2}' $a/$$.next.json\nexit 5\n", runs.display(), b.root.join("run").display(), queue.display()));
    let run = || b.command(&["run", "claude", "--account", "work", "--no-sandbox", "--model", "opus"]).env("XDG_RUNTIME_DIR", b.root.join("run")).output().unwrap();
    let lines = || std::fs::read_to_string(&runs).unwrap_or_default().lines().map(String::from).collect::<Vec<_>>();
    let work = b.root.join("accounts/claude-work");

    let o = run();
    assert_eq!(o.status.code(), Some(5), "nothing queued: the harness's own code; {}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(lines().len(), 1);
    assert!(lines()[0].ends_with(r#"| {"account":"claude:work","model":"opus"}"#), "the mark: {:?}", lines());

    let session = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e5f";
    let transcript = work.join(format!("projects/-r/{session}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    std::fs::write(&queue, serde_json::json!({ "session": session, "account": "claude:spare", "model": "opus", "rung": "same-harness", "transcript": transcript }).to_string()).unwrap();
    std::fs::remove_file(&runs).unwrap();
    let o = run();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(3), "the resumed run's code; {err}");
    assert!(err.contains(&format!("continuing session {session} on claude:spare")), "{err}");
    let l = lines();
    assert_eq!(l.len(), 2, "{l:?}");
    let spare = b.root.join("accounts/claude-spare");
    assert!(l[0].starts_with(&format!("{} |", work.display())), "{l:?}");
    assert!(l[1].starts_with(&format!("{} |", spare.display())) && l[1].contains(&format!("--resume {session} --model opus")), "{l:?}");
    assert!(l[1].ends_with(r#"| {"account":"claude:spare","model":"opus"}"#), "{l:?}");
    assert_eq!(std::fs::read_to_string(spare.join(format!("projects/-r/{session}.jsonl"))).unwrap(), "{}\n", "the transcript, where --resume finds it");
    let daemon = std::fs::read_dir(b.root.join("run/gruntas")).unwrap().flatten().find(|e| e.path().join("attended").is_dir()).unwrap().path();
    assert_eq!(std::fs::read_dir(daemon.join("attended")).unwrap().count(), 0, "the records gone with the run");
}

/// RL03 with RL04 (D5, D6): an attended Codex session whose queued next
/// account is on Claude Code continues there after `/exit`, in the
/// background from a handoff (`continue_on`), the run ending with Codex's
/// own code and saying where the work went on.
#[test]
fn rl03_exit_after_a_notice_for_the_other_harness_continues_there() {
    use gruntas_harness::protocol::{Activity, Attendance, PluginHarness, Registration};
    let b = Sandbox::new("rl03-cross");
    let run = PathBuf::from("/tmp").join(format!("gruntas-rl03x-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run);
    std::fs::create_dir_all(&run).unwrap();
    let (home, run_dir) = (b.root.join("home").display().to_string(), run.display().to_string());
    let env = move |k: &str| match k {
        "HOME" => home.clone(),
        "XDG_RUNTIME_DIR" => run_dir.clone(),
        _ => String::new(),
    };
    let socket = gruntas_harness::daemon::socket(&env).unwrap();
    let daemon = socket.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&daemon).unwrap();
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
    // The Codex session, registered and reported, so its status Task is
    // there to hand off from.
    let session = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e70";
    let send = |a: Activity| gruntas_harness::hook::send(&socket, "/r", "test", a).unwrap();
    send(Activity::Register(Registration { session_id: session.into(), harness: PluginHarness::Codex, harness_pid: Some(std::process::id()), config_home: None, cwd: Some(b.root.display().to_string()), repository: None, transcript_path: None, attendance: Some(Attendance::Attended) }));
    let report = gruntas_client::status::Report { state: Some(gruntas_client::status::TaskState::Working), phase: Some("build".into()), blocked_on: None, pr: None, next: Some("wire the seam".into()), timestamp: "2026-11-24T00:00:00Z".into() };
    send(Activity::Report { session_id: session.into(), summary: "half done".into(), report });
    while !std::fs::read_to_string(daemon.join("status.json")).unwrap_or_default().contains(session) {
        assert!(std::time::Instant::now() < deadline + std::time::Duration::from_secs(5), "the Task was never written");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Codex queues Claude Code next, as the daemon would, and exits 4;
    // `claude --bg` says the id it backgrounded.
    let next = format!(r#"{{"session":"{session}","account":"claude:work","model":"claude-opus-4-5","rung":"other-harness"}}"#);
    script(&b, "codex", &format!("i=0\nuntil [ -e '{0}/attended/'$$.json ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.01; done\necho '{next}' > '{0}/attended/'$$.next.json\nexit 4\n", daemon.display()));
    let out = b.root.display().to_string();
    script(&b, "claude", &format!("for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{out}/claude.args'\necho 'backgrounded · 1234abcd'\n"));
    for bin in ["bwrap", "socat"] {
        script(&b, bin, "exit 0\n");
    }
    let o = b.command(&["run", "codex", "--account", "team", "--no-sandbox"]).env("XDG_RUNTIME_DIR", &run).output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    let _ = std::fs::remove_dir_all(&run);
    assert_eq!(o.status.code(), Some(4), "Codex's own code; {err}");
    assert!(err.contains(&format!("session {session} continues on claude:work as session 1234abcd")), "{err}");
    let (args, _) = b.started("claude");
    assert_eq!(args.first().map(String::as_str), Some("--bg"), "{args:?}");
    assert!(args.windows(2).any(|w| w == ["--model", "claude-opus-4-5"]) && args.iter().any(|l| l.contains("Summary: half done")), "the handoff, at the tier's model: {args:?}");
}
