//! `krowk -p`, the built binary, against the stand-in Anthropic API: the
//! three output formats, a resumed session reading the cache, the session
//! listed beside an imported Claude one, and a rebuild that re-derives it
//! from the log alone.

#![cfg(feature = "harness")]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
    url: String,
}

impl Sandbox {
    fn new(name: &str, url: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-harness-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        // The fake `claude` and `codex`, signed in to nothing, first on
        // PATH: a bare model is routed, which asks every vendor there is,
        // and never the real ones the machine may have.
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for (dir, bin) in [("claude", "fake-claude"), ("codex", "fake-codex")] {
            let at = root.join("bin").join(dir);
            std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(dir).join(bin), &at).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let root = root.canonicalize().unwrap();
        Sandbox { root, url: url.into() }
    }

    /// PATH with the fakes first.
    fn path(&self) -> String {
        format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default())
    }

    fn krowk(&self, args: &[&str]) -> Output {
        self.krowk_with(args, "sk-test")
    }

    fn krowk_with(&self, args: &[&str], key: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(args)
            .env_clear()
            .env("PATH", self.path())
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("ANTHROPIC_API_KEY", key)
            .env("ANTHROPIC_BASE_URL", &self.url)
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.krowk(args);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "krowk {args:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        stdout
    }

    fn json(&self, args: &[&str]) -> Value {
        let s = self.ok(args);
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("krowk {args:?}: {e}: {s}"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn krowk_sessions(b: &Sandbox) -> Vec<Value> {
    b.json(&["sessions", "--json"])["data"]["sessions"].as_array().unwrap().clone()
}

#[test]
fn r_log_5_krowk_p_answers_streams_resumes_and_lists_beside_imported_sessions() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("session", &m.url);

    // text: the answer, after a read of the README in the working directory.
    let text = b.ok(&["-p", "read README.md and summarise it in one line", "--model", "claude-sonnet-4-6"]);
    assert_eq!(text, "krowk turns agent output — screenshots, logs, diffs — into permalinks you can paste anywhere.\n");
    {
        let seen = m.seen.lock().unwrap();
        let result = &seen[1].body["messages"][2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert!(result["content"].as_str().unwrap().contains("Permalinks for agent output."), "{result}");
    }

    // The session is in krowk.db straight away, harness krowk.
    let listed = krowk_sessions(&b);
    let ours = listed.iter().find(|s| s["harness"] == "krowk").expect("the -p session is listed");
    let session_id = ours["foreign_session_id"].as_str().unwrap().to_string();
    assert_eq!(ours["title"], "read README.md and summarise it in one line");

    // stream-json on a resume, by the krowk.db id: start/delta/end, then the result.
    let stream = b.ok(&["-p", "what language is it written in?", "--resume", ours["id"].as_str().unwrap(), "--output-format", "stream-json"]);
    let lines: Vec<Value> = stream.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    for want in ["turn.started", "item.started", "item.delta", "item.completed", "response.completed", "turn.completed"] {
        assert!(types.contains(&want), "{want} missing from {types:?}");
    }
    let result = lines.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["sessionId"], session_id.as_str(), "the same session continued");
    assert!(result["usage"]["cacheReadTokens"].as_i64().unwrap() > 0, "{result}");
    for k in ["inputTokens", "outputTokens", "cacheWriteTokens", "reasoningTokens"] {
        assert!(result["usage"][k].is_i64(), "{k} in {result}");
    }
    assert!(result["costUsd"].as_f64().unwrap() > 0.0 && result["durationMs"].is_u64());

    // json: the result event alone.
    let one = b.json(&["-p", "hello", "--output-format", "json", "--model", "anthropic/claude-sonnet-4-6"]);
    assert_eq!((one["type"].as_str(), one["status"].as_str()), (Some("result"), Some("completed")));

    // An imported Claude session lists beside the native ones.
    let project = b.root.join("home/.claude/projects/-repo");
    std::fs::create_dir_all(&project).unwrap();
    let line = serde_json::json!({"type": "user", "sessionId": "c1a0de00-0000-4000-8000-000000000001", "uuid": "u1", "cwd": b.root.join("repo"), "message": {"role": "user", "content": "hello from claude"}});
    std::fs::write(project.join("c1a0de00-0000-4000-8000-000000000001.jsonl"), format!("{line}\n")).unwrap();
    b.ok(&["sessions", "import", "--from", "all", "--json"]);
    let listed = krowk_sessions(&b);
    let harnesses: Vec<&str> = listed.iter().map(|s| s["harness"].as_str().unwrap()).collect();
    assert_eq!(harnesses.iter().filter(|h| **h == "krowk").count(), 2, "{harnesses:?}");
    assert!(harnesses.contains(&"claude"), "{harnesses:?}");

    // R-LOG-2: a rebuild deletes krowk.db and re-derives the native
    // sessions from their JSONL alone.
    let before = b.json(&["sessions", "show", &session_id, "--json"])["data"].clone();
    b.ok(&["sessions", "rebuild", "--yes", "--json"]);
    let after = b.json(&["sessions", "show", &session_id, "--json"])["data"].clone();
    let strip = |v: &Value| {
        let mut v = v.clone();
        v.as_object_mut().unwrap().remove("id");
        v
    };
    assert_eq!(strip(&after), strip(&before));
    assert_eq!(after["turns"].as_array().unwrap().len(), 2);
    assert_eq!(after["harness"], "krowk");
}

#[test]
fn p_flags_are_refused_elsewhere_and_bad_values_are_named() {
    let b = Sandbox::new("flags", "http://127.0.0.1:9");
    let out = b.krowk(&["sessions", "--model", "x"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("only a flag of `krowk -p`"));
    let out = b.krowk(&["-p", "hi", "--permission-mode", "yolo"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("bypassPermissions"));
    let out = b.krowk(&["-p", "hi", "--output-format", "xml"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("stream-json"));
    let out = b.krowk(&["-p"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a prompt"));
    // No key: refused before a session exists, exit 3, the variable named.
    let out = b.krowk_with(&["-p", "hi"], "");
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ANTHROPIC_API_KEY"));
    assert!(!b.root.join("home/.krowk/sessions").exists(), "a refused prompt leaves no session behind");
    // Nothing listening: named as unreachable, exit 6, and R-OFF-1's words
    // lead the fix line.
    let out = b.krowk(&["-p", "hi", "--model", "claude-sonnet-4-6"]);
    assert_eq!(out.status.code(), Some(6), "{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(err.contains("network_unreachable") && err.contains("no network connectivity"), "r_off_1: {err}");
}

/// R-TOOL-2 through the built binary: the recorded tool definitions carry
/// the edit tool of the model's family — from the models.dev cache when it
/// knows the model, from the id otherwise — and config's `toolset` and
/// `--toolset` override it, in that order.
#[test]
fn r_tool_2_krowk_p_records_each_familys_edit_tool_and_toolset_overrides_it() {
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::fixture("turn2_answer.sse")));
    let b = Sandbox::new("toolset", &m.url);
    let cache = b.root.join("home/.krowk/cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("models.json"), r#"{"anthropic": {"models": {"house-coder": {"family": "grok-build"}}}}"#).unwrap();
    let tools = |args: &[&str]| -> (String, Vec<String>) {
        let r = b.json(&[&["-p", "hello", "--output-format", "json"], args].concat());
        let id = r["sessionId"].as_str().unwrap().to_string();
        let raw = std::fs::read_to_string(b.root.join("home/.krowk/sessions").join(id).join("context.jsonl")).unwrap();
        let rec: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        let names = rec["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        (rec["toolset"].as_str().unwrap().to_string(), names)
    };
    let edit = |args: &[&str]| {
        let (preset, names) = tools(args);
        assert_eq!([&names[..2], &names[3..]].concat(), ["read", "write", "bash", "grep", "glob", "todo_write", "publish", "subagent"], "{args:?}");
        (preset, names[2].clone())
    };
    let pair = |p: &str, e: &str| (p.to_string(), e.to_string());
    assert_eq!(edit(&["--model", "claude-sonnet-4-6"]), pair("claude", "str_replace"));
    assert_eq!(edit(&["--model", "anthropic/gpt-5.1-codex"]), pair("gpt", "apply_patch"));
    assert_eq!(edit(&["--model", "anthropic/grok-code-fast-1"]), pair("grok", "search_replace"));
    assert_eq!(edit(&["--model", "house-coder"]), pair("grok", "search_replace"), "the catalog's family");
    assert_eq!(edit(&["--model", "anthropic/gpt-5.1-codex", "--toolset", "claude"]), pair("claude", "str_replace"));
    // Config pins one for every model; --toolset still wins.
    let config = b.root.join("home/.krowk");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.json"), r#"{"toolset": "gpt"}"#).unwrap();
    assert_eq!(edit(&["--model", "claude-sonnet-4-6"]), pair("gpt", "apply_patch"));
    assert_eq!(edit(&["--model", "claude-sonnet-4-6", "--toolset", "grok"]), pair("grok", "search_replace"));

    let out = b.krowk(&["-p", "hi", "--toolset", "vim"]);
    assert_eq!(out.status.code(), Some(1), "a usage error");
    assert!(String::from_utf8_lossy(&out.stderr).contains("claude, gpt, grok"));
    let out = b.krowk(&["sessions", "--toolset", "gpt"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("only a flag of `krowk -p`"));
    std::fs::write(config.join("config.json"), r#"{"toolset": "vim"}"#).unwrap();
    let out = b.krowk(&["-p", "hi"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("is not a toolset"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[cfg(unix)]
#[test]
fn ctrl_c_interrupts_a_turn_waiting_on_the_model_and_keeps_the_session() {
    // A model that takes its time: the interrupt has to reach a turn that is
    // waiting on the network, not one between events.
    let m = mock::serve(|b, n| {
        std::thread::sleep(std::time::Duration::from_secs(5));
        mock::readme_script(b, n)
    });
    let b = Sandbox::new("interrupt", &m.url);
    let child = Command::new(env!("CARGO_BIN_EXE_krowk"))
        .args(["-p", "hi", "--model", "anthropic/claude-sonnet-4-6", "--output-format", "json"])
        .env_clear()
        .env("PATH", b.path())
        .env("HOME", b.root.join("home"))
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("ANTHROPIC_API_KEY", "sk-test")
        .env("ANTHROPIC_BASE_URL", &b.url)
        .current_dir(b.root.join("repo"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let started = std::time::Instant::now();
    // SAFETY: SIGINT to the child this test spawned.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "the interrupt did not wait for the model");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!(result["status"], "interrupted");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--resume"));
    let listed = krowk_sessions(&b);
    assert_eq!(listed[0]["harness"], "krowk", "the interrupted session is kept and listed");
}

/// Claude Code's `permissions.defaultMode: "auto"` in `~/.claude/settings.json`
/// is a mode krowk does not run: the prompt still runs, in default, with a
/// notice naming the file — and `--permission-mode` wins without one. A
/// deny rule that does not parse still refuses the prompt.
#[test]
fn r_perm_1_krowk_p_runs_beside_a_claude_default_mode_it_does_not_run() {
    let m = mock::serve(mock::readme_script);
    let b = Sandbox::new("auto-mode", &m.url);
    let settings = b.root.join("home/.claude/settings.json");
    let write = |deny: &str| {
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let v = serde_json::json!({"model": "opus", "permissions": {"allow": ["Bash(npm test)"], "deny": [deny], "defaultMode": "auto"}, "alwaysThinkingEnabled": true});
        std::fs::write(&settings, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    };
    let mode_of = |out: &Output| -> String {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let started = stdout.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).find(|l| l["type"] == "turn.started").unwrap_or_else(|| panic!("no turn.started in {stdout}"));
        started["permissionMode"].as_str().unwrap().to_string()
    };
    write("Read(.env)");

    let out = b.krowk(&["-p", "read README.md and summarise it in one line", "--model", "claude-sonnet-4-6", "--output-format", "stream-json"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{err}");
    assert_eq!(mode_of(&out), "default");
    assert!(err.contains("~/.claude/settings.json sets defaultMode \"auto\"") && err.contains("asks before edits and commands"), "the notice names the file and the value: {err}");

    let out = b.krowk(&["-p", "read README.md and summarise it in one line", "--model", "claude-sonnet-4-6", "--output-format", "stream-json", "--permission-mode", "acceptEdits"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{err}");
    assert_eq!(mode_of(&out), "acceptEdits");
    assert!(!err.contains("\"auto\""), "with the flag given the file's mode is not mentioned: {err}");

    write("Read(.env");
    let out = b.krowk(&["-p", "hi", "--model", "claude-sonnet-4-6", "--permission-mode", "acceptEdits"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success() && err.contains("bad_settings") && err.contains("permissions.deny"), "{err}");
}
