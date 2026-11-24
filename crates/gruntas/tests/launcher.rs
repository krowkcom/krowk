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
        Command::new(env!("CARGO_BIN_EXE_gruntas"))
            .args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("GRUNTAS_API_URL", "http://127.0.0.1:9/v1")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .unwrap()
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

#[test]
#[ignore = "LN03: gruntas run claude --unattended starts a --bg session and prints its id"]
fn claude_unattended_starts_in_the_background_and_prints_the_session() {
    let _b = Sandbox::new("ln03");
}

#[test]
#[ignore = "LN04: gruntas run codex attended execs codex in the permissions profile"]
fn codex_attended_execs_codex_with_the_account_and_profile() {
    let _b = Sandbox::new("ln04");
}

#[test]
#[ignore = "LN05: --account auto picks by rollover's headroom ranking and says why"]
fn account_auto_picks_the_account_with_the_most_headroom() {
    let _b = Sandbox::new("ln05");
}
