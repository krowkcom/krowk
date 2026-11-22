//! `gruntas config set defaultMode|rollover` and `gruntas config get`, the built
//! binary against a temp home: both keys always write the global config
//! (defaultMode nested as the engine reads it) and leave other keys alone,
//! an invalid value names the valid ones, `rollover`'s source is repo when a
//! trusted repository sets it, else global, else default, and a `gruntas -p`
//! started afterwards runs in the mode that was set.

#![cfg(feature = "harness")]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("gruntas-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# gruntas\n\nPermalinks for agent output.\n").unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn gruntas(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args)
            .env_clear()
            // The fake `claude` first on PATH, for the session that runs.
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        let scenario = self.root.join("scenario.jsonl");
        if scenario.exists() {
            c.env("FAKE_CLAUDE_SCENARIO", scenario);
        }
        c.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let out = self.gruntas(args);
        assert!(out.status.success(), "gruntas {args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("gruntas {args:?}: {e}"))
    }

    fn global(&self) -> PathBuf {
        self.root.join("home/.gruntas/config.json")
    }

    fn read_global(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.global()).unwrap()).unwrap()
    }

    fn get(&self, key: &str) -> (String, String) {
        let v = self.ok(&["config", "get", key, "--json"]);
        (v["data"]["value"].as_str().unwrap().into(), v["data"]["source"].as_str().unwrap().into())
    }

    fn trust_repo(&self) {
        std::fs::create_dir_all(self.root.join("home/.gruntas")).unwrap();
        std::fs::write(self.root.join("home/.gruntas/trusted.json"), json!({"directories": [self.root.join("repo")]}).to_string()).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn r_cut_6_default_mode_is_set_nested_in_the_global_config_and_read_back() {
    let b = Sandbox::new("mode");
    assert_eq!(b.get("defaultMode"), ("default".into(), "default".into()), "nothing set runs as default");

    std::fs::create_dir_all(b.global().parent().unwrap()).unwrap();
    std::fs::write(b.global(), json!({"tui": {"statusBar": false}, "permissions": {"allow": ["Bash(ls)"]}}).to_string()).unwrap();
    let wrote = b.ok(&["config", "set", "defaultMode", "unhinged", "--json"]);
    assert_eq!(wrote["data"]["value"], "unhinged");
    assert_eq!(b.read_global(), json!({"tui": {"statusBar": false}, "permissions": {"allow": ["Bash(ls)"], "defaultMode": "unhinged"}}), "nested, and the tui.* key is untouched");
    assert_eq!(b.get("defaultMode"), ("unhinged".into(), "global".into()));
    assert!(!b.root.join("repo/.gruntas/config.json").exists(), "never the repo's");
}

#[test]
fn r_cut_6_an_invalid_value_is_refused_naming_the_valid_ones() {
    let b = Sandbox::new("invalid");
    for (key, bad, valid) in [("defaultMode", "plan", "default, unhinged"), ("rollover", "sometimes", "auto, offer, off")] {
        let out = b.gruntas(&["config", "set", key, bad]);
        let err = String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success() && err.contains(valid), "{key}: {err}");
    }
    let out = b.gruntas(&["config", "set", "rollover"]);
    assert!(!out.status.success() && (String::from_utf8_lossy(&out.stderr) + String::from_utf8_lossy(&out.stdout)).contains("auto|offer|off"), "a missing value names them too");
    let out = b.gruntas(&["config", "unset", "defaultMode"]);
    assert!(!out.status.success(), "these two are set, not removed");
    assert!(!b.global().exists(), "nothing was written");
}

#[test]
fn r_cut_7_rollover_source_is_repo_when_trusted_else_global_else_default() {
    let b = Sandbox::new("rollover");
    assert_eq!(b.get("rollover"), ("offer".into(), "default".into()));

    b.ok(&["config", "set", "rollover", "auto"]);
    assert_eq!(b.read_global()["rollover"], "auto");
    assert_eq!(b.get("rollover"), ("auto".into(), "global".into()));

    std::fs::create_dir_all(b.root.join("repo/.gruntas")).unwrap();
    std::fs::write(b.root.join("repo/.gruntas/config.json"), json!({"rollover": "off"}).to_string()).unwrap();
    assert_eq!(b.get("rollover"), ("auto".into(), "global".into()), "an untrusted repo's value is not believed");

    b.trust_repo();
    assert_eq!(b.get("rollover"), ("off".into(), "repo".into()));
}

#[test]
fn r_cut_7_get_reads_workspace_and_refuses_an_unknown_key() {
    let b = Sandbox::new("workspace");
    assert_eq!(b.get("workspace").1, "none");
    b.ok(&["config", "set", "workspace", "ws_one", "--global"]);
    assert_eq!(b.get("workspace"), ("ws_one".into(), "global config".into()));
    let out = b.gruntas(&["config", "get", "nonsense"]);
    assert!(!out.status.success() && (String::from_utf8_lossy(&out.stderr) + String::from_utf8_lossy(&out.stdout)).contains("workspace, defaultMode, rollover"));
}

/// A session on Claude Code (the fake `claude`, signed in to the default
/// account) started after `config set defaultMode unhinged` runs in that
/// mode: gruntas answers Claude Code's permission asks itself, and allows a
/// command it would refuse headless in default with nobody to ask.
#[cfg(unix)]
#[test]
fn r_cut_6_a_session_started_after_config_set_uses_the_default_mode() {
    let b = Sandbox::new("session");
    std::fs::create_dir_all(b.root.join("bin")).unwrap();
    std::os::unix::fs::symlink(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/claude/fake-claude"), b.root.join("bin/claude")).unwrap();
    std::fs::create_dir_all(b.root.join("home/.claude")).unwrap();
    std::fs::write(b.root.join("home/.claude/fake-login"), "").unwrap();
    b.trust_repo();
    let input = json!({"command": "rm -rf build"});
    let call = json!({"type": "assistant", "message": {"id": "msg_rm", "model": "{{MODEL}}", "role": "assistant", "content": [{"type": "tool_use", "id": "toolu_rm", "name": "Bash", "input": input}]}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"});
    let ask = json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": input, "tool_use_id": "toolu_rm"});
    let done = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"{{SESSION}}"}"#;
    std::fs::write(b.root.join("scenario.jsonl"), format!("{call}\n@ask {ask}\n@tool_result toolu_rm\n{done}\n")).unwrap();
    b.ok(&["config", "set", "defaultMode", "unhinged"]);
    let out = b.gruntas(&["-p", "read README.md and summarise it in one line", "--model", "claude/sonnet", "--output-format", "stream-json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let started = stdout.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).find(|l| l["type"] == "turn.started").expect("turn.started");
    assert_eq!(started["permissionMode"], "unhinged");
    let fake = std::fs::read_to_string(b.root.join("fake.log")).unwrap();
    let answer = fake.lines().find(|l| l.starts_with("answer ") && l.contains("fake_3")).unwrap_or_else(|| panic!("the ask was answered: {fake}"));
    assert!(answer.contains(r#""behavior":"allow""#), "{answer}");
}
