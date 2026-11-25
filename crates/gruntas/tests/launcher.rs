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

/// `script`, as `script` does, run by a copy of bash named `harness`: a
/// fake harness that runs `gruntas hook` itself must not look like a shell,
/// which the hook passes over looking for its harness. Linux names a
/// script's process after the script; macOS after its interpreter, which
/// for `#!/bin/sh` is `sh`.
/// A transcript with something said in it, which `claude --resume` takes up.
const SAID: &str = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";

fn harness_script(b: &Sandbox, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let bash = ["/bin/bash", "/usr/bin/bash"].into_iter().find(|p| std::path::Path::new(p).exists()).expect("bash");
    let interpreter = b.root.join("harness");
    std::fs::copy(bash, &interpreter).unwrap();
    std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = b.root.join("bin").join(name);
    std::fs::write(&path, format!("#!{}\n{body}", interpreter.display())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `name`'s fake replaced by `script`, a shell script's body. Runnable even
/// where there was no fake before (`socat`): gruntas looks for an
/// executable on PATH, and would otherwise find the machine's own.
fn script(b: &Sandbox, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = b.root.join("bin").join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    std::fs::write(&transcript, SAID).unwrap();
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
    assert_eq!(std::fs::read_to_string(spare.join(format!("projects/-r/{session}.jsonl"))).unwrap(), SAID, "the transcript, where --resume finds it");
    let daemon = std::fs::read_dir(b.root.join("run/gruntas")).unwrap().flatten().find(|e| e.path().join("attended").is_dir()).unwrap().path();
    assert_eq!(std::fs::read_dir(daemon.join("attended")).unwrap().count(), 0, "the records gone with the run");
}

/// An account the daemon queues as the one a run is on runs low is not
/// one the run already left: going back would only go round the same
/// accounts, so the run ends there — until a window of it has reset. One a
/// person picked is gone to anyway.
#[test]
fn a_run_never_rolls_back_to_an_account_it_left_until_it_resets() {
    let b = Sandbox::new("rollover-loop");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(b.home().join("config.json")).unwrap()).unwrap();
    std::fs::create_dir_all(b.root.join("accounts/claude-spare")).unwrap();
    config["instances"]["claude:spare"] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("accounts/claude-spare") });
    std::fs::write(b.home().join("config.json"), config.to_string()).unwrap();
    let (runs, queue) = (b.root.join("runs"), b.root.join("queue"));
    std::fs::create_dir_all(b.root.join("run")).unwrap();
    // Each run writes down its home and queues the next of queue.1,
    // queue.2, … left, then exits with its count.
    script(&b, "claude", &format!("i=0\nuntil a=$(ls -d {1}/gruntas/*/attended 2>/dev/null) && [ -e \"$a/$$.json\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.01; done\necho \"$CLAUDE_CONFIG_DIR | $*\" >> '{0}'\nfor q in '{2}'.*; do [ -e \"$q\" ] && mv \"$q\" $a/$$.next.json && break; done\nexit $(wc -l < '{0}')\n", runs.display(), b.root.join("run").display(), queue.display()));
    let run = || b.command(&["run", "claude", "--account", "work", "--no-sandbox"]).env("XDG_RUNTIME_DIR", b.root.join("run")).output().unwrap();
    let lines = || std::fs::read_to_string(&runs).unwrap_or_default().lines().map(String::from).collect::<Vec<_>>();
    let session = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e64";
    let transcript = b.root.join(format!("accounts/claude-work/projects/-r/{session}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, SAID).unwrap();
    let queued = |n: u32, account: &str, picked: bool| std::fs::write(b.root.join(format!("queue.{n}")), serde_json::json!({ "session": session, "account": account, "rung": "same-harness", "picked": picked }).to_string()).unwrap();

    // work → spare, then the daemon's pick of work again: the run ends on spare.
    queued(1, "claude:spare", false);
    queued(2, "claude:work", false);
    let o = run();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "spare's own code; {err}");
    assert_eq!(lines().len(), 2, "{:?}", lines());
    assert!(err.contains(&format!("session {session} is not moved back to claude:work, which this run left when it ran low and has not reset since")), "{err}");

    // A person's pick goes back.
    std::fs::remove_file(&runs).unwrap();
    queued(1, "claude:spare", false);
    queued(2, "claude:work", true);
    let o = run();
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(lines()[2].starts_with(&format!("{} |", b.root.join("accounts/claude-work").display())), "{:?}", lines());

    // Back to work automatically once a window of its login that was
    // running when the run left it has reset: it has room again. The first
    // harness writes that window, to reset a second or two after it ends;
    // the second outlasts it.
    std::fs::remove_file(&runs).unwrap();
    queued(1, "claude:spare", false);
    queued(2, "claude:work", false);
    let usage = b.home().join("usage.json");
    script(&b, "claude", &format!("i=0\nuntil a=$(ls -d {1}/gruntas/*/attended 2>/dev/null) && [ -e \"$a/$$.json\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.01; done\necho \"$CLAUDE_CONFIG_DIR | $*\" >> '{0}'\nfor q in '{2}'.*; do [ -e \"$q\" ] && mv \"$q\" $a/$$.next.json && break; done\nn=$(wc -l < '{0}')\nif [ $n = 1 ]; then r=$(( $(date +%s) * 1000 + 2000 )); echo \"{{\\\"logins\\\":{{\\\"w@x.io\\\":{{\\\"five_hour\\\":{{\\\"status\\\":\\\"limited\\\",\\\"usedPercent\\\":100,\\\"resetsAtMs\\\":$r,\\\"seenAtMs\\\":$((r - 60000))}}}}}},\\\"instances\\\":{{\\\"claude:work\\\":{{\\\"login\\\":\\\"w@x.io\\\",\\\"seenAtMs\\\":1}}}}}}\" > '{3}'; fi\n[ $n = 2 ] && sleep 3\nexit $n\n", runs.display(), b.root.join("run").display(), queue.display(), usage.display()));
    let o = run();
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(lines()[2].starts_with(&format!("{} |", b.root.join("accounts/claude-work").display())), "{:?}", lines());
}

/// A switch from the default account, run in the home the shell exports
/// (`CLAUDE_CONFIG_DIR`, as `claude` there would): the transcript is carried
/// from that home. A session left before anything was said has no
/// transcript to carry, so a new one starts on the next account.
#[test]
fn a_switch_carries_from_the_inherited_home_and_an_empty_session_starts_afresh() {
    let b = Sandbox::new("switch-inherited");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(b.home().join("config.json")).unwrap()).unwrap();
    // The default account written down with Claude Code's own home, as
    // `gruntas connect` leaves it; the shell exports another.
    config["instances"]["claude"] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("home/.claude") });
    std::fs::create_dir_all(b.root.join("accounts/claude-spare")).unwrap();
    config["instances"]["claude:spare"] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("accounts/claude-spare") });
    std::fs::write(b.home().join("config.json"), config.to_string()).unwrap();
    let (runs, queue) = (b.root.join("runs"), b.root.join("queue"));
    std::fs::create_dir_all(b.root.join("run")).unwrap();
    // As RL03's: each run writes down its home and arguments; the first
    // queues what the test left (once), and a resumed run exits 3.
    script(&b, "claude", &format!("i=0\nuntil a=$(ls -d {1}/gruntas/*/attended 2>/dev/null) && [ -e \"$a/$$.json\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.01; done\necho \"$CLAUDE_CONFIG_DIR | $*\" >> '{0}'\ncase \"$*\" in *--resume*) exit 3;; esac\n[ -e '{2}' ] && mv '{2}' $a/$$.next.json\nexit 5\n", runs.display(), b.root.join("run").display(), queue.display()));
    let elsewhere = b.root.join("accounts/elsewhere");
    let run = || b.command(&["run", "claude", "--no-sandbox", "--model", "opus"]).env("XDG_RUNTIME_DIR", b.root.join("run")).env("CLAUDE_CONFIG_DIR", &elsewhere).output().unwrap();
    let lines = || std::fs::read_to_string(&runs).unwrap_or_default().lines().map(String::from).collect::<Vec<_>>();
    let spare = b.root.join("accounts/claude-spare");

    let session = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e60";
    let transcript = elsewhere.join(format!("projects/-r/{session}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, SAID).unwrap();
    std::fs::write(&queue, serde_json::json!({ "session": session, "account": "claude:spare", "model": "opus", "rung": "same-harness", "transcript": transcript }).to_string()).unwrap();
    let o = run();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(3), "the resumed run's code; {err}");
    assert!(err.contains(&format!("continuing session {session} on claude:spare")), "{err}");
    assert!(lines()[1].starts_with(&format!("{} |", spare.display())) && lines()[1].contains(&format!("--resume {session}")), "{:?}", lines());
    assert_eq!(std::fs::read_to_string(spare.join(format!("projects/-r/{session}.jsonl"))).unwrap(), SAID, "carried from the inherited home");

    // Nothing said yet: no transcript, so a new session, not a resume.
    std::fs::remove_file(&runs).unwrap();
    let fresh = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e61";
    std::fs::write(&queue, serde_json::json!({ "session": fresh, "account": "claude:spare", "model": "opus", "rung": "same-harness", "transcript": elsewhere.join(format!("projects/-r/{fresh}.jsonl")) }).to_string()).unwrap();
    let o = run();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(5), "the new session's own code; {err}");
    assert!(err.contains(&format!("session {fresh} had nothing said yet, so a new one starts on claude:spare")), "{err}");
    let l = lines();
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(l[1].starts_with(&format!("{} |", spare.display())) && !l[1].contains("--resume") && l[1].contains("--model opus"), "{l:?}");

    // Only a slash command the mod answered: Claude Code wrote a
    // transcript of its own records, with nothing said, which it would
    // refuse to resume.
    std::fs::remove_file(&runs).unwrap();
    let answered = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e63";
    let records = elsewhere.join(format!("projects/-r/{answered}.jsonl"));
    std::fs::write(&records, format!("{{\"type\":\"last-prompt\",\"leafUuid\":\"x\",\"sessionId\":\"{answered}\"}}\n{{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"{answered}\"}}\n")).unwrap();
    std::fs::write(&queue, serde_json::json!({ "session": answered, "account": "claude:spare", "model": "opus", "rung": "same-harness", "transcript": records }).to_string()).unwrap();
    let o = run();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(5), "the new session's own code; {err}");
    assert!(err.contains(&format!("session {answered} had nothing said yet, so a new one starts on claude:spare")), "{err}");
    let l = lines();
    assert!(l.len() == 2 && !l[1].contains("--resume"), "{l:?}");
    assert!(!spare.join(format!("projects/-r/{answered}.jsonl")).exists(), "nothing carried");

    // Back from claude:spare to the default: carried into the home the
    // default runs in, the inherited one, not its written-down one. The
    // queue names no transcript where one is: it is found under spare's.
    std::fs::remove_file(&runs).unwrap();
    let back = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e62";
    let at_spare = spare.join(format!("projects/-q/{back}.jsonl"));
    std::fs::create_dir_all(at_spare.parent().unwrap()).unwrap();
    std::fs::write(&at_spare, SAID).unwrap();
    std::fs::write(&queue, serde_json::json!({ "session": back, "account": "claude", "model": "opus", "rung": "same-harness", "transcript": spare.join(format!("projects/-r/{back}.jsonl")) }).to_string()).unwrap();
    let o = b.command(&["run", "claude", "--account", "spare", "--no-sandbox", "--model", "opus"]).env("XDG_RUNTIME_DIR", b.root.join("run")).env("CLAUDE_CONFIG_DIR", &elsewhere).output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(3), "the resumed run's code; {err}");
    assert!(lines()[1].starts_with(&format!("{} |", elsewhere.display())) && lines()[1].contains(&format!("--resume {back}")), "{:?}", lines());
    assert_eq!(std::fs::read_to_string(elsewhere.join(format!("projects/-q/{back}.jsonl"))).unwrap(), SAID, "carried into the inherited home");
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

/// RL06 (R-RL-7), an attended session end to end: the Launcher runs
/// `claude:work` as its child; the mod's report that the account hit its
/// limit reaches a daemon in this process, which queues `claude:spare` and
/// writes the notice into the session's inbox (as the mod shows it); once
/// the harness exits (`/exit`), the Launcher resumes the same session on
/// `claude:spare`, its transcript carried over.
#[test]
fn rl06_an_attended_session_is_told_and_continues_after_exit() {
    use gruntas_harness::protocol::{Activity, Attendance, PluginHarness, Registration, UsageWindow};
    let b = Sandbox::new("rl06-attended");
    let path = b.home().join("config.json");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    std::fs::create_dir_all(b.root.join("accounts/claude-spare")).unwrap();
    config["instances"]["claude:spare"] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin/claude"), "configDir": b.root.join("accounts/claude-spare") });
    std::fs::write(&path, config.to_string()).unwrap();
    let (work, spare) = (b.root.join("accounts/claude-work"), b.root.join("accounts/claude-spare"));
    // The usage records: the default accounts out of the way (Claude Code's
    // shares work's login, Codex's are spent); spare has room.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let spent = serde_json::json!({ "five_hour": { "status": "limited", "usedPercent": 100.0, "resetsAtMs": now + 3_600_000, "seenAtMs": now } });
    let w = serde_json::json!({ "login": "w", "seenAtMs": 1, "configHome": gruntas_harness::usage::home_key(&work.display().to_string()) });
    let usage = b.root.join("usage.json");
    std::fs::write(&usage, serde_json::json!({ "instances": { "claude": { "login": "w", "seenAtMs": 1 }, "claude:work": w, "codex": { "login": "spent", "seenAtMs": 1 }, "codex:team": { "login": "spent", "seenAtMs": 1 } }, "logins": { "spent": spent } }).to_string()).unwrap();

    let run = PathBuf::from("/tmp").join(format!("gruntas-rl06a-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run);
    std::fs::create_dir_all(&run).unwrap();
    let (home, run_dir) = (b.root.join("home").display().to_string(), run.display().to_string());
    let env = move |k: &str| match k {
        "HOME" => home.clone(),
        "XDG_RUNTIME_DIR" => run_dir.clone(),
        _ => String::new(),
    };
    let registry = gruntas_harness::instances::Registry::resolve(&gruntas_harness::instances::from_config_json(&config).unwrap(), &env);
    let socket = gruntas_harness::daemon::socket(&env).unwrap();
    let daemon = socket.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&daemon).unwrap();
    let opts = gruntas_harness::daemon::server::Options {
        socket: socket.clone(),
        idle: None,
        gruntas_version: "test".into(),
        usage: Some(gruntas_harness::usage::Usage::at(usage)),
        accounts: Some(std::sync::Arc::new(move || Ok(registry.clone()))),
        ..Default::default()
    };
    std::thread::spawn(move || {
        use gruntas_harness::daemon::server;
        let factory: server::Factory = Box::new(|_, _| Err(gruntas_harness::engine::EngineError::new("no_sessions", "this test runs none")));
        server::run(opts, factory)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    wait("the daemon never listened", &|| std::os::unix::net::UnixStream::connect(&socket).is_ok());

    // The harness: the first run says its pid and stays until an account
    // is queued for it, then exits as `/exit` would; the resumed run
    // writes down its account and arguments and exits 3.
    let (pid_file, resumed) = (b.root.join("pid"), b.root.join("resumed"));
    script(&b, "claude", &format!("case \"$*\" in *--resume*) echo \"$CLAUDE_CONFIG_DIR | $*\" > '{1}'; exit 3;; esac\necho $$ > '{0}'\ni=0\nuntil [ -e '{2}/attended/'$$.next.json ]; do i=$((i+1)); [ $i -gt 1000 ] && exit 9; sleep 0.01; done\nexit 0\n", pid_file.display(), resumed.display(), daemon.display()));
    let launcher = b.command(&["run", "claude", "--account", "work", "--no-sandbox", "--model", "opus"]).env("XDG_RUNTIME_DIR", &run).stderr(Stdio::piped()).spawn().unwrap();
    wait("the harness never started", &|| std::fs::read_to_string(&pid_file).is_ok_and(|p| p.ends_with('\n')));
    let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    wait("the Launcher never marked its harness", &|| daemon.join(format!("attended/{pid}.json")).exists());

    // What the plugin's hooks and mod send: the session starts, then its
    // account hits the limit.
    let session = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e71";
    let transcript = work.join(format!("projects/-r/{session}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, SAID).unwrap();
    let send = |a: Activity| gruntas_harness::hook::send(&socket, "/r", "test", a).unwrap();
    send(Activity::Register(Registration { session_id: session.into(), harness: PluginHarness::Claude, harness_pid: Some(pid), config_home: Some(work.display().to_string()), cwd: Some(b.root.display().to_string()), repository: None, transcript_path: Some(transcript.display().to_string()), attendance: Some(Attendance::Attended) }));
    send(Activity::Usage { config_home: work.display().to_string(), windows: vec![UsageWindow { kind: "fiveHour".into(), used_percent: 100.0, resets_at_ms: None }] });

    let o = launcher.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    let inbox = std::fs::read_to_string(gruntas_harness::sync::god::inbox::path(&daemon, session)).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&run);
    assert!(inbox.contains(r#""type":"notice""#) && inbox.contains("This account has hit its usage limit. Next: claude:spare on opus (another subscription) — type /exit to continue this session there."), "the notice: {inbox}");
    assert_eq!(o.status.code(), Some(3), "the resumed run's code; {err}");
    assert!(err.contains(&format!("continuing session {session} on claude:spare")), "{err}");
    let resumed = std::fs::read_to_string(&resumed).unwrap();
    assert!(resumed.starts_with(&format!("{} |", spare.display())) && resumed.contains(&format!("--resume {session} --model opus")), "{resumed}");
    assert_eq!(std::fs::read_to_string(spare.join(format!("projects/-r/{session}.jsonl"))).unwrap(), SAID, "the transcript, where --resume finds it");
}

/// RL07 fixtures: `claude:spare` on another login, `claude:same` on
/// `claude:work`'s, `claude:out` signed out; the usage records say each
/// one's login and `claude:work`'s windows. Every Claude Code fake answers
/// `auth status` as a Max subscription, `claude:out`'s as signed out.
fn rl07_accounts(b: &Sandbox, now_ms: i64) {
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(b.home().join("config.json")).unwrap()).unwrap();
    for name in ["spare", "same", "out"] {
        std::fs::create_dir_all(b.root.join(format!("accounts/claude-{name}"))).unwrap();
        let bin = if name == "out" { "claude-out" } else { "claude" };
        config["instances"][format!("claude:{name}")] = serde_json::json!({ "kind": "claude-code", "binary": b.root.join("bin").join(bin), "configDir": b.root.join(format!("accounts/claude-{name}")) });
    }
    std::fs::write(b.home().join("config.json"), config.to_string()).unwrap();
    script(b, "claude-out", "echo '{\"loggedIn\":false}'\nexit 1\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(b.root.join("bin/claude-out"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let seen = |login: &str| serde_json::json!({ "login": login, "seenAtMs": now_ms });
    let usage = serde_json::json!({
        "logins": { "w@x.io": {
            "five_hour": { "status": "allowed", "usedPercent": 40.0, "resetsAtMs": now_ms + 3_600_000, "seenAtMs": now_ms },
            "seven_day": { "status": "allowed", "usedPercent": 90.0, "resetsAtMs": now_ms + 86_400_000, "seenAtMs": now_ms },
        } },
        "instances": { "claude:work": seen("w@x.io"), "claude:same": seen("w@x.io"), "claude:spare": seen("s@x.io") },
    });
    std::fs::write(b.home().join("usage.json"), usage.to_string()).unwrap();
}

const RL07_AUTH: &str = "case \"$1 $2\" in \"auth status\") echo '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"subscriptionType\":\"max\"}'; exit 0;; esac\n";

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// The mod's picker (`/gruntas:account` bare): `--choices` lists every
/// connected account, those this session can move to first, by what their
/// tightest window has left (unknown after), and the rest with why not:
/// the account it runs on, one signed out, one on its login, Codex's.
#[test]
fn account_choices_list_every_account_movable_first_and_why_not() {
    let b = Sandbox::new("account-choices");
    let now = now_ms();
    rl07_accounts(&b, now);
    script(&b, "claude", RL07_AUTH);
    // claude:spare's login has a window known, half left.
    let mut usage: serde_json::Value = serde_json::from_slice(&std::fs::read(b.home().join("usage.json")).unwrap()).unwrap();
    usage["logins"]["s@x.io"] = serde_json::json!({ "five_hour": { "status": "allowed", "usedPercent": 50.0, "resetsAtMs": now + 3_600_000, "seenAtMs": now } });
    // What a session `gruntas run` launched on it reported: under its home.
    usage["logins"][b.root.join("accounts/claude-spare").display().to_string()] = serde_json::json!({ "seven_day": { "status": "allowed", "usedPercent": 30.0, "resetsAtMs": now + 86_400_000, "seenAtMs": now } });
    std::fs::write(b.home().join("usage.json"), usage.to_string()).unwrap();
    let session = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e72";
    let o = b.command(&["hook", "claude", "account", session, "--choices"]).env("CLAUDE_CONFIG_DIR", b.root.join("accounts/claude-work")).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)));
    assert_eq!((&v["launched"], &v["current"], &v["queued"]), (&serde_json::json!(false), &serde_json::json!("claude:work"), &serde_json::Value::Null), "{v}");
    let rows: Vec<(String, Option<String>)> = v["choices"].as_array().unwrap().iter().map(|c| (c["account"].as_str().unwrap().to_string(), c["why"].as_str().map(String::from))).collect();
    let names: Vec<&str> = rows.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["claude:spare", "claude", "claude:out", "claude:same", "claude:work", "codex", "codex:team"], "{v}");
    assert!(rows[..2].iter().all(|(_, why)| why.is_none()), "{v}");
    let why = |n: &str| rows.iter().find(|(m, _)| m == n).and_then(|(_, w)| w.clone()).unwrap_or_default();
    assert!(why("claude:out").starts_with("it is not signed in"), "{v}");
    assert!(why("claude:same").contains("same login as claude:work"), "{v}");
    assert_eq!(why("claude:work"), "this session runs on it");
    assert_eq!(why("codex:team"), "it is a Codex account, and a Claude Code session moves only to another Claude Code account");
    let label = |n: &str| v["choices"].as_array().unwrap().iter().find(|c| c["account"] == n).map(|c| c["label"].as_str().unwrap_or_default().to_string()).unwrap_or_default();
    assert_eq!(label("claude:spare"), "claude:spare · 30% weekly, 50% 5-hour", "the weekly reported under spare's home, read as its login's");
    assert_eq!(label("claude:out"), "claude:out · not signed in");
    assert_eq!(label("claude:same"), "claude:same · same login as claude:work");
    assert_eq!(label("claude:work"), "claude:work · this session");
    assert_eq!(label("codex:team"), "codex:team · Codex account");
}

/// RL07 (D8, R-RL-8): each account's row says its login, plan, what each
/// window has left and when it resets, and the accounts on the same login.
#[test]
fn rl07_providers_check_rows_carry_login_plan_usage_and_shared_logins() {
    let b = Sandbox::new("rl07-rows");
    let now = now_ms();
    rl07_accounts(&b, now);
    script(&b, "claude", RL07_AUTH);
    let o = b.gruntas(&["providers", "list", "--check", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&o.stderr)));
    let row = |n: &str| v["data"]["instances"].as_array().unwrap().iter().find(|r| r["instance"] == n).cloned().unwrap_or_else(|| panic!("{n} not in {v}"));
    let work = row("claude:work");
    assert_eq!((work["login"].as_str(), work["plan"].as_str(), &work["sharedWith"]), (Some("w@x.io"), Some("max"), &serde_json::json!(["claude:same"])), "{work}");
    assert_eq!(work["usage"], serde_json::json!([
        { "window": "five_hour", "left": 60.0, "resetsAtMs": now + 3_600_000 },
        { "window": "seven_day", "left": 10.0, "resetsAtMs": now + 86_400_000 },
    ]));
    let spare = row("claude:spare");
    assert_eq!((spare["login"].as_str(), &spare["usage"], &spare["sharedWith"]), (Some("s@x.io"), &serde_json::json!([]), &serde_json::json!([])), "{spare}");
    let out = row("claude:out");
    assert_eq!((out["ready"].as_bool(), &out["plan"]), (Some(false), &serde_json::Value::Null), "{out}");
    assert_eq!(row("claude")["login"], serde_json::Value::Null, "never seen: unknown");
    let human = String::from_utf8_lossy(&b.gruntas(&["providers", "list", "--check", "--format", "human"]).stdout).into_owned();
    assert!(human.contains("w@x.io (max) · 90% weekly (resets ") && human.contains("), 40% 5-hour (resets ") && human.contains("same login as claude:same"), "{human}");
    assert!(human.contains("s@x.io (max) · usage unknown"), "{human}");
}

/// RL07 (D8, R-RL-8): inside a session `gruntas run` started,
/// `gruntas hook claude account` (the mod's `/gruntas:account`) lists the
/// accounts with the session's marked; refuses an unknown, other-harness,
/// not-ready, same-login or current account with why and the valid
/// choices; queues a good one, which `--cancel` drops; and after `/exit`
/// the same session resumes on the account queued, on the same model.
/// Outside such a session it says how to start one that can move.
#[test]
fn rl07_a_picked_account_continues_the_session_after_exit() {
    let b = Sandbox::new("rl07-pick");
    rl07_accounts(&b, now_ms());
    std::fs::create_dir_all(b.root.join("run")).unwrap();
    let (runs, said) = (b.root.join("runs"), b.root.join("said"));
    let gruntas = env!("CARGO_BIN_EXE_gruntas");
    let session = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e71";
    // Where Claude Code keeps the session's transcript, for the directory it runs in.
    let slug: String = b.root.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let transcript = b.root.join(format!("accounts/claude-work/projects/{slug}/{session}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, SAID).unwrap();
    harness_script(&b, "claude", &format!("{RL07_AUTH}i=0\nuntil a=$(ls -d {run}/gruntas/*/attended 2>/dev/null) && [ -e \"$a/$$.json\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.01; done\necho \"$CLAUDE_CONFIG_DIR | $*\" >> '{runs}'\ncase \"$*\" in *--resume*) exit 3;; esac\nfor w in . claude:nope codex:team claude:out claude:same claude:work claude:spare --cancel --cancel claude:spare; do [ \"$w\" = . ] && w=; echo \"> $w\" >> '{said}'; {gruntas} hook claude account {session} $w >> '{said}' 2>&1; echo \"= $? queued=$([ -e $a/$$.next.json ] && echo y || echo n)\" >> '{said}'; done\nexit 5\n", run = b.root.join("run").display(), runs = runs.display(), said = said.display()));
    let o = b.command(&["run", "claude", "--account", "work", "--no-sandbox", "--model", "opus"]).env("XDG_RUNTIME_DIR", b.root.join("run")).output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    let said = std::fs::read_to_string(&said).unwrap_or_default();
    assert_eq!(o.status.code(), Some(3), "the resumed run's code; {err}\n{said}");
    let answers: Vec<&str> = said.split("> ").skip(1).collect();
    assert_eq!(answers.len(), 10, "{said}");
    let list = answers[0];
    assert!(list.contains("* claude:work ") && list.contains("  claude:spare ") && !list.contains("codex:team") && list.ends_with("= 0 queued=n\n"), "the list, the session's account marked: {list}");
    let refused = |i: usize, why: &str| assert!(answers[i].contains(why) && answers[i].contains("choose one of claude, claude:spare.") && answers[i].ends_with("= 1 queued=n\n"), "{}", answers[i]);
    refused(1, "There is no account named claude:nope");
    refused(2, "codex:team is not a claude account");
    refused(3, "claude:out is not ready (not signed in");
    refused(4, "claude:same is signed in as the same login as claude:work, so moving there gains nothing");
    refused(5, "This session already runs on claude:work");
    assert_eq!(answers[6], "claude:spare\nType /exit to continue on claude:spare.\n= 0 queued=y\n");
    assert_eq!(answers[7], "--cancel\nclaude:spare is no longer queued: /exit ends this session as usual.\n= 0 queued=n\n");
    assert_eq!(answers[8], "--cancel\nNothing is queued: /exit ends this session as usual.\n= 0 queued=n\n");
    assert_eq!(answers[9], "claude:spare\nType /exit to continue on claude:spare.\n= 0 queued=y\n");
    let l: Vec<String> = std::fs::read_to_string(&runs).unwrap().lines().map(String::from).collect();
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(l[1].starts_with(&format!("{} |", b.root.join("accounts/claude-spare").display())) && l[1].contains(&format!("--resume {session} --model opus")), "{l:?}");
    assert!(err.contains(&format!("continuing session {session} on claude:spare")), "{err}");
    assert!(b.root.join(format!("accounts/claude-spare/projects/{slug}/{session}.jsonl")).exists(), "the transcript, where --resume finds it");

    // Outside a session `gruntas run` started: the list, and no move.
    let o = b.command(&["hook", "claude", "account", session, "claude:spare"]).env("XDG_RUNTIME_DIR", b.root.join("run")).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "This session was not started by gruntas, so it cannot move to another account: start Claude Code with `gruntas run claude` to be able to.\n");
    let o = b.command(&["hook", "claude", "account", session]).env("XDG_RUNTIME_DIR", b.root.join("run")).output().unwrap();
    let list = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && list.contains("* claude ") && list.contains("claude:spare"), "the session's home is ~/.claude: {list}");
    let o = b.command(&["hook", "claude", "account", session]).env("XDG_RUNTIME_DIR", b.root.join("run")).env("CLAUDE_CONFIG_DIR", b.root.join("accounts/claude-spare")).output().unwrap();
    let list = String::from_utf8_lossy(&o.stdout);
    assert!(list.contains("* claude:spare ") && !list.contains("* claude "), "the named account whose home it is: {list}");
    let o = b.command(&["hook", "claude", "account", "--dangerously-skip-permissions", "claude:spare"]).env("XDG_RUNTIME_DIR", b.root.join("run")).output().unwrap();
    assert_eq!((o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned()), (Some(1), "\"--dangerously-skip-permissions\" is not a session id\n".to_string()));
}
