//! The fake `claude` beside the Claude Code tests (`fixtures/claude/fake-claude`)
//! as an instance: a stand-in model for the tests of the host, the daemon,
//! sync and handoff, which need answers and not a provider. Its scenario
//! is written by the test; see the fake's header for what one holds.
#![allow(dead_code)]

use krowk_harness::instances::InstanceKind;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The fake itself.
pub fn binary() -> PathBuf {
    // Through the workspace, so a test of another crate including this file finds it too.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude/fake-claude")
}

/// A signed-in Claude Code account under `root/<dir>` whose sessions play
/// `scenario` (a plain `ok` a turn without one), logging to `root/<dir>.log`.
pub fn claude(root: &Path, dir: &str, scenario: Option<&str>) -> InstanceKind {
    let config = root.join(dir);
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("fake-login"), "").unwrap();
    let mut env = BTreeMap::from([("FAKE_CLAUDE_LOG".to_string(), log_path(root, dir).display().to_string())]);
    if let Some(s) = scenario {
        let file = root.join(format!("{dir}.scenario.jsonl"));
        std::fs::write(&file, s).unwrap();
        env.insert("FAKE_CLAUDE_SCENARIO".into(), file.display().to_string());
    }
    InstanceKind::ClaudeCode { binary: Some(binary().display().to_string()), config_dir: Some(config.display().to_string()), env, args: Vec::new(), api_key_env: None, effort: None }
}

pub fn log_path(root: &Path, dir: &str) -> PathBuf {
    root.join(format!("{dir}.log"))
}

/// What the fake under `root/<dir>` logged: every argv, and each line in and out.
pub fn log(root: &Path, dir: &str) -> String {
    std::fs::read_to_string(log_path(root, dir)).unwrap_or_default()
}

/// The prompts it was sent: each `user` line krowk wrote it.
pub fn prompts(root: &Path, dir: &str) -> Vec<serde_json::Value> {
    log(root, dir).lines().filter_map(|l| l.strip_prefix("in ")).filter(|l| l.starts_with(r#"{"type":"user""#)).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// A turn's answer: `text`, then its result.
pub fn answer(text: &str) -> String {
    let t = serde_json::to_string(text).unwrap();
    format!(
        "{{\"type\":\"assistant\",\"message\":{{\"id\":\"msg_fake\",\"model\":\"{{{{MODEL}}}}\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":{t}}}],\"usage\":{{\"input_tokens\":3,\"output_tokens\":1}}}},\"parent_tool_use_id\":null,\"session_id\":\"{{{{SESSION}}}}\"}}\n{}",
        result(text)
    )
}

/// A turn's `result` line, saying `text`.
pub fn result(text: &str) -> String {
    let t = serde_json::to_string(text).unwrap();
    format!("{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":{t},\"session_id\":\"{{{{SESSION}}}}\"}}")
}

/// `n` words typed `pause` seconds apart, `per` at a time, and the turn's
/// result: `@type` and the line ending it.
pub fn typed(n: usize, pause: f64, word: &str, per: usize) -> String {
    format!("@type {n} {pause} {word} {per}\n{}", result("typed"))
}

/// A Bash call the model makes, krowk asked whether it may run, and the
/// result the model reads (a denial's message, or empty). The fake runs
/// nothing either way: a test wanting the command's file made adds a
/// `@touch` after it.
pub fn asks_bash(id: &str, command: &str) -> String {
    let input = serde_json::json!({"command": command});
    let call = serde_json::json!({"type": "assistant", "message": {"id": format!("msg_{id}"), "model": "{{MODEL}}", "role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "Bash", "input": input}]}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"});
    let ask = serde_json::json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": input, "tool_use_id": id});
    format!("{call}\n@ask {ask}\n@tool_result {id}")
}
