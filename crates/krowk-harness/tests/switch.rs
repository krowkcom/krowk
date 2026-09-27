//! Switching a session's model, instance or engine, through the host,
//! against stand-ins: the Anthropic, OpenAI Responses and Chat Completions
//! (xAI) APIs as mock servers, and the fake `claude` and `codex` binaries.
//! No key and no login is real.
//!
//! - Every pair of engines round-trips (R-SWITCH-5): a turn on one, a turn
//!   on the other, back on the first — the second engine reads the first's
//!   turn, and the first reads the second's on its way back.
//! - Native to native is lossless for text, tool calls and results, and
//!   reasoning from another provider arrives as framed plain text
//!   (R-SWITCH-1); into a backend it is a seeded handoff (R-SWITCH-2);
//!   back from one the native model reads its turns whole (R-SWITCH-3).
//! - A switch that cannot run is refused with its fix, and the session stays
//!   where it was (R-SWITCH-4).
//! - Two accounts of one vendor: the transcript is copied and resumed whole,
//!   or a summary when it cannot be (R-INST-4).
//! - Limits: per instance (R-INST-6), an offer by default (R-INST-7), and
//!   `rollover = "auto"` moving on, telling every client and logging it
//!   (R-INST-8).

#![cfg(unix)]

#[path = "common/mock.rs"]
mod mock;
#[path = "common/providers.rs"]
mod providers;

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstanceKind, InstancesConfig, Registry, Rollover};
use krowk_harness::log;
use krowk_harness::protocol::{
    BudgetLimits, Command, ContextRecord, HandoffKind, Item, LimitState, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine, SwitchReason, TurnStatus,
};
use mock::Reply;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

/// One test at a time: a process another test spawns holds a copy of every
/// open descriptor between its fork and its exec, a session log's lock
/// included.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fixture(dir: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(dir).join(name)
}

/// The last thing the person asked, in whichever wire API's shape.
fn last_prompt(body: &Value) -> String {
    let text_of = |c: &Value| -> Option<String> {
        match c {
            Value::String(s) => Some(s.clone()),
            Value::Array(parts) => parts.iter().rev().find_map(|p| p.get("text").and_then(Value::as_str).map(String::from)),
            _ => None,
        }
    };
    let msgs = body.get("messages").or_else(|| body.get("input")).and_then(Value::as_array).cloned().unwrap_or_default();
    msgs.iter().rev().filter(|m| m["role"] == "user").find_map(|m| text_of(&m["content"])).unwrap_or_default()
}

/// Whether the request's last input is a tool's output: the model answers.
fn after_tool(body: &Value) -> bool {
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        return input.last().is_some_and(|i| matches!(i["type"].as_str(), Some("custom_tool_call_output" | "function_call_output")));
    }
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let last = msgs.last().cloned().unwrap_or_default();
    last["role"] == "tool" || last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"))
}

/// What each stand-in model says: a tool call when the prompt asks for one,
/// else an answer naming the engine and the prompt it answers.
fn anthropic_script(body: &Value, _n: usize) -> Reply {
    let p = last_prompt(body);
    // A subagent's own conversation starts with its task; it answers
    // slowly, so a switch can land while the fan-out runs.
    let first = body["messages"][0]["content"].as_array().and_then(|c| c.iter().find_map(|b| b["text"].as_str())).unwrap_or_default().to_string();
    if first.contains("TASK-S") {
        return Reply::paced(mock::text_stream(&"child did it ".repeat(12)), std::time::Duration::from_millis(60));
    }
    if !after_tool(body) && p.contains("fan out") {
        return Reply::sse(&mock::tool_use("toolu_sub", "subagent", &json!({"description": "read it", "prompt": "TASK-S read the README"})));
    }
    if !after_tool(body) && p.contains("call a tool") {
        return Reply::sse(&mock::tool_use(&format!("toolu_an_{}", p.len()), "read", &json!({"path": "README.md"})));
    }
    Reply::sse(&mock::text_stream(&format!("anthropic answered: {p}")))
}

fn responses_script(body: &Value, _n: usize) -> Reply {
    let p = last_prompt(body);
    if !after_tool(body) && p.contains("call a tool") {
        return Reply::sse(&providers::fixture("openai/turn1_apply_patch.sse"));
    }
    Reply::sse(&providers::fixture("openai/turn1_answer.sse").replace("The README now says: Permalinks for everything agents make.", &format!("openai answered: {p}")))
}

fn chat_script(body: &Value, _n: usize) -> Reply {
    let p = last_prompt(body);
    if !after_tool(body) && p.contains("call a tool") {
        return Reply::sse(&providers::fixture("chat/xai_tool_call.sse"));
    }
    Reply::sse(&providers::fixture("chat/xai_answer.sse").replace("The README now says: Permalinks for everything agents make.", &format!("xai answered: {p}")))
}

/// The engines a pair is made of.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Eng {
    Anthropic,
    Openai,
    Xai,
    Claude,
    Codex,
}

impl Eng {
    const ALL: [Eng; 5] = [Eng::Anthropic, Eng::Openai, Eng::Xai, Eng::Claude, Eng::Codex];

    fn model(self) -> ModelRef {
        let (i, m) = match self {
            Eng::Anthropic => ("anthropic", "claude-sonnet-4-6"),
            Eng::Openai => ("openai", "gpt-5.4"),
            Eng::Xai => ("xai", "grok-4.7"),
            Eng::Claude => ("claude:work", "haiku"),
            Eng::Codex => ("codex:team", "gpt-5.5"),
        };
        ModelRef { instance: i.into(), model: m.into() }
    }

    fn native(self) -> bool {
        !matches!(self, Eng::Claude | Eng::Codex)
    }
}

struct World {
    root: PathBuf,
    anthropic: mock::Mock,
    openai: mock::Mock,
    xai: mock::Mock,
    instances: Vec<(String, InstanceKind)>,
    rollover: Option<Rollover>,
    order: Vec<String>,
    trust: Option<krowk_harness::trust::Gate>,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl World {
    fn new(name: &str) -> World {
        World::with(name, mock::serve(anthropic_script))
    }

    fn with(name: &str, anthropic: mock::Mock) -> World {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("krowk-switch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        for (dir, bin) in [("claude", "fake-claude"), ("codex", "fake-codex")] {
            let to = root.join("bin").join(dir);
            std::fs::copy(fixture(dir, bin), &to).unwrap();
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let mut w = World { root, anthropic, openai: mock::serve(responses_script), xai: mock::serve(chat_script), instances: Vec::new(), rollover: None, order: Vec::new(), trust: None, _serial: guard };
        let key = |v: &str| Some(v.to_string());
        w.instances = vec![
            ("anthropic".into(), InstanceKind::AnthropicApi { api_key_env: key("TEST_ANTHROPIC_KEY"), base_url: Some(w.anthropic.url.clone()), thinking: None, max_tokens: None, effort: None }),
            ("openai".into(), InstanceKind::OpenaiApi { api_key_env: key("TEST_OPENAI_KEY"), base_url: Some(format!("{}/v1", w.openai.url)), wire_api: None, effort: None }),
            ("xai".into(), InstanceKind::XaiApi { api_key_env: key("TEST_XAI_KEY"), base_url: Some(format!("{}/v1", w.xai.url)), effort: None }),
        ];
        w.claude("claude:work", "claude-work", Some("switch.jsonl"), true);
        w.codex("codex:team", "codex-team", Some("switch.jsonl"));
        w
    }

    /// A Claude Code account: the fake, its config directory (signed in or
    /// not), and a scenario.
    fn claude(&mut self, name: &str, dir: &str, scenario: Option<&str>, signed_in: bool) -> PathBuf {
        let d = self.root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        if signed_in {
            std::fs::write(d.join("fake-login"), "").unwrap();
        }
        let mut env = BTreeMap::from([("FAKE_CLAUDE_LOG".to_string(), self.root.join(format!("{dir}.log")).display().to_string())]);
        if let Some(s) = scenario {
            env.insert("FAKE_CLAUDE_SCENARIO".into(), fixture("claude", s).display().to_string());
        }
        let kind = InstanceKind::ClaudeCode { binary: Some(self.root.join("bin/claude").display().to_string()), config_dir: Some(d.display().to_string()), env, args: Vec::new(), api_key_env: None, effort: None };
        self.instances.retain(|(n, _)| n != name);
        self.instances.push((name.into(), kind));
        d
    }

    fn codex(&mut self, name: &str, dir: &str, scenario: Option<&str>) -> PathBuf {
        let d = self.root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("fake-login"), "chatgpt team@example.com\n").unwrap();
        let mut env = BTreeMap::from([("FAKE_CODEX_LOG".to_string(), self.root.join(format!("{dir}.log")).display().to_string())]);
        if let Some(s) = scenario {
            env.insert("FAKE_CODEX_SCENARIO".into(), fixture("codex", s).display().to_string());
        }
        let kind = InstanceKind::CodexAppServer { binary: Some(self.root.join("bin/codex").display().to_string()), codex_home: Some(d.display().to_string()), env, args: Vec::new(), api_key_env: None, effort: None };
        self.instances.retain(|(n, _)| n != name);
        self.instances.push((name.into(), kind));
        d
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            // No `claude` or `codex` on PATH: the implicit instances of the
            // real binaries are not candidates to switch to.
            "PATH" => String::new(),
            "TEST_ANTHROPIC_KEY" | "TEST_OPENAI_KEY" | "TEST_XAI_KEY" => "sk-test".into(),
            _ => String::new(),
        }
    }

    fn host(&self) -> Host {
        let cfg = InstancesConfig { instances: self.instances.iter().cloned().collect(), rollover: self.rollover, rollover_order: self.order.clone(), ..Default::default() };
        Host::new(HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.root.join("repo"),
            registry: Registry::resolve(&cfg, &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.config/krowk/providers/credentials.json"),
            trust: self.trust.clone().unwrap_or_else(krowk_harness::trust::allow_all),
            publisher: None,
            permissions: Default::default(),
            agents: krowk_harness::subagent::AgentsConfig::none(),
        })
    }

    fn events(&self, session: &str) -> Vec<LogEvent> {
        log::read_events(&log::sessions_dir(&self.env()).unwrap().join(session).join(log::EVENTS_FILE)).unwrap()
    }

    fn context(&self, session: &str) -> Vec<ContextRecord> {
        let raw = std::fs::read_to_string(log::sessions_dir(&self.env()).unwrap().join(session).join(log::CONTEXT_FILE)).unwrap();
        raw.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn fake_log(&self, dir: &str) -> String {
        std::fs::read_to_string(self.root.join(format!("{dir}.log"))).unwrap_or_default()
    }

    /// The prompts a fake Claude Code was sent, as the text of each `user`
    /// line krowk wrote.
    fn claude_prompts(&self, dir: &str) -> Vec<String> {
        self.fake_log(dir)
            .lines()
            .filter_map(|l| l.strip_prefix("in "))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["type"] == "user")
            .filter_map(|v| v.pointer("/message/content").and_then(Value::as_str).map(String::from))
            .collect()
    }

    /// The input of each `turn/start` a fake Codex was sent.
    fn codex_prompts(&self, dir: &str) -> Vec<String> {
        self.fake_log(dir)
            .lines()
            .filter_map(|l| l.strip_prefix("in "))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["method"] == "turn/start")
            .filter_map(|v| v.pointer("/params/input/0/text").and_then(Value::as_str).map(String::from))
            .collect()
    }

    /// What `e` was sent for its latest turn: a native API's request bodies
    /// since `from`, or a backend's last prompt.
    fn seen_by(&self, e: Eng, from: usize) -> String {
        let bodies = |m: &mock::Mock| m.seen.lock().unwrap()[from..].iter().map(|s| s.body.to_string()).collect::<Vec<_>>().join("\n");
        match e {
            Eng::Anthropic => bodies(&self.anthropic),
            Eng::Openai => bodies(&self.openai),
            Eng::Xai => bodies(&self.xai),
            Eng::Claude => self.claude_prompts("claude-work").last().cloned().unwrap_or_default(),
            Eng::Codex => self.codex_prompts("codex-team").last().cloned().unwrap_or_default(),
        }
    }

    fn requests(&self, e: Eng) -> usize {
        match e {
            Eng::Anthropic => self.anthropic.seen.lock().unwrap().len(),
            Eng::Openai => self.openai.seen.lock().unwrap().len(),
            Eng::Xai => self.xai.seen.lock().unwrap().len(),
            _ => 0,
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn prompt(session: Option<&str>, text: &str, model: Option<ModelRef>) -> Command {
    Command::Prompt { session_id: session.map(String::from), text: text.into(), model, permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }
}

async fn run(host: &Host, cmd: Command) -> (RunResult, Vec<StreamLine>) {
    let (tx, mut rx) = mpsc::channel(4096);
    let r = host.execute(cmd, tx).await.unwrap().unwrap();
    let mut lines = Vec::new();
    while let Ok(l) = rx.try_recv() {
        lines.push(l);
    }
    (r, lines)
}

/// A text as it appears inside a JSON request body.
fn escaped(s: &str) -> String {
    let j = serde_json::to_string(s).unwrap();
    j[1..j.len() - 1].to_string()
}

/// The last assistant text of each turn of the session, in order.
fn answers(events: &[LogEvent]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ev in events {
        match &ev.body {
            LogBody::TurnStarted { .. } => out.push(String::new()),
            LogBody::ItemCompleted { item: Item::AssistantText { text }, .. } => {
                if let Some(last) = out.last_mut() {
                    last.clone_from(text);
                }
            }
            _ => {}
        }
    }
    out
}

fn handoffs(events: &[LogEvent]) -> Vec<(HandoffKind, Option<String>, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            LogBody::BackendHandoff { how, from_instance, fell_back, .. } => Some((*how, from_instance.clone(), fell_back.clone())),
            _ => None,
        })
        .collect()
}

/// Whether `seen` — what an engine was sent — holds `text`, the way that
/// engine is sent it: escaped in a JSON body, or as it is in a backend's
/// prompt.
fn holds(e: Eng, seen: &str, text: &str) -> bool {
    if e.native() { seen.contains(&escaped(text)) } else { seen.contains(text) }
}

#[test]
fn r_switch_5_every_pair_of_engines_round_trips() {
    for (i, a) in Eng::ALL.iter().enumerate() {
        for b in &Eng::ALL[i + 1..] {
            let (a, b) = (*a, *b);
            let w = World::new(&format!("pair-{a:?}-{b:?}"));
            let host = w.host();
            let rt = rt();
            let ask = [format!("call a tool: step one on {a:?}"), format!("call a tool: step two on {b:?}"), format!("step three, back on {a:?}")];
            let (r1, _) = rt.block_on(run(&host, prompt(None, &ask[0], Some(a.model()))));
            assert_eq!(r1.status, TurnStatus::Completed, "{a:?}: {:?}", r1.error);
            let id = r1.session_id.clone();
            let from = w.requests(b);
            let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), &ask[1], Some(b.model()))));
            assert_eq!(r2.status, TurnStatus::Completed, "{a:?} → {b:?}: {:?}", r2.error);
            let said = answers(&w.events(&id));
            let seen = w.seen_by(b, from);
            assert!(holds(b, &seen, &ask[0]) && holds(b, &seen, &said[0]), "{b:?} reads {a:?}'s turn — its prompt and its answer {:?}:\n{seen}", said[0]);
            if !b.native() {
                assert!(seen.starts_with("<handoff from krowk>\n") && seen.ends_with(&ask[1]), "into a backend, a handoff then the prompt (R-SWITCH-2): {seen}");
            }
            let from = w.requests(a);
            let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), &ask[2], Some(a.model()))));
            assert_eq!(r3.status, TurnStatus::Completed, "{b:?} → {a:?}: {:?}", r3.error);
            let said = answers(&w.events(&id));
            let seen = w.seen_by(a, from);
            assert!(holds(a, &seen, &ask[1]) && holds(a, &seen, &said[1]), "{a:?} reads {b:?}'s turn on the way back — {:?}:\n{seen}", said[1]);
            if a.native() {
                assert!(holds(a, &seen, &ask[0]) && holds(a, &seen, &said[0]), "and its own first turn, whole: {seen}");
                // A backend's reasoning reaches it as framed plain text,
                // Codex's too, which has no blob at all (R-SWITCH-1).
                let thought = match b {
                    Eng::Codex => Some("<reasoning from an earlier model>\nCodex plans step 1."),
                    Eng::Claude => Some("<reasoning from an earlier model>\nClaude Code thinks about step 1."),
                    _ => None,
                };
                if let Some(t) = thought {
                    assert!(holds(a, &seen, t), "{a:?} gets {b:?}'s reasoning as text: {seen}");
                }
            } else {
                // Its own thread, caught up on the turn it did not run —
                // not told its own turn again.
                assert!(seen.starts_with("<handoff from krowk>\nWhile you were not running this conversation") && !seen.contains(&ask[0]), "{seen}");
                assert!(handoffs(&w.events(&id)).iter().any(|h| h.0 == HandoffKind::CatchUp));
            }
            assert_eq!(said.len(), 3);
            drop(host);
        }
    }
}

#[test]
fn r_switch_1_native_to_native_carries_calls_and_results_and_downgrades_reasoning() {
    let w = World::new("native");
    let host = w.host();
    let rt = rt();
    // A GPT turn with OpenAI's encrypted reasoning and an apply_patch call.
    let (r1, _) = rt.block_on(run(&host, prompt(None, "call a tool: patch the README", Some(Eng::Openai.model()))));
    assert_eq!(r1.status, TurnStatus::Completed, "{:?}", r1.error);
    let id = r1.session_id.clone();
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "now on claude", Some(Eng::Anthropic.model()))));
    assert_eq!(r2.status, TurnStatus::Completed, "{:?}", r2.error);
    let body = &w.anthropic.seen.lock().unwrap().last().unwrap().body.clone();
    let msgs = body["messages"].as_array().unwrap();
    let blocks: Vec<&Value> = msgs.iter().flat_map(|m| m["content"].as_array().unwrap().iter()).collect();
    // The call and its result, under the id GPT gave them.
    assert!(blocks.iter().any(|b| b["type"] == "tool_use" && b["id"] == "call_01ApplyPatch" && b["name"] == "apply_patch"), "{body}");
    assert!(blocks.iter().any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "call_01ApplyPatch"), "{body}");
    // GPT's reasoning: its summary as framed text, its encrypted blob never.
    let text = body.to_string();
    assert!(text.contains("<reasoning from an earlier model>\\n**Rewording the README**"), "{text}");
    assert!(!text.contains(providers::ENCRYPTED), "OpenAI's encrypted reasoning never goes to Anthropic");
    // On to Grok, then back to GPT, which gets its own blob back unmodified.
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "now on grok", Some(Eng::Xai.model()))));
    assert_eq!(r3.status, TurnStatus::Completed, "{:?}", r3.error);
    let xai = w.xai.seen.lock().unwrap().last().unwrap().body.to_string();
    assert!(xai.contains("call_01ApplyPatch") && xai.contains(&escaped("anthropic answered: now on claude")) && !xai.contains(providers::ENCRYPTED), "{xai}");
    let (r4, _) = rt.block_on(run(&host, prompt(Some(&id), "back on gpt", Some(Eng::Openai.model()))));
    assert_eq!(r4.status, TurnStatus::Completed, "{:?}", r4.error);
    let gpt = w.openai.seen.lock().unwrap().last().unwrap().body.clone();
    let input = gpt["input"].as_array().unwrap();
    assert!(input.iter().any(|i| i["type"] == "reasoning" && i["encrypted_content"] == providers::ENCRYPTED), "its own reasoning replays to OpenAI whole: {gpt}");
    assert!(gpt.to_string().contains(&escaped("xai answered: now on grok")));
}

#[test]
fn r_switch_3_back_from_claude_code_the_native_model_reads_its_turns_whole() {
    let w = World::new("back");
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "read the README with Claude Code", Some(Eng::Claude.model()))));
    assert_eq!(r1.status, TurnStatus::Completed, "{:?}", r1.error);
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "and now natively", Some(Eng::Anthropic.model()))));
    assert_eq!(r2.status, TurnStatus::Completed, "{:?}", r2.error);
    let body = &w.anthropic.seen.lock().unwrap().last().unwrap().body.clone();
    let text = body.to_string();
    assert!(text.contains("\"id\":\"toolu_cc_1\"") && text.contains("\"name\":\"Read\"") && text.contains("\"tool_use_id\":\"toolu_cc_1\""), "Claude Code's call and result, as it made them: {text}");
    assert!(text.contains("# krowk (as Claude Code read it)") && text.contains("claude-code did step 1"));
    // Its thinking was Claude Code's to replay, not the Messages API's:
    // downgraded, never sent with its signature.
    assert!(text.contains("Claude Code thinks about step 1.") && !text.contains("ClaudeCodeSignature1"), "{text}");
}

#[test]
fn r_switch_2_one_session_moves_claude_api_gpt_claude_code_grok_and_back_to_native() {
    let w = World::new("chain");
    let host = w.host();
    let rt = rt();
    let steps = [
        (Eng::Anthropic, "call a tool: we are renaming the tagline — read the README first"),
        (Eng::Openai, "call a tool: patch the tagline to say everything agents make"),
        (Eng::Claude, "call a tool: check the patch landed"),
        (Eng::Xai, "call a tool: tidy the wording"),
        (Eng::Anthropic, "summarize everything that was done"),
    ];
    let mut id: Option<String> = None;
    for (n, (e, text)) in steps.iter().enumerate() {
        let from = w.requests(*e);
        let (r, _) = rt.block_on(run(&host, prompt(id.as_deref(), text, Some(e.model()))));
        assert_eq!(r.status, TurnStatus::Completed, "step {n} on {e:?}: {:?}", r.error);
        let said = answers(&w.events(&r.session_id));
        let seen = w.seen_by(*e, from);
        // Every earlier step reached this one: the task goes on.
        for (m, (_, earlier)) in steps[..n].iter().enumerate() {
            assert!(holds(*e, &seen, earlier) && holds(*e, &seen, &said[m]), "step {n} on {e:?} does not know step {m}:\n{seen}");
        }
        id = Some(r.session_id);
    }
    let id = id.unwrap();
    let events = w.events(&id);
    let models: Vec<String> = events.iter().filter_map(|e| match &e.body {
        LogBody::TurnStarted { model, .. } => Some(model.to_string()),
        _ => None,
    }).collect();
    assert_eq!(models, ["anthropic/claude-sonnet-4-6", "openai/gpt-5.4", "claude:work/haiku", "xai/grok-4.7", "anthropic/claude-sonnet-4-6"]);
    assert_eq!(handoffs(&events), [(HandoffKind::Summary, None, None)], "Claude Code was seeded once, with the two turns before it");
    // What Claude Code was sent is on the record, in the context file.
    let seeded = w.context(&id).into_iter().find_map(|c| c.handoff).unwrap();
    assert!(seeded.contains("anthropic/claude-sonnet-4-6, openai/gpt-5.4") && seeded.ends_with(steps[2].1), "{seeded}");
    // The last native request holds Claude Code's own call and result.
    let last = w.anthropic.seen.lock().unwrap().last().unwrap().body.to_string();
    assert!(last.contains("toolu_cc_1") && last.contains("call_01ApplyPatch") && last.contains("call_01SearchReplace"), "{last}");
}

#[test]
fn r_inst_4_switching_claude_accounts_carries_the_transcript_whole() {
    let mut w = World::new("claude-accounts");
    let personal = w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "step one on work", Some(Eng::Claude.model()))));
    assert_eq!(r1.status, TurnStatus::Completed, "{:?}", r1.error);
    let id = r1.session_id.clone();
    let to = ModelRef { instance: "claude:personal".into(), model: "haiku".into() };
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "step two on personal", Some(to.clone()))));
    assert_eq!(r2.status, TurnStatus::Completed, "{:?}", r2.error);
    // The work account's transcript is now in the personal one's directory,
    // and Claude Code resumed it there: the full context, not a summary.
    let log = w.fake_log("claude-personal");
    assert!(log.contains("resume fa4e0000-0000-4000-8000-000000000001") && log.contains("resume-lines 1"), "{log}");
    assert_eq!(w.claude_prompts("claude-personal"), ["step two on personal"], "nothing but the prompt: the thread holds the rest");
    let slug: String = w.root.join("repo").display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let copied = personal.join("projects").join(&slug).join("fa4e0000-0000-4000-8000-000000000001.jsonl");
    assert!(std::fs::read_to_string(&copied).unwrap().contains("step one on work"), "{}", copied.display());
    assert_eq!(handoffs(&w.events(&id)), [(HandoffKind::Transcript, Some("claude:work".into()), None)]);
}

#[test]
fn r_inst_4_a_transcript_that_cannot_be_carried_falls_back_to_a_summary() {
    let mut w = World::new("claude-fallback");
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "step one on work", Some(Eng::Claude.model()))));
    let id = r1.session_id.clone();
    // The work account's transcript is gone — moved, cleaned up.
    let _ = std::fs::remove_dir_all(w.root.join("claude-work/projects"));
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "step two on personal", Some(ModelRef { instance: "claude:personal".into(), model: "haiku".into() }))));
    assert_eq!(r2.status, TurnStatus::Completed, "{:?}", r2.error);
    let sent = w.claude_prompts("claude-personal");
    assert!(sent[0].starts_with("<handoff from krowk>") && sent[0].contains("step one on work") && sent[0].contains("claude-code did step 1"), "{sent:?}");
    let h = handoffs(&w.events(&id));
    assert!(matches!(&h[..], [(HandoffKind::Summary, None, Some(why))] if why.contains("could not be carried over")), "{h:?}");
}

#[test]
fn r_inst_4_a_thread_claude_code_cannot_resume_is_seeded_fresh() {
    let w = World::new("claude-resume-fails");
    let rt = rt();
    let (r1, _) = rt.block_on(run(&w.host(), prompt(None, "step one", Some(Eng::Claude.model()))));
    let id = r1.session_id.clone();
    // A new host resumes the session; the vendor's transcript is gone.
    let _ = std::fs::remove_dir_all(w.root.join("claude-work/projects"));
    let (r2, _) = rt.block_on(run(&w.host(), prompt(Some(&id), "step two", None)));
    assert_eq!(r2.status, TurnStatus::Completed, "the session is not lost: {:?}", r2.error);
    assert!(w.fake_log("claude-work").contains("resume-missing"));
    let h = handoffs(&w.events(&id));
    assert!(matches!(&h[..], [(HandoffKind::Summary, None, Some(why))] if why.contains("could not resume")), "{h:?}");
}

#[test]
fn r_inst_4_switching_codex_accounts_resumes_the_copied_rollout_by_path() {
    let mut w = World::new("codex-accounts");
    let personal = w.codex("codex:personal", "codex-personal", Some("switch.jsonl"));
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "step one on team", Some(Eng::Codex.model()))));
    assert_eq!(r1.status, TurnStatus::Completed, "{:?}", r1.error);
    let id = r1.session_id.clone();
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "step two on personal", Some(ModelRef { instance: "codex:personal".into(), model: "gpt-5.5".into() }))));
    assert_eq!(r2.status, TurnStatus::Completed, "{:?}", r2.error);
    let rollout = personal.join("sessions/2026/09/25/rollout-2026-09-25T12-00-00-01a0d8df-ba17-70c0-b9b6-9049bdb7a89c.jsonl");
    let log = w.fake_log("codex-personal");
    assert!(log.contains(&format!("resume-lines 1 {}", rollout.display())), "resumed from the copy, by its path: {log}");
    assert_eq!(w.codex_prompts("codex-personal"), ["step two on personal"]);
    assert_eq!(handoffs(&w.events(&id)), [(HandoffKind::Transcript, Some("codex:team".into()), None)]);
}

#[test]
fn r_switch_4_a_switch_to_a_model_without_credentials_is_refused_and_the_session_stays() {
    let mut w = World::new("refused");
    w.instances.push(("anthropic:nokey".into(), InstanceKind::AnthropicApi { api_key_env: Some("UNSET_KEY".into()), base_url: Some(w.anthropic.url.clone()), thinking: None, max_tokens: None, effort: None }));
    w.claude("claude:nologin", "claude-nologin", None, false);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Anthropic.model()))));
    let id = r1.session_id.clone();
    let nokey = ModelRef { instance: "anthropic:nokey".into(), model: "claude-sonnet-4-6".into() };
    // Asked for as a switch: refused before anything is logged.
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: nokey.clone() }, tx)).unwrap_err();
    assert_eq!(e.code, "not_authenticated");
    assert!(e.message.contains("set UNSET_KEY") && e.message.ends_with("the session stays on anthropic/claude-sonnet-4-6"), "{}", e.message);
    // Named on a prompt: the same.
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(prompt(Some(&id), "second", Some(nokey)), tx)).unwrap_err();
    assert!(e.message.ends_with("the session stays on anthropic/claude-sonnet-4-6"), "{}", e.message);
    // A backend instance that is signed out — `claude auth status` says so —
    // is refused by the readiness check the same way, before a Claude
    // process is started for a turn, with the fix and the session kept.
    let nologin = ModelRef { instance: "claude:nologin".into(), model: "haiku".into() };
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: nologin.clone() }, tx)).unwrap_err();
    assert_eq!(e.code, "not_authenticated");
    assert!(e.message.contains("sign in with `krowk providers add claude --name nologin`, which runs Claude's own login") && e.message.ends_with("the session stays on anthropic/claude-sonnet-4-6"), "{}", e.message);
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(prompt(Some(&id), "third", Some(nologin.clone())), tx)).unwrap_err();
    assert!(e.message.contains("krowk providers add claude --name nologin") && e.message.ends_with("the session stays on anthropic/claude-sonnet-4-6"), "{}", e.message);
    let asked = w.fake_log("claude-nologin");
    assert!(asked.contains("argv auth status --json") && !asked.contains("argv -p"), "status asked, no turn process: {asked}");
    // One that fails only once it runs — a Claude Code login gone stale
    // after its status said yes — goes back to the model before, on the
    // record.
    w.claude("claude:stale", "claude-stale", Some("stale_login.jsonl"), true);
    let host = w.host();
    let stale = ModelRef { instance: "claude:stale".into(), model: "haiku".into() };
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "third", Some(stale.clone()))));
    assert_eq!(r2.status, TurnStatus::Failed);
    let err = r2.error.unwrap();
    assert_eq!(err.code, "not_authenticated");
    assert!(err.message.contains("krowk providers add claude --name stale") && err.message.ends_with("the session continues on anthropic/claude-sonnet-4-6"), "{}", err.message);
    let back = w.events(&id).into_iter().find_map(|e| match e.body {
        LogBody::ModelSwitched { from, to, reason, .. } => Some((from, to, reason)),
        _ => None,
    });
    assert_eq!(back, Some((Some(stale), Eng::Anthropic.model(), SwitchReason::SwitchFailed)));
    // The session continues where it was.
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "fourth", None)));
    assert_eq!((r3.status, r3.model), (TurnStatus::Completed, Eng::Anthropic.model()));
    // A good switch is logged, and the next prompt runs there.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: Eng::Xai.model() }, tx)).unwrap();
    let (r4, _) = rt.block_on(run(&host, prompt(Some(&id), "fifth", None)));
    assert_eq!((r4.status, r4.model), (TurnStatus::Completed, Eng::Xai.model()));
    // Without a session it is only a check.
    let (tx, _rx) = mpsc::channel(16);
    assert!(rt.block_on(host.execute(Command::SwitchModel { session_id: None, model: Eng::Openai.model() }, tx)).is_ok());
}

#[test]
fn r_switch_4_a_switch_does_not_escape_the_budget_or_loosen_the_mode() {
    let w = World::new("budget");
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "spend some tokens", Some(Eng::Anthropic.model()))));
    let id = r1.session_id.clone();
    let spent = r1.usage.output_tokens;
    assert!(spent > 0);
    // On another provider the tokens already spent still count.
    let mut cmd = prompt(Some(&id), "and more elsewhere", Some(Eng::Openai.model()));
    if let Command::Prompt { budget, .. } = &mut cmd {
        *budget = Some(BudgetLimits { max_usd: None, max_tokens: Some(spent) });
    }
    let (r2, _) = rt.block_on(run(&host, cmd));
    assert_eq!(r2.error.map(|e| e.code), Some("budget_exceeded".into()));
    // Into Claude Code in plan mode: launched in plan, whatever came before.
    let mut cmd = prompt(Some(&id), "plan it with Claude Code", Some(Eng::Claude.model()));
    if let Command::Prompt { permission_mode, .. } = &mut cmd {
        *permission_mode = PermissionMode::Plan;
    }
    rt.block_on(run(&host, cmd));
    assert!(w.fake_log("claude-work").contains("permission-mode plan"));
}

#[test]
fn r_inst_7_a_limited_instance_offers_the_next_one_and_a_yes_continues_there() {
    let limited = mock::serve(|body, _| {
        let _ = body;
        Reply { headers: vec![("retry-after".into(), "0".into())], ..Reply::json(429, &json!({"type": "error", "error": {"type": "rate_limit_error", "message": "Number of request tokens has exceeded your per-minute rate limit"}})) }
    });
    let mut w = World::with("offer", limited);
    let ok = mock::serve(anthropic_script);
    w.instances.push(("anthropic:personal".into(), InstanceKind::AnthropicApi { api_key_env: Some("TEST_ANTHROPIC_KEY".into()), base_url: Some(ok.url.clone()), thinking: None, max_tokens: None, effort: None }));
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "do the thing", Some(Eng::Anthropic.model()))));
    assert_eq!(r1.status, TurnStatus::Failed);
    let offer = r1.switch_offer.clone().expect("an offer, not a silent move");
    assert_eq!((offer.from.instance.as_str(), offer.to.instance.as_str(), offer.to.model.as_str()), ("anthropic", "anthropic:personal", "claude-sonnet-4-6"));
    assert!(offer.resets_at_ms.is_some(), "retry-after says when");
    let err = r1.error.unwrap();
    assert_eq!(err.code, "rate_limited");
    assert!(err.message.contains(&format!("continue on anthropic:personal/claude-sonnet-4-6 with `krowk -p --resume {} --model anthropic:personal/claude-sonnet-4-6`", r1.session_id)), "{}", err.message);
    assert!(w.events(&r1.session_id).iter().all(|e| !matches!(e.body, LogBody::ModelSwitched { .. })), "offered, never taken by itself");
    // The yes: switch, and the prompt again.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(r1.session_id.clone()), model: offer.to.clone() }, tx)).unwrap();
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "do the thing", None)));
    assert_eq!((r2.status, r2.model.instance.as_str()), (TurnStatus::Completed, "anthropic:personal"));
    // Claude Code's own limit is detected the same way, with when it lifts.
    drop(host);
    drop(w);
    let mut w = World::new("offer-claude");
    w.claude("claude:work", "claude-work", Some("limited.jsonl"), true);
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    let host = w.host();
    let (r, lines) = rt.block_on(run(&host, prompt(None, "do the thing", Some(Eng::Claude.model()))));
    let err = r.error.unwrap();
    assert_eq!((err.code.as_str(), err.resets_at_ms), ("rate_limited", Some(1_790_431_200_000)));
    assert_eq!(r.switch_offer.map(|o| o.to.instance), Some("claude:personal".into()));
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::Limits { instance, limit, .. }) if instance == "claude:work" && limit.status == LimitState::Limited)));
}

#[test]
fn r_inst_8_rollover_auto_moves_on_tells_every_client_and_logs_why() {
    let mut w = World::new("auto");
    w.claude("claude:work", "claude-work", Some("limited.jsonl"), true);
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    w.rollover = Some(Rollover::Auto);
    w.order = vec!["claude:work".into(), "claude:personal".into()];
    let host = w.host();
    let rt = rt();
    let mut cmd = prompt(None, "keep going", Some(Eng::Claude.model()));
    if let Command::Prompt { permission_mode, .. } = &mut cmd {
        *permission_mode = PermissionMode::Plan;
    }
    let (r, lines) = rt.block_on(run(&host, cmd));
    assert_eq!((r.status, r.model.instance.as_str()), (TurnStatus::Completed, "claude:personal"), "{:?}", r.error);
    assert_eq!(r.result, "claude-code did step 1");
    // Every client is told, and the log says from, to and why.
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::Notice { text, .. }) if text.contains("claude:work/haiku is limited until") && text.contains("continues on claude:personal/haiku") && !text.contains("another model"))), "the notice names both, whole: {lines:?}");
    let events = w.events(&r.session_id);
    let switched = events.iter().find_map(|e| match &e.body {
        LogBody::ModelSwitched { from, to, reason, detail, .. } => Some((from.clone(), to.clone(), *reason, detail.clone())),
        _ => None,
    });
    let (from, to, reason, detail) = switched.expect("the rollover is on the record");
    assert_eq!((from.map(|m| m.instance), to.instance.as_str(), reason), (Some("claude:work".into()), "claude:personal", SwitchReason::RateLimited));
    assert!(detail.unwrap().contains("rollover is auto"));
    // The limited turn's thread came along whole, in the same mode.
    assert!(w.fake_log("claude-personal").contains("resume-lines 1") && w.fake_log("claude-personal").contains("permission-mode plan"));
    // And the session stays on the instance it moved to.
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&r.session_id), "and more", None)));
    assert_eq!(r2.model.instance, "claude:personal");
    // Off by default: the same limit only offers.
    w.rollover = None;
    let (r3, _) = rt.block_on(run(&w.host(), prompt(None, "keep going", Some(Eng::Claude.model()))));
    assert_eq!((r3.status, r3.switch_offer.map(|o| o.to.instance)), (TurnStatus::Failed, Some("claude:personal".into())));
}

#[test]
fn r_inst_6_each_instance_reports_how_near_its_limit_it_is() {
    let headers = mock::serve(|body, _| Reply {
        headers: vec![("anthropic-ratelimit-tokens-limit".into(), "1000".into()), ("anthropic-ratelimit-tokens-remaining".into(), "150".into())],
        ..Reply::sse(&mock::text_stream(&format!("anthropic answered: {}", last_prompt(body))))
    });
    let w = World::with("limits", headers);
    let host = w.host();
    let rt = rt();
    let limits = |lines: &[StreamLine]| -> Vec<(String, LimitState, Option<f64>)> {
        lines.iter().filter_map(|l| match l {
            StreamLine::Live(LiveEvent::Limits { instance, limit, .. }) => Some((instance.clone(), limit.status, limit.used_percent.map(f64::round))),
            _ => None,
        }).collect()
    };
    let (r, lines) = rt.block_on(run(&host, prompt(None, "hello", Some(Eng::Anthropic.model()))));
    assert_eq!(limits(&lines), [("anthropic".to_string(), LimitState::Warning, Some(85.0))]);
    let (_, lines) = rt.block_on(run(&host, prompt(Some(&r.session_id), "on claude code", Some(Eng::Claude.model()))));
    assert_eq!(limits(&lines), [("claude:work".to_string(), LimitState::Warning, Some(82.0))]);
    let (_, lines) = rt.block_on(run(&host, prompt(Some(&r.session_id), "on codex", Some(Eng::Codex.model()))));
    assert_eq!(limits(&lines), [("codex:team".to_string(), LimitState::Allowed, Some(35.0))]);
}

#[test]
fn r_switch_4_a_switch_the_picker_made_goes_back_when_its_first_turn_cannot_run() {
    let mut w = World::new("picker-back");
    // Signed in as far as `claude auth status` says, stale once it runs.
    w.claude("claude:nologin", "claude-nologin", Some("stale_login.jsonl"), true);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Anthropic.model()))));
    let id = r1.session_id.clone();
    let nologin = ModelRef { instance: "claude:nologin".into(), model: "haiku".into() };
    // The picker, an offer's yes: switchModel, then a prompt naming no model.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: nologin.clone() }, tx)).unwrap();
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "second", None)));
    assert_eq!((r2.status, r2.model.clone()), (TurnStatus::Failed, nologin.clone()));
    assert!(r2.error.unwrap().message.ends_with("the session continues on anthropic/claude-sonnet-4-6"));
    let back: Vec<(SwitchReason, ModelRef)> = w.events(&id).into_iter().filter_map(|e| match e.body {
        LogBody::ModelSwitched { reason, to, .. } => Some((reason, to)),
        _ => None,
    }).collect();
    assert_eq!(back, [(SwitchReason::Requested, nologin), (SwitchReason::SwitchFailed, Eng::Anthropic.model())]);
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "third", None)));
    assert_eq!((r3.status, r3.model), (TurnStatus::Completed, Eng::Anthropic.model()));
}

#[test]
fn r_inst_8_a_rollover_is_logged_only_once_its_turn_can_start() {
    let mut w = World::new("rollover-preflight");
    w.claude("claude:work", "claude-work", Some("limited.jsonl"), true);
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    w.rollover = Some(Rollover::Auto);
    w.order = vec!["claude:work".into(), "claude:personal".into(), "anthropic/claude-sonnet-4-6".into()];
    // The repository is trusted for the limited turn and the candidate's
    // check, and refused when the candidate's own turn asks: a check that
    // passed is not a turn that starts.
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    w.trust = Some(Arc::new(move |root: &Path| {
        if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 { Err(krowk_harness::trust::untrusted(root, "refused for the test")) } else { Ok(()) }
    }));
    let host = w.host();
    let rt = rt();
    let (r, lines) = rt.block_on(run(&host, prompt(None, "keep going", Some(Eng::Claude.model()))));
    // claude:personal could not start, so the session went on to the next.
    assert_eq!((r.status, r.model.clone()), (TurnStatus::Completed, Eng::Anthropic.model()), "{:?}", r.error);
    let switched: Vec<(ModelRef, SwitchReason, String)> = w.events(&r.session_id).into_iter().filter_map(|e| match e.body {
        LogBody::ModelSwitched { to, reason, detail, .. } => Some((to, reason, detail.unwrap_or_default())),
        _ => None,
    }).collect();
    assert_eq!(switched.len(), 1, "one move on the record, to where the turn ran: {switched:?}");
    assert_eq!((switched[0].0.clone(), switched[0].1), (Eng::Anthropic.model(), SwitchReason::RateLimited));
    assert!(switched[0].2.contains("claude:work/haiku is limited") && switched[0].2.contains("another model: claude-sonnet-4-6 instead of haiku"), "a model change is said: {}", switched[0].2);
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::Notice { text, .. }) if text.contains("another model"))));
    // With nowhere left that can start, the limit's own result stands and
    // nothing moved.
    drop(host);
    calls.store(0, std::sync::atomic::Ordering::SeqCst);
    w.order = vec!["claude:work".into(), "claude:personal".into()];
    let host = w.host();
    let (r, _) = rt.block_on(run(&host, prompt(None, "keep going", Some(Eng::Claude.model()))));
    assert_eq!((r.status, r.model.instance.as_str()), (TurnStatus::Failed, "claude:work"));
    let e = r.error.unwrap();
    assert_eq!(e.code, "rate_limited");
    assert!(e.message.contains("rollover to claude:personal/haiku could not start"), "{}", e.message);
    assert!(w.events(&r.session_id).iter().all(|e| !matches!(e.body, LogBody::ModelSwitched { .. })), "never logged");
}

#[test]
fn r_inst_7_the_models_own_words_are_not_a_rate_limit() {
    let mut w = World::new("says-limit");
    w.claude("claude:work", "claude-work", Some("says_limit.jsonl"), true);
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    let (r, _) = rt().block_on(run(&w.host(), prompt(None, "say you are limited", Some(Eng::Claude.model()))));
    assert_eq!(r.error.map(|e| e.code), Some("backend_failed".into()));
    assert!(r.switch_offer.is_none(), "no offer on what the model said");
}

#[test]
fn r_switch_4_a_switch_during_a_turn_is_kept_for_after_it() {
    let mut w = World::new("during");
    w.claude("claude:work", "claude-work", Some("slow.jsonl"), true);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Anthropic.model()))));
    let id = r1.session_id.clone();
    let (r2, switched) = rt.block_on(async {
        let turn = run(&host, prompt(Some(&id), "slowly", Some(Eng::Claude.model())));
        let switch = async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let (tx, _rx) = mpsc::channel(16);
            host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: Eng::Xai.model() }, tx).await
        };
        tokio::join!(turn, switch)
    });
    assert!(switched.is_ok(), "{switched:?}");
    assert_eq!(r2.0.status, TurnStatus::Completed);
    // Right after the turn, from the same host: no lock in the way.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: Eng::Openai.model() }, tx)).unwrap();
    let to: Vec<ModelRef> = w.events(&id).into_iter().filter_map(|e| match e.body {
        LogBody::ModelSwitched { to, .. } => Some(to),
        _ => None,
    }).collect();
    assert_eq!(to, [Eng::Xai.model(), Eng::Openai.model()]);
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "next", None)));
    assert_eq!(r3.model, Eng::Openai.model());
}

#[test]
fn r_switch_4_a_switch_during_a_fan_out_moves_the_parent_and_leaves_its_subagent_on_its_model() {
    let w = World::new("fan-out");
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "hello", Some(Eng::Anthropic.model()))));
    let id = r1.session_id.clone();
    let (r2, switched) = rt.block_on(async {
        let turn = run(&host, prompt(Some(&id), "fan out to a subagent", None));
        let switch = async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let (tx, _rx) = mpsc::channel(16);
            host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: Eng::Xai.model() }, tx).await
        };
        tokio::join!(turn, switch)
    });
    assert!(switched.is_ok(), "{switched:?}");
    assert_eq!(r2.0.status, TurnStatus::Completed, "{:?}", r2.0.error);
    let events = w.events(&id);
    let child = events.iter().find_map(|e| match &e.body {
        LogBody::SubagentStarted { subagent_session_id, model, .. } => Some((subagent_session_id.clone(), model.clone())),
        _ => None,
    });
    let (child, child_model) = child.expect("the fan-out ran");
    assert_eq!(child_model, Eng::Anthropic.model(), "the subagent ran on the model it was started on");
    assert!(w.events(&child).iter().all(|e| !matches!(e.body, LogBody::ModelSwitched { .. })), "its log records no switch");
    let to: Vec<ModelRef> = events.iter().filter_map(|e| match &e.body {
        LogBody::ModelSwitched { to, .. } => Some(to.clone()),
        _ => None,
    }).collect();
    assert_eq!(to, [Eng::Xai.model()], "the parent's switch, after its turn");
    // A subagent's session is not one to switch.
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(Command::SwitchModel { session_id: Some(child.clone()), model: Eng::Openai.model() }, tx)).unwrap_err();
    assert_eq!(e.code, "subagent_session");
    // The parent's next turn is on Grok, and reads the subagent's call and result.
    let from = w.requests(Eng::Xai);
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "what did the subagent say?", None)));
    assert_eq!((r3.status, r3.model), (TurnStatus::Completed, Eng::Xai.model()));
    let seen = w.seen_by(Eng::Xai, from);
    assert!(seen.contains("toolu_sub") && seen.contains("child did it"), "{seen}");
}

#[test]
fn readiness_a_vendors_signed_in_is_believed_for_a_minute_and_a_signed_out_asked_every_time() {
    let mut w = World::new("readiness-cache");
    w.claude("claude:in", "claude-in", None, true);
    let out_dir = w.claude("claude:out", "claude-out", None, false);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Anthropic.model()))));
    let id = r1.session_id.clone();
    let asked = |dir: &str| w.fake_log(dir).lines().filter(|l| *l == "argv auth status --json").count();
    let switch = |instance: &str| {
        let (tx, _rx) = mpsc::channel(16);
        rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: ModelRef { instance: instance.into(), model: "haiku".into() } }, tx))
    };
    // Signed in: asked once, then believed — a long-lived host (the TUI)
    // does not spawn Claude Code's status for every switch.
    switch("claude:in").unwrap();
    switch(&Eng::Anthropic.model().instance).unwrap();
    switch("claude:in").unwrap();
    assert_eq!(asked("claude-in"), 1);
    // Signed out: asked every time, so signing in elsewhere and trying again
    // works at once.
    assert_eq!(switch("claude:out").unwrap_err().code, "not_authenticated");
    std::fs::write(out_dir.join("fake-login"), "").unwrap();
    switch("claude:out").unwrap();
    assert_eq!(asked("claude-out"), 2);
}
