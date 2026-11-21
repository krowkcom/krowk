//! `krowk -p`, the built binary, held to a budget and publishing evidence,
//! on Claude Code (the fake `claude` on PATH, signed in to the default
//! account). Claude Code makes its own model calls, so krowk cannot refuse
//! one mid-turn: it refuses a turn the session is already at its limit for
//! (`--max-usd`, `--max-tokens`), and interrupts a turn as soon as a metered
//! call takes it past, exiting 4 either way; an agent's spend counts toward
//! its session's. `publish`, through krowk's bridge, pushes a file to the
//! stand-in registry with the claim token of an anonymous upload shown to
//! the person alone, and runs nothing of the repository's git config.

#![cfg(all(feature = "harness", unix))]

use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
    /// The stand-in registry's API, when the test runs one.
    api: String,
    /// The krowk key sent to it; empty for a keyless run.
    token: String,
}

impl Sandbox {
    /// A home, a trusted repository, and the fake `claude` first on PATH,
    /// signed in to the default account and playing `scenario`.
    fn new(name: &str, scenario: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-budget-publish-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home/.claude", "home/.krowk", "repo/.git", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        std::fs::write(root.join("home/.claude/fake-login"), "").unwrap();
        std::os::unix::fs::symlink(fixture("fake-claude"), root.join("bin/claude")).unwrap();
        std::fs::write(root.join("scenario.jsonl"), scenario).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("home/.krowk/trusted.json"), json!({"directories": [root.join("repo")]}).to_string()).unwrap();
        Sandbox { root, api: "http://127.0.0.1:9/v1".into(), token: String::new() }
    }

    fn krowk(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_API_URL", &self.api)
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .env("FAKE_CLAUDE_SCENARIO", self.root.join("scenario.jsonl"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        if !self.token.is_empty() {
            c.env("KROWK_TOKEN", &self.token);
        }
        c.output().unwrap()
    }

    fn sessions(&self) -> PathBuf {
        self.root.join("home/.krowk/sessions")
    }

    fn log(&self, session: &str) -> Vec<Value> {
        std::fs::read_to_string(self.sessions().join(session).join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// What the fake logged: every argv, and each line in and out.
    fn fake_log(&self) -> String {
        std::fs::read_to_string(self.root.join("fake.log")).unwrap_or_default()
    }

    /// How many prompts Claude Code was sent, over every process.
    fn prompts(&self) -> usize {
        self.fake_log().lines().filter(|l| l.starts_with(r#"in {"type":"user""#)).count()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude").join(name)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// One model call of Claude Code's that reads README.md, and the read's
/// result: 20,000 tokens read from cache and 40 generated, about $0.0066 at
/// Sonnet's prices, so a cent allows one call and not two.
fn rereading(n: usize) -> String {
    let call = json!({"type": "assistant", "message": {"id": format!("msg_{n:02}"), "model": "claude-sonnet-4-6", "role": "assistant", "content": [{"type": "tool_use", "id": format!("toolu_{n:02}"), "name": "Read", "input": {"file_path": "README.md"}}], "usage": {"input_tokens": 3, "cache_read_input_tokens": 20000, "output_tokens": 40}}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"});
    let read = json!({"type": "user", "message": {"role": "user", "content": [{"tool_use_id": format!("toolu_{n:02}"), "type": "tool_result", "content": "# krowk"}]}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"});
    format!("{call}\n{read}\n")
}

/// A turn of two such calls, and then a wait for krowk's interrupt — a
/// model that would go on reading until it is stopped.
fn reading_until_stopped() -> String {
    let stopped = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","session_id":"{{SESSION}}"}"#;
    format!("{}{}@await-interrupt\n{stopped}\n", rereading(1), rereading(2))
}

/// R-BUDGET-1 on Claude Code: krowk cannot refuse the call that would cross
/// the limit (Claude Code makes it), so it interrupts the turn on the
/// metered call that does, keeping what it made; a turn begun at the limit
/// is refused before Claude Code is sent the prompt.
#[test]
fn r_budget_1_max_usd_interrupts_the_turn_a_call_takes_past_it_and_exits_4() {
    let b = Sandbox::new("usd", &reading_until_stopped());
    let out = b.krowk(&["-p", "read the README until you are told to stop", "--model", "claude/sonnet", "--max-usd", "0.01", "--output-format", "json"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(4), "exits 4, like `krowk sessions budget`: {stdout}{stderr}");
    assert!(b.fake_log().contains("\ninterrupt\n"), "the call past the cent interrupted the turn");
    let result: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert_eq!((result["status"].as_str(), result["error"]["code"].as_str()), (Some("failed"), Some("budget_exceeded")), "{result}");
    let session = result["sessionId"].as_str().unwrap();
    assert!(stderr.contains("budget_exceeded") && stderr.contains("over --max-usd $0.0100"), "{stderr}");
    assert!(stderr.contains(&format!("krowk -p --resume {session} --max-usd")), "the message carries the fix: {stderr}");
    // What it made before the stop is kept: both calls, and their reads.
    let log = b.log(session);
    assert_eq!(log.iter().filter(|e| e["type"] == "response.completed").count(), 2);
    assert_eq!(log.iter().filter(|e| e["item"]["kind"] == "toolResult").count(), 2);

    // The same limit on a resume refuses at once: the session is already
    // past it, so Claude Code is not even sent the prompt.
    let again = b.krowk(&["-p", "go on", "--resume", session, "--max-usd", "0.01"]);
    assert_eq!(again.status.code(), Some(4), "{}", text(&again.stderr));
    assert_eq!(b.prompts(), 1);
    // A limit with room for the next call's least cost lets the turn begin
    // ($0.0132 spent, a call costs at least $0.0060), and it is interrupted
    // once a call takes it past $0.0200.
    let more = b.krowk(&["-p", "go on", "--resume", session, "--max-usd", "0.02", "--output-format", "json"]);
    assert_eq!(more.status.code(), Some(4), "{}", text(&more.stderr));
    assert_eq!(b.prompts(), 2);
    assert_eq!(b.log(session).iter().filter(|e| e["type"] == "response.completed").count(), 4);
}

#[test]
fn r_budget_1_max_tokens_stops_the_session_the_same_way() {
    // Each call generates 40 tokens: the turn's two reach 80, which is
    // inside a limit of 80, and the next call would generate at least one more.
    let done = r#"{"type":"result","subtype":"success","is_error":false,"result":"read it twice","session_id":"{{SESSION}}"}"#;
    let b = Sandbox::new("tokens", &format!("{}{}{done}\n", rereading(1), rereading(2)));
    let first = b.krowk(&["-p", "read the README", "--model", "claude/sonnet", "--max-tokens", "80", "--output-format", "json"]);
    assert!(first.status.success(), "at the limit is inside it: {}", text(&first.stderr));
    let session = serde_json::from_slice::<Value>(&first.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let out = b.krowk(&["-p", "go on", "--resume", &session, "--max-tokens", "80"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert_eq!(b.prompts(), 1, "the next turn was never begun");
    assert!(stderr.contains("budget_exceeded") && stderr.contains("80 tokens generated of --max-tokens 80"), "{stderr}");
    assert!(stderr.contains("--max-tokens <more>"), "{stderr}");
    // A limit given blank is a mistake, not an unlimited run.
    let blank = b.krowk(&["-p", "hi", "--max-tokens", ""]);
    assert_eq!(blank.status.code(), Some(1));
    assert!(text(&blank.stderr).contains("--max-tokens was given with no value"));
    let elsewhere = b.krowk(&["push", "README.md", "--max-usd", "1"]);
    assert!(text(&elsewhere.stderr).contains("only a flag of `krowk -p` and `krowk sessions budget`"));
}

/// A child of a Claude Code session is one of its agents: its calls are
/// metered on the session's own log (`subagent.response`), once each, and
/// count toward the budget the session runs under.
#[test]
fn r_budget_1_a_child_sessions_spend_counts_toward_its_parents_budget() {
    let line = |v: Value| format!("{v}\n");
    let call = |id: &str, content: Value, output: i64, parent: Value| line(json!({"type": "assistant", "message": {"id": id, "model": "claude-sonnet-4-6", "role": "assistant", "content": [content], "usage": {"input_tokens": 10, "output_tokens": output}}, "parent_tool_use_id": parent, "session_id": "{{SESSION}}"}));
    let system = |v: Value| {
        let mut v = v;
        v["type"] = "system".into();
        v["session_id"] = "{{SESSION}}".into();
        line(v)
    };
    let agent = [
        "@when run an agent\n".to_string(),
        call("msg_agent", json!({"type": "tool_use", "id": "toolu_agent", "name": "Agent", "input": {"description": "survey", "subagent_type": "general-purpose", "prompt": "look around"}}), 4, Value::Null),
        system(json!({"subtype": "task_started", "task_id": "a_1", "tool_use_id": "toolu_agent", "description": "survey", "subagent_type": "general-purpose", "is_backgrounded": false, "spawn_depth": 1, "task_type": "local_agent", "prompt": "look around"})),
        // The agent generates 5,000 tokens.
        call("msg_sub", json!({"type": "text", "text": "it is a small repository"}), 5000, "toolu_agent".into()),
        system(json!({"subtype": "task_notification", "task_id": "a_1", "tool_use_id": "toolu_agent", "status": "completed", "summary": "looked"})),
        line(json!({"type": "user", "message": {"role": "user", "content": [{"tool_use_id": "toolu_agent", "type": "tool_result", "content": "it is a small repository"}]}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"})),
        call("msg_done", json!({"type": "text", "text": "The agent looked."}), 3, Value::Null),
        line(json!({"type": "result", "subtype": "success", "is_error": false, "result": "The agent looked.", "session_id": "{{SESSION}}"})),
    ]
    .concat();
    let go_on = format!("@when go on\n{}{}", call("msg_on", json!({"type": "text", "text": "ok"}), 1, Value::Null), line(json!({"type": "result", "subtype": "success", "is_error": false, "result": "ok", "session_id": "{{SESSION}}"})));
    let b = Sandbox::new("child", &format!("{agent}---\n{go_on}"));
    let first = b.krowk(&["-p", "run an agent", "--model", "claude/sonnet", "--output-format", "json"]);
    assert!(first.status.success(), "{}", text(&first.stderr));
    let parent = serde_json::from_slice::<Value>(&first.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let log = b.log(&parent);
    let child = log.iter().find(|e| e["type"] == "subagent.started").unwrap_or_else(|| panic!("the agent is a child session: {log:?}"))["subagentSessionId"].as_str().unwrap().to_string();
    assert_eq!(b.log(&child)[0]["parentSessionId"], parent.as_str());
    assert_eq!(log.iter().filter(|e| e["type"] == "subagent.response").map(|e| e["usage"]["outputTokens"].as_i64().unwrap()).collect::<Vec<_>>(), [5000], "its call, metered once, on the session's log");
    // The session alone has generated 7: 1,000 would allow it. With its
    // agent it is at 5,007, past the limit before any call.
    let out = b.krowk(&["-p", "go on", "--resume", &parent, "--max-tokens", "1000"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("5007 tokens generated"), "{stderr}");
    assert_eq!(b.prompts(), 1, "refused before Claude Code was sent the prompt");
    // And in dollars: 5,000 Sonnet output tokens are $0.075.
    let out = b.krowk(&["-p", "go on", "--resume", &parent, "--max-usd", "0.05"]);
    assert_eq!(out.status.code(), Some(4), "{}", text(&out.stderr));
    let out = b.krowk(&["-p", "go on", "--resume", &parent, "--max-usd", "0.10", "--output-format", "stream-json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    // R-BUDGET-2: the cost frame after each call counts the agent too.
    let cost = text(&out.stdout).lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).find(|l| l["type"] == "cost").expect("a cost frame");
    assert!(cost["costUsd"].as_f64().unwrap() > 0.075 && cost["generatedTokens"].as_i64().unwrap() > 5007, "{cost}");
}

/// R-EVID-1 through krowk's bridge, keyless: Claude Code's `publish` lands
/// anonymously, and its claim token is a secret — the person gets it on
/// stderr, and it is nowhere Claude Code, a log or a program reading stdout
/// would see it.
#[test]
fn r_evid_1_without_a_key_publish_through_the_bridge_shows_its_claim_token_to_the_person_alone() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let mut b = Sandbox::new("keyless", &std::fs::read_to_string(fixture("publish.jsonl")).unwrap());
    b.api = format!("{}/v1", registry.url());
    std::fs::write(b.root.join("repo/shot.png"), PNG).unwrap();
    let out = b.krowk(&["-p", "publish the screenshot", "--model", "claude/sonnet", "--output-format", "stream-json", "--permission-mode", "acceptEdits"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stderr}");
    let session = stdout.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find_map(|l| l["sessionId"].as_str().map(String::from)).unwrap();
    // What Claude Code was answered for its call: the card, and no run.
    let fake = b.fake_log();
    let said = fake.lines().find(|l| l.starts_with("answer ") && l.contains("/a/art_")).unwrap_or_else(|| panic!("no card URL sent to Claude Code: {fake}"));
    assert!(said.contains("belongs to no run") && said.contains("shown to the person rather than to you"), "{said}");
    assert!(!b.log(&session).iter().any(|e| e["type"] == "run.opened"));
    let token = stderr.split_whitespace().find(|w| w.starts_with("krowk_claim_")).unwrap_or_else(|| panic!("no claim command on stderr: {stderr}")).trim_end_matches(['`', ')']).to_string();
    assert!(stderr.contains("krowk claim art_"), "{stderr}");
    assert!(!fake.contains(&token), "Claude Code was sent the claim token");
    assert!(!std::fs::read_to_string(b.sessions().join(&session).join("events.jsonl")).unwrap().contains(&token), "the log holds the claim token");
    assert!(!stdout.contains(&token), "stream-json printed the claim token");
}

/// A repository can name a command in its git config (`core.fsmonitor`);
/// the run metadata a publish through the bridge detects runs none of it.
#[test]
fn r_evid_1_publish_through_the_bridge_detects_the_run_context_without_running_the_repositorys_git_config() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let mut b = Sandbox::new("fsmonitor", &std::fs::read_to_string(fixture("publish.jsonl")).unwrap());
    b.api = format!("{}/v1", registry.url());
    b.token = "krowk_sk_test_fsmonitor".into();
    let repo = b.root.join("repo");
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    if !Command::new("git").args(["init", "-q"]).current_dir(&repo).status().is_ok_and(|s| s.success()) {
        eprintln!("git is not installed: skipping");
        return;
    }
    let marker = b.root.join("fsmonitor-ran");
    let config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
    std::fs::write(repo.join(".git/config"), format!("{config}[core]\n\tfsmonitor = \"touch '{}'; false\"\n", marker.display())).unwrap();
    std::fs::write(repo.join("shot.png"), PNG).unwrap();
    let out = b.krowk(&["-p", "publish the screenshot", "--model", "claude/sonnet", "--output-format", "json", "--permission-mode", "acceptEdits"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let session = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    assert!(b.log(&session).iter().any(|e| e["type"] == "run.opened"), "published, under a run");
    assert!(!marker.exists(), "the repository's fsmonitor ran");
}

/// A real PNG header, so it is served as the image it is.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";
