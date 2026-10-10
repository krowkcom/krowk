//! Subagents and todos, end to end against a stand-in Anthropic API: three
//! subagents fanned out in parallel, one interrupted alone while the TUI
//! draws each as a line; only their summaries back in the parent's
//! context; a child's spend stopping the parent at `--max-usd`; a Claude
//! Code agent definition used as it is; the todo list across `--resume`;
//! and `krowk sessions rebuild` restoring the tree from the logs alone.

#![cfg(feature = "harness")]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{Command, LiveEvent, LogBody, PermissionMode, StreamLine, TurnStatus, Usage};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

struct Sandbox {
    root: PathBuf,
    url: String,
}

impl Sandbox {
    fn new(name: &str, url: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-subagents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        // Long enough that a child which reads it has a context many times
        // the size of anything that comes back to its parent.
        let readme: String = (0..400).map(|i| format!("Line {i}: krowk turns agent output into permalinks you can paste anywhere.\n")).collect();
        std::fs::write(root.join("repo/README.md"), format!("# krowk\n\n{readme}")).unwrap();
        // The fake `claude` and `codex`, signed in to nothing, first on
        // PATH: routing asks every vendor there is, and never the real ones
        // the machine may have.
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

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "ANTHROPIC_API_KEY" => "sk-test".into(),
            "ANTHROPIC_BASE_URL" => self.url.clone(),
            _ => String::new(),
        }
    }

    fn krowk(&self, args: &[&str]) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("ANTHROPIC_API_KEY", "sk-test")
            .env("ANTHROPIC_BASE_URL", &self.url)
            .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn sessions(&self) -> PathBuf {
        self.root.join("home/.krowk/sessions")
    }

    fn log(&self, session: &str) -> Vec<Value> {
        std::fs::read_to_string(self.sessions().join(session).join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// The host `krowk` builds, in-process, so a test can interrupt one
    /// subagent while the others run.
    fn host(&self) -> HostConfig {
        HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.root.join("repo"),
            registry: Registry::resolve(&InstancesConfig::default(), &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(sonnet),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.krowk/credentials.json"),
            trust: krowk_harness::trust::allow_all(),
            publisher: None,
            // The sandbox's own home: the person's real settings stay out.
            permissions: krowk_harness::permissions::Config { home: Some(self.root.join("home")), krowk_dir: Some(self.root.join("home/.krowk")), ..Default::default() },
            agents: krowk_harness::subagent::AgentsConfig::none(),
            session: Default::default(),
        }
    }

    /// A models.dev cache in the sandbox's home, as `krowk pricing refresh`
    /// leaves it: what a subagent's model alias is resolved against.
    fn catalog(&self) {
        let dir = self.root.join("home/.krowk/cache");
        std::fs::create_dir_all(&dir).unwrap();
        let model = |family: &str, released: &str, input: f64, output: f64| {
            json!({"family": family, "tool_call": true, "release_date": released, "modalities": {"input": ["text"], "output": ["text"]}, "cost": {"input": input, "output": output, "cache_read": input / 10.0, "cache_write": input * 1.25}, "limit": {"context": 200000, "output": 64000}})
        };
        let doc = json!({"anthropic": {"id": "anthropic", "npm": "@ai-sdk/anthropic", "models": {
            "claude-sonnet-4-6": model("claude-sonnet", "2026-02-17", 3.0, 15.0),
            "claude-haiku-4-5": model("claude-haiku", "2025-10-15", 1.0, 5.0),
        }}});
        std::fs::write(dir.join("models.json"), doc.to_string()).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Sonnet's list prices, per million tokens, for every model.
fn sonnet(_: &str, _: &str, u: &Usage) -> Option<f64> {
    Some((u.input_tokens as f64 * 3.0 + u.output_tokens as f64 * 15.0 + u.cache_read_tokens as f64 * 0.3 + u.cache_write_tokens as f64 * 3.75) / 1e6)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// One Messages-API response calling several tools at once.
fn tool_calls(calls: &[(&str, &str, Value)]) -> String {
    let mut out = format!(
        "event: message_start\ndata: {}\n\n",
        json!({"type": "message_start", "message": {"id": "msg_calls", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}})
    );
    for (i, (id, name, input)) in calls.iter().enumerate() {
        out += &format!("event: content_block_start\ndata: {}\n\n", json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        out += &format!("event: content_block_delta\ndata: {}\n\n", json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
        out += &format!("event: content_block_stop\ndata: {}\n\n", json!({"type": "content_block_stop", "index": i}));
    }
    out + "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":40}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
}

fn is_child(body: &Value) -> bool {
    body["system"].to_string().contains("You are a subagent")
}

/// The first prompt of the conversation the request carries.
fn task(body: &Value) -> String {
    body["messages"][0].to_string()
}

fn answered(body: &Value) -> bool {
    body["messages"].as_array().and_then(|m| m.last()).and_then(|m| m["content"].as_array()).is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"))
}

/// Every `tool_result` block of the request's last message, by call id.
fn results(body: &Value) -> Vec<(String, String, bool)> {
    let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
    last["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            let content = match &b["content"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (b["tool_use_id"].as_str().unwrap_or_default().to_string(), content, b["is_error"].as_bool().unwrap_or(false))
        })
        .collect()
}

/// Four bytes a token, krowk's own estimate.
fn tokens(v: &Value) -> usize {
    v.to_string().len().div_ceil(4)
}

/// The parent fans out three subagents; A reads the README and sums it up,
/// B writes slowly until it is interrupted, C answers at once.
fn fan_out(body: &Value, _: usize) -> mock::Reply {
    if !is_child(body) {
        if answered(body) {
            return mock::Reply::sse(&mock::text_stream("All three reported."));
        }
        return mock::Reply::sse(&tool_calls(&[
            ("toolu_a", "subagent", json!({"description": "summarise the readme", "prompt": "TASK-A: read README.md and summarise it"})),
            ("toolu_b", "subagent", json!({"description": "write an essay", "prompt": "TASK-B: write a long essay"})),
            ("toolu_c", "subagent", json!({"description": "say hello", "prompt": "TASK-C: say hello"})),
        ]));
    }
    let t = task(body);
    if t.contains("TASK-A") {
        if answered(body) {
            return mock::Reply::sse(&mock::text_stream("SUMMARY-A: krowk makes permalinks of agent output."));
        }
        return mock::Reply::sse(&mock::tool_use("toolu_read", "read", &json!({"path": "README.md"})));
    }
    if t.contains("TASK-B") {
        return mock::Reply::paced(mock::text_stream(&"word ".repeat(400)), Duration::from_millis(25));
    }
    mock::Reply::sse(&mock::text_stream("SUMMARY-C: hello."))
}

/// The text of the TUI's rows, as a terminal would show them.
macro_rules! row_text {
    ($rows:expr) => {
        $rows.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<String>>()
    };
}

#[test]
fn r_sub_1_r_sub_2_r_sub_3_three_subagents_run_in_parallel_each_on_a_line_and_one_is_interrupted_alone() {
    let m = mock::serve(fan_out);
    let b = Sandbox::new("fan-out", &m.url);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let host = Host::new(b.host());
    let mut app = krowk_tui::app::App::new(krowk_tui::editor::Editor::new(None), 160, krowk_tui::settings::Settings::default(), None, None);
    let mut lines: Vec<StreamLine> = Vec::new();
    let mut most_lines = 0;
    let mut essay: Option<String> = None;
    let result = rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let cmd = Command::Prompt { session_id: None, text: "look into three things at once".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        app.start_turn(std::time::Instant::now());
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let mut interrupted = false;
        let mut others: Vec<String> = Vec::new();
        let mut others_done = 0;
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    app.on_line(&line);
                    let shown = row_text!(app.view(std::time::Instant::now()).0).iter().filter(|r| r.contains("Agent ")).count();
                    most_lines = most_lines.max(shown);
                    if let StreamLine::Log(ev) = &line
                        && let LogBody::SubagentStarted { subagent_session_id, description, .. } = &ev.body
                    {
                        if description == "write an essay" {
                            essay = Some(subagent_session_id.clone());
                        } else {
                            others.push(subagent_session_id.clone());
                        }
                    }
                    if let StreamLine::Log(ev) = &line
                        && matches!(ev.body, LogBody::TurnCompleted { .. })
                        && others.contains(&ev.session_id)
                    {
                        others_done += 1;
                    }
                    // B is streaming and A and C are done: interrupt it, and
                    // it alone. Waiting for A and C keeps a slow runner from
                    // ending B before A's second round-trip lands.
                    if !interrupted
                        && others_done == 2
                        && let StreamLine::Live(LiveEvent::ItemDelta { session_id, .. }) = &line
                        && essay.as_deref() == Some(session_id.as_str())
                    {
                        let (itx, _irx) = tokio::sync::mpsc::channel(1);
                        host.execute(Command::Interrupt { session_id: session_id.clone() }, itx).await.expect("the child's turn is running");
                        interrupted = true;
                    }
                    lines.push(line);
                }
                r = &mut exec => break r,
            }
        };
        while let Ok(line) = rx.try_recv() {
            app.on_line(&line);
            lines.push(line);
        }
        result
    });
    let result = result.unwrap().unwrap();
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.error);
    assert_eq!(result.result, "All three reported.");
    let parent = result.session_id.clone();
    let essay = essay.expect("B was started");

    // R-SUB-2: in parallel — all three children started before any ended,
    // and A and C finished while B was still writing.
    let started: Vec<(usize, String)> = lines.iter().enumerate().filter_map(|(i, l)| match l {
        StreamLine::Log(ev) if matches!(&ev.body, LogBody::SessionStarted { parent_session_id: Some(p), .. } if *p == parent) => Some((i, ev.session_id.clone())),
        _ => None,
    }).collect();
    assert_eq!(started.len(), 3, "three child sessions");
    let ended = |sid: &str| lines.iter().position(|l| matches!(l, StreamLine::Log(ev) if ev.session_id == sid && matches!(ev.body, LogBody::TurnCompleted { .. }))).unwrap();
    let first_end = started.iter().map(|(_, s)| ended(s)).min().unwrap();
    assert!(started.iter().all(|(i, _)| *i < first_end), "every child started before the first ended");
    let others: Vec<&String> = started.iter().map(|(_, s)| s).filter(|s| **s != essay).collect();
    assert!(others.iter().all(|s| ended(s) < ended(&essay)), "A and C finished while B ran");
    // R-SUB-3: each drew its own line in the TUI while it ran.
    assert_eq!(most_lines, 3, "three subagent lines at once");
    let done = row_text!(app.take_pending());
    for d in ["Agent summarise the readme", "Agent write an essay", "Agent say hello"] {
        assert!(done.iter().any(|l| l.contains(d)), "{d} is in scrollback: {done:?}");
    }
    assert!(done.iter().any(|l| l.contains("Agent write an essay") && l.contains("interrupted")), "{done:?}");

    // One interrupted, the others untouched.
    let status = |sid: &str| b.log(sid).iter().rev().find(|e| e["type"] == "turn.completed").unwrap()["status"].as_str().unwrap().to_string();
    assert_eq!(status(&essay), "interrupted");
    assert!(others.iter().all(|s| status(s) == "completed"));

    // R-SUB-1: only the summaries came back — the parent's context grew by
    // its three calls and three short results, while A alone read the
    // whole README into its own.
    let seen = m.seen.lock().unwrap();
    let parents: Vec<&Value> = seen.iter().map(|s| &s.body).filter(|b| !is_child(b)).collect();
    assert_eq!(parents.len(), 2);
    let back = results(parents[1]);
    let by = |id: &str| back.iter().find(|(c, _, _)| c == id).cloned().unwrap();
    assert_eq!(by("toolu_a"), ("toolu_a".into(), "SUMMARY-A: krowk makes permalinks of agent output.".into(), false));
    assert!(by("toolu_b").2 && by("toolu_b").1.starts_with("the subagent was interrupted"), "{:?}", by("toolu_b"));
    assert_eq!(by("toolu_c").1, "SUMMARY-C: hello.");
    let grew = tokens(&parents[1]["messages"]) - tokens(&parents[0]["messages"]);
    let child_a = seen.iter().map(|s| &s.body).rfind(|b| is_child(b) && task(b).contains("TASK-A")).unwrap();
    let a_context = tokens(&child_a["messages"]);
    println!("R-SUB-1 context tokens: the parent grew by {grew}; subagent A's context is {a_context}");
    assert!(grew < 1_000, "the parent grew by {grew} tokens");
    assert!(a_context > 10 * grew, "A's context ({a_context} tokens) stayed A's");
    assert!(!parents[1].to_string().contains("Line 399: krowk turns"), "the README A read never reached the parent");
    // Each child has its own context: its prompt and nothing of the parent's.
    assert_eq!(child_a["messages"][0]["content"][0]["text"], "TASK-A: read README.md and summarise it");
    assert!(!child_a["tools"].as_array().unwrap().iter().any(|t| t["name"] == "subagent"), "subagents start none of their own");
    drop(seen);

    // The parent's log links each child, which names the parent back.
    let plog = b.log(&parent);
    let links: Vec<&str> = plog.iter().filter(|e| e["type"] == "subagent.started").map(|e| e["subagentSessionId"].as_str().unwrap()).collect();
    assert_eq!(links.len(), 3);
    for (_, s) in &started {
        assert!(links.contains(&s.as_str()));
        assert_eq!(b.log(s)[0]["parentSessionId"], parent.as_str());
    }

}

/// The parent starts two subagents: A reads the README and sums it up, B
/// answers with words of its own.
fn two_children(body: &Value, _: usize) -> mock::Reply {
    if !is_child(body) {
        if answered(body) {
            return mock::Reply::sse(&mock::text_stream("PARENT-DONE: both reported."));
        }
        return mock::Reply::sse(&tool_calls(&[
            ("toolu_a", "subagent", json!({"description": "summarise the readme", "prompt": "TASK-A: read README.md and summarise it"})),
            ("toolu_b", "subagent", json!({"description": "say hello", "prompt": "TASK-B: say hello"})),
        ]));
    }
    if task(body).contains("TASK-A") {
        if answered(body) {
            return mock::Reply::paced(mock::text_stream("SUMMARY-A: krowk makes permalinks.\nSECOND-A: of agent output.\n"), Duration::from_millis(5));
        }
        return mock::Reply::sse(&mock::tool_use("toolu_read", "read", &json!({"path": "README.md"})));
    }
    mock::Reply::paced(mock::text_stream("SIBLING-B: hello from B.\n"), Duration::from_millis(5))
}

/// The child view's rows, tall enough to hold a child's whole transcript.
fn child_rows(app: &mut krowk_tui::app::App) -> Vec<String> {
    row_text!(app.child.as_mut().expect("the child view is open").rows(120, 400, None))
}

/// R-SUB-11: Enter on a child in the Agents overlay opens its transcript —
/// its log read at once, the parent's prompt first, then its tool call and
/// text as they arrive, and nothing of its sibling — while the main App
/// goes on drawing the conversation. Resumed later, a finished child opens
/// the same way, whole, from its log.
#[test]
fn r_sub_11_a_childs_transcript_opens_from_its_log_and_follows_it_live_and_whole_after_a_resume() {
    let m = mock::serve(two_children);
    let b = Sandbox::new("child-view", &m.url);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let host = Host::new(b.host());
    let new_app = || krowk_tui::app::App::new(krowk_tui::editor::Editor::new(None), 120, krowk_tui::settings::Settings::default(), None, None);
    let read = |sid: &str| log::read_events(&b.sessions().join(sid).join(log::EVENTS_FILE)).map_err(|e| e.message().to_string());
    let mut app = new_app();
    let mut a: Option<String> = None;
    // What the view showed when A's call came back, and when its first line
    // of text did: before its turn ended.
    let (mut at_result, mut at_text): (Option<Vec<String>>, Option<Vec<String>>) = (None, None);
    let result = rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let cmd = Command::Prompt { session_id: None, text: "look into two things at once".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        app.start_turn(std::time::Instant::now());
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let mut on = |app: &mut krowk_tui::app::App, line: &StreamLine| {
            app.on_line(line);
            if let StreamLine::Log(ev) = line
                && let LogBody::SubagentStarted { subagent_session_id, description, .. } = &ev.body
                && description == "summarise the readme"
            {
                // Ctrl-G, down to A, Enter.
                app.open_agents();
                while app.agent_selected().as_deref() != Some(subagent_session_id.as_str()) {
                    app.agent_move(1);
                }
                app.open_child(subagent_session_id, read(subagent_session_id));
                a = Some(subagent_session_id.clone());
            }
            if let StreamLine::Log(ev) = line
                && Some(&ev.session_id) == a.as_ref()
                && let LogBody::ItemCompleted { item: krowk_harness::protocol::Item::ToolResult { .. }, .. } = &ev.body
            {
                at_result = Some(child_rows(app));
            }
            if at_text.is_none()
                && let StreamLine::Live(LiveEvent::ItemDelta { session_id, .. }) = line
                && Some(session_id) == a.as_ref()
                && child_rows(app).iter().any(|r| r.contains("SUMMARY-A"))
            {
                at_text = Some(child_rows(app));
            }
        };
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => on(&mut app, &line),
                r = &mut exec => break r,
            }
        };
        while let Ok(line) = rx.try_recv() {
            on(&mut app, &line);
        }
        result
    });
    let result = result.unwrap().unwrap();
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.error);
    let a = a.expect("A was opened");

    // Its prompt first, then its call, then its text as it came, before the
    // turn ended; and nothing of B.
    let at_result = at_result.expect("A's call came back with the view open");
    let prompt = at_result.iter().position(|r| r.contains("TASK-A: read README.md and summarise it")).unwrap_or_else(|| panic!("the prompt is shown: {at_result:#?}"));
    let read_call = at_result.iter().position(|r| r.contains("Read README.md")).unwrap_or_else(|| panic!("the call is shown: {at_result:#?}"));
    assert!(prompt < read_call, "the prompt first: {at_result:#?}");
    assert_eq!(at_result.iter().filter(|r| r.contains("TASK-A")).count(), 1, "once, though it was read and sent too: {at_result:#?}");
    let at_text = at_text.expect("A's text was shown as it streamed");
    assert!(!at_text.iter().any(|r| r.contains("Worked for")), "before A's turn ended: {at_text:#?}");
    let all = child_rows(&mut app);
    let summary = all.iter().position(|r| r.contains("SUMMARY-A")).unwrap();
    assert!(read_call < summary && all.iter().any(|r| r.contains("SECOND-A")), "{all:#?}");
    assert!(!all.iter().any(|r| r.contains("TASK-B") || r.contains("SIBLING-B") || r.contains("PARENT-DONE")), "nothing of the sibling or the parent: {all:#?}");

    // Closed, the conversation has everything that happened meanwhile.
    app.child = None;
    let done = row_text!(app.take_pending());
    for d in ["Agent summarise the readme", "Agent say hello", "PARENT-DONE: both reported."] {
        assert!(done.iter().any(|l| l.contains(d)), "{d} is in scrollback: {done:#?}");
    }
    assert!(!done.iter().any(|l| l.contains("SUMMARY-A")), "a child's text is its own: {done:#?}");

    // Resumed: A, finished, opens whole from its log.
    let parent = read(&result.session_id).unwrap();
    let head = parent.last().unwrap().id.clone();
    let mut app = new_app();
    app.replay(&log::branch(&parent, &head));
    app.end_replayed_children(true);
    app.open_agents();
    while app.agent_selected().as_deref() != Some(a.as_str()) {
        app.agent_move(1);
    }
    app.open_child(&a, read(&a));
    let all = child_rows(&mut app);
    let at = |s: &str| all.iter().position(|r| r.contains(s)).unwrap_or_else(|| panic!("{s} is shown: {all:#?}"));
    assert!(at("TASK-A: read README.md") < at("Read README.md") && at("Read README.md") < at("SUMMARY-A") && at("SUMMARY-A") < at("SECOND-A") && at("SECOND-A") < at("Worked for"), "{all:#?}");
    assert!(all[0].contains("summarise the readme"), "titled by its description: {all:#?}");
}

/// The parent starts one subagent that rereads the README, each call
/// reading 20,000 tokens from cache — about $0.0066 a call at Sonnet's
/// prices.
fn costly_child(body: &Value, n: usize) -> mock::Reply {
    if !is_child(body) {
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        return mock::Reply::sse(&tool_calls(&[("toolu_spend", "subagent", json!({"description": "reread the readme", "prompt": "TASK-S: read README.md until told to stop"}))]));
    }
    let call = mock::tool_use(&format!("toolu_{n:02}"), "read", &json!({"path": "README.md"}));
    mock::Reply::sse(&call.replace("\"cache_read_input_tokens\":0", "\"cache_read_input_tokens\":20000"))
}

#[test]
fn r_sub_4_a_childs_spend_counts_toward_the_parents_max_usd() {
    let m = mock::serve(costly_child);
    let b = Sandbox::new("budget", &m.url);
    // Half a cent: the parent's first call and the child's first fit; the
    // child's is over it, so the child's second call is refused, and so is
    // the parent's next.
    let out = b.krowk(&["-p", "reread the readme in a subagent", "--model", "claude-sonnet-4-6", "--max-usd", "0.005", "--output-format", "json"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    let result: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let parent = result["sessionId"].as_str().unwrap();
    assert_eq!(result["error"]["code"], "budget_exceeded");
    assert!(stderr.contains(&format!("krowk -p --resume {parent} --max-usd")), "the fix names the session the limit was set on: {stderr}");
    let seen = m.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "the parent's call and the child's first; neither's next was sent");
    assert!(is_child(&seen[1].body));
    drop(seen);
    let plog = b.log(parent);
    let child = plog.iter().find(|e| e["type"] == "subagent.started").unwrap()["subagentSessionId"].as_str().unwrap().to_string();
    let clog = b.log(&child);
    let end = clog.iter().rev().find(|e| e["type"] == "turn.completed").unwrap();
    assert_eq!((end["status"].as_str(), end["error"]["code"].as_str()), (Some("failed"), Some("budget_exceeded")), "the child was held to its parent's limit");
    let call = plog.iter().find(|e| e["item"]["kind"] == "toolResult").unwrap();
    assert_eq!(call["item"]["isError"], true);
    assert!(call["item"]["output"].as_str().unwrap().contains("over --max-usd"), "{call}");
    // `krowk -p` listed the child under its parent straight away.
    let env = b.env();
    let conn = krowk_store::open(&env).unwrap();
    let rows = krowk_store::list_sessions(&conn, "krowk", "", 20).unwrap();
    let id = |f: &str| rows.iter().find(|r| r.foreign_session_id == f).unwrap().id.clone();
    assert_eq!(krowk_store::descendant_session_ids(&conn, &id(parent)).unwrap(), [id(&child)]);
}

/// The parent asks the `reviewer` agent; the reviewer answers at once.
fn review(body: &Value, _: usize) -> mock::Reply {
    if is_child(body) {
        return mock::Reply::sse(&mock::text_stream("REVIEW-OK: the README is fine."));
    }
    if answered(body) {
        return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
    }
    mock::Reply::sse(&tool_calls(&[("toolu_rev", "subagent", json!({"description": "review the readme", "prompt": "TASK-R: review README.md", "agent": "reviewer"}))]))
}

#[test]
fn r_sub_5_a_claude_code_agent_definition_is_used_as_is() {
    let m = mock::serve(review);
    let b = Sandbox::new("definition", &m.url);
    b.catalog();
    std::fs::create_dir_all(b.root.join("repo/.claude/agents")).unwrap();
    std::fs::write(
        b.root.join("repo/.claude/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews files for mistakes. Use after every edit.\ntools: Read, Grep, WebFetch\nmodel: haiku\ncolor: green\n---\n\nREVIEWER-INSTRUCTIONS: read every file whole before you judge it.\n",
    )
    .unwrap();
    let out = b.krowk(&["-p", "have the reviewer look at the README", "--model", "claude-sonnet-4-6", "--output-format", "json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = m.seen.lock().unwrap();
    let parent_tools = &seen[0].body["tools"];
    let subagent = parent_tools.as_array().unwrap().iter().find(|t| t["name"] == "subagent").unwrap();
    assert!(subagent["description"].as_str().unwrap().contains("- reviewer: Reviews files for mistakes. Use after every edit."), "the parent's model is told of it: {subagent}");
    let child = seen.iter().map(|s| &s.body).find(|b| is_child(b)).expect("the reviewer ran");
    assert_eq!(child["model"], "claude-haiku-4-5", "`model: haiku` is the newest Haiku the catalog lists");
    assert!(child["system"].to_string().contains("REVIEWER-INSTRUCTIONS: read every file whole"), "{}", child["system"]);
    let names: Vec<&str> = child["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["read", "grep"], "Read and Grep; WebFetch is not krowk's to offer");
    let parent = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let started = b.log(&parent).into_iter().find(|e| e["type"] == "subagent.started").unwrap();
    assert_eq!((started["agent"].as_str(), started["model"]["model"].as_str()), (Some("reviewer"), Some("claude-haiku-4-5")));
    assert_eq!(b.log(started["subagentSessionId"].as_str().unwrap())[0]["agent"], "reviewer");
}

/// A model that writes a todo list on its first call and answers after.
fn planning(body: &Value, _: usize) -> mock::Reply {
    let prompts = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user" && m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "text"))).count();
    if answered(body) || prompts > 1 {
        return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
    }
    let todos = json!({"todos": [{"content": "read the README", "status": "completed"}, {"content": "fix the typo", "status": "in_progress"}, {"content": "run the tests", "status": "pending"}]});
    mock::Reply::sse(&mock::tool_use("toolu_plan", "todo_write", &todos))
}

#[test]
fn r_todo_1_r_todo_2_the_todo_list_lives_in_the_log_and_survives_resume() {
    let m = mock::serve(planning);
    let b = Sandbox::new("todos", &m.url);
    let out = b.krowk(&["-p", "plan the fix", "--model", "claude-sonnet-4-6", "--output-format", "json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let session = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let updated: Vec<Value> = b.log(&session).into_iter().filter(|e| e["type"] == "todos.updated").collect();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["todos"][1], json!({"content": "fix the typo", "status": "in_progress"}));
    let answer = &results(&m.seen.lock().unwrap()[1].body)[0];
    assert_eq!(answer.1, "Todos updated: 1 in progress, 1 pending, 1 completed.");

    let again = b.krowk(&["-p", "--resume", &session, "go on", "--output-format", "json"]);
    assert!(again.status.success(), "{}", text(&again.stderr));
    // The resumed turn's model reads the list back in its own history…
    let seen = m.seen.lock().unwrap();
    let resumed = seen.last().unwrap().body.to_string();
    assert!(resumed.contains("todo_write") && resumed.contains("fix the typo"), "{resumed}");
    drop(seen);
    // …and a client resuming it shows it: the TUI's overlay, from the log.
    let events = log::read_events(&b.sessions().join(&session).join(log::EVENTS_FILE)).unwrap();
    let head = events.last().unwrap().id.clone();
    let mut app = krowk_tui::app::App::new(krowk_tui::editor::Editor::new(None), 80, krowk_tui::settings::Settings::default(), None, None);
    app.replay(&log::branch(&events, &head));
    app.overlay = krowk_tui::app::Overlay::Todos;
    let rows = row_text!(app.view(std::time::Instant::now()).0);
    assert_eq!(&rows[..3], ["☒ read the README", "◐ fix the typo", "☐ run the tests"]);
    assert!(app.status_bar().contains("[2 tasks]"), "the open ones: {}", app.status_bar());
}

/// The session tree as `krowk sessions` lists it: each listed child of the
/// parent's row, by its krowk session id.
fn listed_children(b: &Sandbox, parent: &str) -> Vec<String> {
    let env = b.env();
    let conn = krowk_store::open(&env).unwrap();
    let rows = krowk_store::list_sessions(&conn, "krowk", "", 50).unwrap();
    let id = |f: &str| rows.iter().find(|r| r.foreign_session_id == f).map(|r| r.id.clone()).unwrap_or_else(|| panic!("{f} is listed"));
    let mut kids: Vec<String> = krowk_store::descendant_session_ids(&conn, &id(parent))
        .unwrap()
        .into_iter()
        .map(|sid| rows.iter().find(|r| r.id == sid).unwrap().foreign_session_id.clone())
        .collect();
    kids.sort();
    kids
}

fn children_of(b: &Sandbox, parent: &str) -> Vec<String> {
    let mut kids: Vec<String> = b.log(parent).iter().filter(|e| e["type"] == "subagent.started").map(|e| e["subagentSessionId"].as_str().unwrap().to_string()).collect();
    kids.sort();
    kids
}

#[test]
fn r_sub_6_sessions_rebuild_restores_parent_and_children_from_the_logs_alone() {
    let m = mock::serve(fan_three(Duration::ZERO));
    let b = Sandbox::new("rebuild", &m.url);
    let out = b.krowk(&["-p", "three at once", "--model", "claude-sonnet-4-6", "--output-format", "json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let parent = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let kids = children_of(&b, &parent);
    assert_eq!(kids.len(), 3);
    assert_eq!(listed_children(&b, &parent), kids, "`krowk -p` lists them under it at once");
    // The store, gone; the logs, all there is.
    std::fs::remove_file(b.root.join("home/.krowk/sessions/krowk.db")).unwrap();
    let out = b.krowk(&["sessions", "rebuild", "--yes"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(listed_children(&b, &parent), kids, "the children hang from the parent again");
}

/// Three subagents, each taking `pause` to answer.
fn fan_three(pause: Duration) -> impl Fn(&Value, usize) -> mock::Reply + Send + 'static {
    move |body, _| {
        if is_child(body) {
            std::thread::sleep(pause);
            return mock::Reply::sse(&mock::text_stream("done."));
        }
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        mock::Reply::sse(&tool_calls(&[
            ("toolu_1", "subagent", json!({"description": "one", "prompt": "TASK-1"})),
            ("toolu_2", "subagent", json!({"description": "two", "prompt": "TASK-2"})),
            ("toolu_3", "subagent", json!({"description": "three", "prompt": "TASK-3"})),
        ]))
    }
}

#[test]
fn r_sub_2_max_parallel_one_runs_the_fan_out_one_subagent_at_a_time() {
    let m = mock::serve(fan_three(Duration::from_millis(150)));
    let b = Sandbox::new("max-parallel", &m.url);
    let config = b.root.join("home/.krowk");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.json"), r#"{"subagents": {"maxParallel": 1}}"#).unwrap();
    let out = b.krowk(&["-p", "three, one at a time", "--model", "claude-sonnet-4-6", "--output-format", "json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let parent = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    // Each child's session, from its root to its turn's end.
    let mut spans: Vec<(i64, i64)> = children_of(&b, &parent)
        .iter()
        .map(|k| {
            let log = b.log(k);
            (log[0]["timeMs"].as_i64().unwrap(), log.iter().rev().find(|e| e["type"] == "turn.completed").unwrap()["timeMs"].as_i64().unwrap())
        })
        .collect();
    spans.sort();
    assert_eq!(spans.len(), 3);
    for w in spans.windows(2) {
        assert!(w[1].0 >= w[0].1, "one waits for the one before: {spans:?}");
    }
    // All three calls answered, in the order they were asked.
    let seen = m.seen.lock().unwrap();
    let ids: Vec<String> = results(&seen.last().unwrap().body).into_iter().map(|(id, _, _)| id).collect();
    assert_eq!(ids, ["toolu_1", "toolu_2", "toolu_3"]);
}

/// The parent asks for a subagent the repository defines on another
/// instance's model.
fn elsewhere(body: &Value, _: usize) -> mock::Reply {
    if is_child(body) {
        return mock::Reply::sse(&mock::text_stream("ran here."));
    }
    if answered(body) {
        return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
    }
    mock::Reply::sse(&tool_calls(&[("toolu_far", "subagent", json!({"description": "go elsewhere", "prompt": "TASK-F", "agent": "far"}))]))
}

#[test]
fn r_sub_5_an_untrusted_repositorys_definition_stays_on_the_parents_instance() {
    let m = mock::serve(elsewhere);
    let b = Sandbox::new("untrusted-def", &m.url);
    std::fs::create_dir_all(b.root.join("repo/.krowk/agents")).unwrap();
    std::fs::write(b.root.join("repo/.krowk/agents/far.md"), "---\nname: far\ndescription: runs elsewhere\nmodel: gpt-5.5\n---\nGo.\n").unwrap();
    let out = b.krowk(&["-p", "use the far agent", "--model", "claude-sonnet-4-6", "--output-format", "json"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("agent far asks for openai/gpt-5.5") && stderr.contains("not a repository you have trusted"), "the person is told: {stderr}");
    let parent = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let started = b.log(&parent).into_iter().find(|e| e["type"] == "subagent.started").unwrap();
    assert_eq!(started["model"], json!({"instance": "anthropic", "model": "claude-sonnet-4-6"}), "the parent's instance, not the definition's");
    assert!(m.seen.lock().unwrap().iter().any(|s| is_child(&s.body)), "it ran, here");
    // Trusted (here for one run), the definition's choice stands — and
    // this machine has no OpenAI key, so it cannot start.
    let out = b.krowk(&["-p", "use the far agent", "--model", "claude-sonnet-4-6", "--output-format", "json", "--trust"]);
    let parent = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let result = b.log(&parent).into_iter().find(|e| e["item"]["kind"] == "toolResult").unwrap();
    assert_eq!(result["item"]["isError"], true);
    assert!(result["item"]["output"].as_str().unwrap().contains("OPENAI_API_KEY"), "{result}");
}

/// The parent's one subagent reads the README once, then answers.
fn one_reader(body: &Value, _: usize) -> mock::Reply {
    if is_child(body) {
        if answered(body) {
            return mock::Reply::sse(&mock::text_stream("read it."));
        }
        return mock::Reply::sse(&mock::tool_use("toolu_r", "read", &json!({"path": "README.md"})));
    }
    if answered(body) {
        return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
    }
    mock::Reply::sse(&tool_calls(&[("toolu_one", "subagent", json!({"description": "read it", "prompt": "TASK-R"}))]))
}

fn run_in_process(b: &Sandbox, cfg: HostConfig, session: Option<String>, text: &str, mode: PermissionMode) -> krowk_harness::protocol::RunResult {
    let _ = b;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let host = Host::new(cfg);
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let r = host.execute(Command::Prompt { session_id: session, text: text.into(), images: Vec::new(), model: Some(model), permission_mode: mode, toolset: None, effort: None, budget: None }, tx).await;
        let _ = drain.await;
        r.unwrap().unwrap()
    })
}

#[test]
fn r_sub_4_the_parents_turn_cost_includes_its_subagents() {
    let m = mock::serve(one_reader);
    let b = Sandbox::new("turn-cost", &m.url);
    let r = run_in_process(&b, b.host(), None, "have a subagent read it", PermissionMode::Default);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    let priced = |session: &str| -> f64 {
        b.log(session).iter().filter(|e| e["type"] == "response.completed").map(|e| sonnet("", "", &serde_json::from_value(e["usage"].clone()).unwrap()).unwrap()).sum()
    };
    let own = priced(&r.session_id);
    let child = children_of(&b, &r.session_id);
    let theirs = priced(&child[0]);
    assert!(theirs > 0.0);
    let cost = r.cost_usd.unwrap();
    assert!((cost - (own + theirs)).abs() < 1e-12, "the turn's cost is its own {own} and its subagent's {theirs}: {cost}");
}

/// The parent's subagent publishes a file; the parent publishes another
/// on its next turn.
fn publishing_child(body: &Value, n: usize) -> mock::Reply {
    let publish = |id: &str| mock::Reply::sse(&mock::tool_use(id, "publish", &json!({"files": ["README.md"]})));
    if is_child(body) {
        return if answered(body) { mock::Reply::sse(&mock::text_stream("published.")) } else { publish("toolu_cpub") };
    }
    if answered(body) {
        return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
    }
    if body["messages"].as_array().unwrap().len() > 1 {
        return publish(&format!("toolu_ppub{n}"));
    }
    mock::Reply::sse(&tool_calls(&[("toolu_pub", "subagent", json!({"description": "publish it", "prompt": "TASK-P: publish README.md"}))]))
}

#[test]
fn r_sub_6_a_run_a_subagent_opens_is_the_parents_and_a_resume_reuses_it() {
    let m = mock::serve(publishing_child);
    let b = Sandbox::new("child-run", &m.url);
    let asked: Arc<std::sync::Mutex<Vec<krowk_harness::evidence::PublishRequest>>> = Arc::default();
    let seen = asked.clone();
    let publisher: krowk_harness::evidence::Publisher = Arc::new(move |r: &krowk_harness::evidence::PublishRequest| {
        seen.lock().unwrap().push(r.clone());
        Ok(krowk_harness::evidence::Published { text: "published".into(), run: Some(r.run.clone().unwrap_or_else(|| "run_child".into())), for_person: Vec::new() })
    });
    let cfg = || HostConfig { publisher: Some(publisher.clone()), ..b.host() };
    let r = run_in_process(&b, cfg(), None, "have a subagent publish the README", PermissionMode::AcceptEdits);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    let opened = |session: &str| b.log(session).iter().filter(|e| e["type"] == "run.opened").map(|e| e["run"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let child = children_of(&b, &r.session_id).remove(0);
    assert_eq!(opened(&r.session_id), ["run_child"], "logged in the parent's log");
    assert!(opened(&child).is_empty(), "and not the subagent's");
    // The parent's next turn, a new host, attaches to the same run.
    let again = run_in_process(&b, cfg(), Some(r.session_id.clone()), "now publish it yourself", PermissionMode::AcceptEdits);
    assert_eq!(again.status, TurnStatus::Completed, "{:?}", again.error);
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert_eq!((asked[0].session_id.as_str(), asked[0].run.as_deref()), (r.session_id.as_str(), None), "tagged with the parent's session");
    assert_eq!(asked[1].run.as_deref(), Some("run_child"), "the resumed parent reuses the run its subagent opened");
}

/// The parent starts one subagent, or writes a todo list, as `first`
/// says; the subagent runs `echo from-the-child` and reports what came of
/// it word for word.
fn child_runs_bash(first: &'static str) -> impl Fn(&Value, usize) -> mock::Reply + Send + 'static {
    move |body, _| {
        if is_child(body) {
            // Once it has its command's result, it only reports — sent back
            // to work by a hook, it reports again.
            let result = body["messages"].as_array().unwrap().iter().filter_map(|m| m["content"].as_array()).flatten().find(|c| c["type"] == "tool_result").cloned();
            if let Some(r) = result {
                let (out, err) = (match &r["content"] { Value::String(s) => s.clone(), o => o.to_string() }, r["is_error"].as_bool().unwrap_or(false));
                return mock::Reply::sse(&mock::text_stream(&format!("CHILD-SAW error={err} {}", out.replace('\n', " "))));
            }
            return mock::Reply::sse(&mock::tool_use("toolu_sh", "bash", &json!({"command": "echo from-the-child"})));
        }
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        match first {
            "todo" => mock::Reply::sse(&mock::tool_use("toolu_todo", "todo_write", &json!({"todos": [{"content": "x", "status": "pending"}]}))),
            _ => mock::Reply::sse(&tool_calls(&[("toolu_sub", "subagent", json!({"description": "run a command", "prompt": "TASK-B: run echo"}))])),
        }
    }
}

impl Sandbox {
    fn host_with(&self, user: Value, approvals: bool) -> HostConfig {
        let mut cfg = self.host();
        cfg.permissions.user = Some(user);
        cfg.permissions.approvals = approvals;
        cfg
    }
}

/// What the parent's one tool call was answered with.
fn parent_result(b: &Sandbox, session: &str) -> (String, bool) {
    let r = b.log(session).into_iter().find(|e| e["item"]["kind"] == "toolResult").unwrap();
    (r["item"]["output"].as_str().unwrap().to_string(), r["item"]["isError"].as_bool().unwrap())
}

#[test]
fn r_sub_1_a_subagents_calls_are_judged_by_its_parents_rules_and_mode() {
    let m = mock::serve(child_runs_bash("subagent"));
    let b = Sandbox::new("child-rules", &m.url);
    // The parent's mode: default asks about a command, and headless nobody
    // answers — refused at once, in the subagent as in the parent.
    let r = run_in_process(&b, b.host_with(json!({}), false), None, "run it in a subagent", PermissionMode::Default);
    let (out, _) = parent_result(&b, &r.session_id);
    assert!(out.starts_with("CHILD-SAW error=true") && out.contains("echo from-the-child"), "refused, never waited on: {out}");
    // bypassPermissions in the parent is bypassPermissions in the child.
    let r = run_in_process(&b, b.host_with(json!({}), false), None, "run it in a subagent", PermissionMode::BypassPermissions);
    let (out, _) = parent_result(&b, &r.session_id);
    assert!(out.starts_with("CHILD-SAW error=false from-the-child"), "{out}");
    // A deny rule holds in the child, bypass or not.
    let deny = json!({"permissions": {"deny": ["Bash(echo *)"]}});
    let r = run_in_process(&b, b.host_with(deny, false), None, "run it in a subagent", PermissionMode::BypassPermissions);
    let (out, _) = parent_result(&b, &r.session_id);
    assert!(out.starts_with("CHILD-SAW error=true") && out.contains("denied by the rule `Bash(echo *)`"), "{out}");
}

#[test]
fn r_sub_1_subagent_and_todo_write_need_no_mode_but_a_deny_rule_refuses_them() {
    let m = mock::serve(child_runs_bash("subagent"));
    let b = Sandbox::new("task-deny", &m.url);
    // In plan mode, which changes nothing, both run: they need no mode.
    let r = run_in_process(&b, b.host_with(json!({}), false), None, "run it in a subagent", PermissionMode::Plan);
    assert!(parent_result(&b, &r.session_id).0.starts_with("CHILD-SAW"), "the subagent ran, in plan mode itself");
    let r = run_in_process(&b, b.host_with(json!({"permissions": {"deny": ["Task"]}}), false), None, "run it in a subagent", PermissionMode::BypassPermissions);
    let (out, err) = parent_result(&b, &r.session_id);
    assert!(err && out.contains("denied by the rule `Task`"), "{out}");
    assert!(children_of(&b, &r.session_id).is_empty(), "no subagent was started");
    // By its agent: `Task(<name>)`, the general one being `general-purpose`.
    let r = run_in_process(&b, b.host_with(json!({"permissions": {"deny": ["Task(general-purpose)"]}}), false), None, "run it in a subagent", PermissionMode::Default);
    assert!(parent_result(&b, &r.session_id).0.contains("denied by the rule `Task(general-purpose)`"));
    let m = mock::serve(child_runs_bash("todo"));
    let b = Sandbox::new("todo-deny", &m.url);
    let r = run_in_process(&b, b.host_with(json!({"permissions": {"deny": ["TodoWrite"]}}), false), None, "plan it", PermissionMode::Default);
    let (out, err) = parent_result(&b, &r.session_id);
    assert!(err && out.contains("denied by the rule `TodoWrite`"), "{out}");
    assert!(!b.log(&r.session_id).iter().any(|e| e["type"] == "todos.updated"), "the list was not written");
}

#[test]
fn r_sub_1_hooks_see_subagent_calls_as_task_and_can_block_them() {
    let m = mock::serve(child_runs_bash("subagent"));
    let b = Sandbox::new("task-hook", &m.url);
    let seen = b.root.join("task-hook.json");
    let hook = json!({"hooks": {"PreToolUse": [{"matcher": "Task", "hooks": [{"type": "command", "command": format!("cat > '{}'; echo 'no subagents today' >&2; exit 2", seen.display())}]}]}});
    let r = run_in_process(&b, b.host_with(hook, false), None, "run it in a subagent", PermissionMode::BypassPermissions);
    let (out, err) = parent_result(&b, &r.session_id);
    assert!(err && out.contains("PreToolUse hook blocked it: no subagents today"), "{out}");
    let input: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!((input["tool_name"].as_str(), input["tool_input"]["description"].as_str()), (Some("Task"), Some("run a command")));
    assert!(children_of(&b, &r.session_id).is_empty());
}

#[test]
fn r_sub_2_a_subagents_approval_request_is_answered_under_its_own_session() {
    let m = mock::serve(child_runs_bash("subagent"));
    let b = Sandbox::new("child-approval", &m.url);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let host = Host::new(b.host_with(json!({}), true));
    let mut app = krowk_tui::app::App::new(krowk_tui::editor::Editor::new(None), 160, krowk_tui::settings::Settings::default(), None, None);
    let mut asked = None;
    let r = rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let exec = host.execute(Command::Prompt { session_id: None, text: "run it in a subagent".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }, tx);
        tokio::pin!(exec);
        app.start_turn(std::time::Instant::now());
        loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    app.on_line(&line);
                    if let StreamLine::Live(LiveEvent::ApprovalRequested(req)) = &line {
                        // The TUI shows it, and says whose it is.
                        let shown = row_text!(app.view(std::time::Instant::now()).0).join("\n");
                        assert!(shown.contains("subagent “run a command”: allow"), "{shown}");
                        asked = Some(req.clone());
                        let (itx, _irx) = tokio::sync::mpsc::channel(1);
                        host.execute(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: krowk_harness::protocol::ApprovalDecision::Allow, answers: Vec::new() }, itx).await.unwrap();
                    }
                }
                r = &mut exec => break r.unwrap().unwrap(),
            }
        }
    });
    let req = asked.expect("the subagent's bash was asked about");
    let child = children_of(&b, &r.session_id).remove(0);
    assert_eq!(req.session_id, child, "asked under the subagent's own session");
    assert!(parent_result(&b, &r.session_id).0.starts_with("CHILD-SAW error=false from-the-child"), "approved, it ran");
}

/// A `subagent.status` frame as `(status, tool and when it began, waiting,
/// tokens)`.
type Status = (String, Option<(String, i64)>, Option<String>, i64);

/// A child's `subagent.status` frames, from the lines of a run.
fn statuses(lines: &[StreamLine], child: &str) -> Vec<Status> {
    lines
        .iter()
        .filter_map(|l| match l {
            StreamLine::Live(LiveEvent::SubagentStatus { session_id, status, tool, waiting, tokens, last_event_ms }) if session_id == child => {
                assert!(*last_event_ms > 0);
                let word = |v: Value| v.as_str().unwrap().to_string();
                Some((word(json!(status)), tool.as_ref().map(|t| (t.name.clone(), t.started_ms)), waiting.map(|w| word(json!(w))), *tokens))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn r_sub_9_a_child_that_runs_a_tool_and_waits_on_an_approval_says_each_change_in_order() {
    let m = mock::serve(child_runs_bash("subagent"));
    let b = Sandbox::new("child-status", &m.url);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let host = Host::new(b.host_with(json!({}), true));
    let mut lines = Vec::new();
    let r = rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let exec = host.execute(Command::Prompt { session_id: None, text: "run it in a subagent".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }, tx);
        tokio::pin!(exec);
        loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    if let StreamLine::Live(LiveEvent::ApprovalRequested(req)) = &line {
                        let (itx, _irx) = tokio::sync::mpsc::channel(1);
                        host.execute(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: krowk_harness::protocol::ApprovalDecision::Allow, answers: Vec::new() }, itx).await.unwrap();
                    }
                    lines.push(line);
                }
                r = &mut exec => {
                    while let Ok(l) = rx.try_recv() {
                        lines.push(l);
                    }
                    break r.unwrap().unwrap();
                }
            }
        }
    });
    let child = children_of(&b, &r.session_id).remove(0);
    let s = statuses(&lines, &child);
    let shape: Vec<(&str, Option<&str>, Option<&str>)> = s.iter().map(|(st, t, w, _)| (st.as_str(), t.as_ref().map(|(n, _)| n.as_str()), w.as_deref())).collect();
    assert_eq!(
        shape,
        [
            ("running", None, None),
            // The model's call came back: its tokens.
            ("running", None, None),
            ("running", Some("bash"), None),
            // Waiting on the person, it is in no call yet.
            ("running", None, Some("approval")),
            ("running", Some("bash"), None),
            ("running", None, None),
            ("running", None, None),
            ("done", None, None),
        ],
        "{s:?}"
    );
    // The call's start is a time, moved to when the approval was answered:
    // the wait was not the tool's. The tokens only grow.
    let started: Vec<i64> = s.iter().filter_map(|(_, t, _, _)| t.as_ref().map(|(_, at)| *at)).collect();
    assert!(started.len() == 2 && started[0] > 0 && started[1] >= started[0], "{started:?}");
    assert!(s.windows(2).all(|w| w[0].3 <= w[1].3) && s.last().unwrap().3 > 0, "{s:?}");
    // Its link says krowk ran it.
    let started = b.log(&r.session_id).into_iter().find(|e| e["type"] == "subagent.started").unwrap();
    assert_eq!(started["ranBy"], "krowk");
    assert!(started.get("backendId").is_none());
}

#[test]
fn r_sub_9_a_child_streaming_a_thousand_deltas_sends_a_frame_per_change_of_state_only() {
    let words: String = (0..1000).map(|i| format!("w{i} ")).collect();
    let m = mock::serve(move |body: &Value, _| {
        if is_child(body) {
            return mock::Reply::sse(&mock::text_stream(&words));
        }
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        mock::Reply::sse(&tool_calls(&[("toolu_long", "subagent", json!({"description": "talk at length", "prompt": "TASK-L"}))]))
    });
    let b = Sandbox::new("child-deltas", &m.url);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let host = Host::new(b.host());
    let (r, lines) = rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let keep = tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Some(l) = rx.recv().await {
                lines.push(l);
            }
            lines
        });
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let r = host.execute(Command::Prompt { session_id: None, text: "have a child talk".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }, tx).await;
        (r.unwrap().unwrap(), keep.await.unwrap())
    });
    let child = children_of(&b, &r.session_id).remove(0);
    let deltas = lines.iter().filter(|l| matches!(l, StreamLine::Live(LiveEvent::ItemDelta { session_id, .. }) if *session_id == child)).count();
    assert!(deltas >= 1000, "{deltas} deltas");
    // Its start, its one response, its end.
    let s = statuses(&lines, &child);
    assert_eq!(s.iter().map(|(st, ..)| st.as_str()).collect::<Vec<_>>(), ["running", "running", "done"], "{s:?}");
}

/// The parent starts one subagent, naming `agent`; the subagent answers
/// at once.
fn names_agent(agent: &'static str) -> impl Fn(&Value, usize) -> mock::Reply + Send + 'static {
    move |body, _| {
        if is_child(body) {
            return mock::Reply::sse(&mock::text_stream("CHILD-DONE"));
        }
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        let mut input = json!({"description": "review it", "prompt": "TASK-R"});
        if !agent.is_empty() {
            input["agent"] = json!(agent);
        }
        mock::Reply::sse(&tool_calls(&[("toolu_rv", "subagent", input)]))
    }
}

#[test]
fn r_sub_5_every_spelling_of_a_denied_agent_is_denied_and_an_unknown_one_refused_first() {
    for spelled in ["reviewer", " reviewer ", "Reviewer", "\treviewer\n", "REVIEWER"] {
        let m = mock::serve(names_agent(spelled));
        let b = Sandbox::new("deny-spelling", &m.url);
        std::fs::create_dir_all(b.root.join("repo/.krowk/agents")).unwrap();
        std::fs::write(b.root.join("repo/.krowk/agents/reviewer.md"), "---\nname: reviewer\ndescription: reviews\n---\n").unwrap();
        let seen = b.root.join("pre.json");
        let hook = json!({"permissions": {"deny": ["Task(reviewer)"]}, "hooks": {"PreToolUse": [{"matcher": "Task", "hooks": [{"type": "command", "command": format!("cat > '{}'", seen.display())}]}]}});
        let r = run_in_process(&b, b.host_with(hook, false), None, "review it", PermissionMode::BypassPermissions);
        let (out, err) = parent_result(&b, &r.session_id);
        assert!(err && out.contains("denied by the rule `Task(reviewer)`"), "{spelled:?}: {out}");
        assert!(children_of(&b, &r.session_id).is_empty(), "{spelled:?} started no subagent");
        // The hook saw the call as Task, its agent as the definition names it.
        let input: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
        assert_eq!(input["tool_name"], "Task");
    }
    // A name no definition has is refused before hooks and rules see it.
    let m = mock::serve(names_agent("nobody"));
    let b = Sandbox::new("unknown-agent", &m.url);
    let seen = b.root.join("pre.json");
    let hook = json!({"hooks": {"PreToolUse": [{"matcher": "Task", "hooks": [{"type": "command", "command": format!("cat > '{}'", seen.display())}]}]}});
    let r = run_in_process(&b, b.host_with(hook, false), None, "review it", PermissionMode::BypassPermissions);
    let (out, err) = parent_result(&b, &r.session_id);
    assert!(err && out.contains("there is no agent named \"nobody\""), "{out}");
    assert!(!seen.exists(), "no hook ran for it");
    assert!(children_of(&b, &r.session_id).is_empty());
}

#[test]
fn r_sub_1_an_ask_rule_or_a_hooks_ask_holds_subagent_and_todo_write_and_headless_refuses() {
    let asks = [
        json!({"permissions": {"ask": ["Task"]}}),
        json!({"permissions": {"ask": ["Task(general-purpose)"]}}),
        json!({"hooks": {"PreToolUse": [{"matcher": "Task", "hooks": [{"type": "command", "command": "echo '{\"hookSpecificOutput\": {\"hookEventName\": \"PreToolUse\", \"permissionDecision\": \"ask\"}}'"}]}]}}),
    ];
    for user in asks {
        let m = mock::serve(names_agent(""));
        let b = Sandbox::new("task-ask", &m.url);
        // bypassPermissions does not answer an ask.
        let r = run_in_process(&b, b.host_with(user.clone(), false), None, "review it", PermissionMode::BypassPermissions);
        let (out, err) = parent_result(&b, &r.session_id);
        assert!(err && out.contains("needs approval") && out.contains("nobody is here to give it"), "{user}: {out}");
        assert!(children_of(&b, &r.session_id).is_empty(), "{user}");
    }
    let m = mock::serve(child_runs_bash("todo"));
    let b = Sandbox::new("todo-ask", &m.url);
    let r = run_in_process(&b, b.host_with(json!({"permissions": {"ask": ["TodoWrite"]}}), false), None, "plan it", PermissionMode::Default);
    let (out, err) = parent_result(&b, &r.session_id);
    assert!(err && out.contains("needs approval"), "{out}");
    assert!(!b.log(&r.session_id).iter().any(|e| e["type"] == "todos.updated"));
}

#[test]
fn r_sub_1_a_subagent_fires_subagent_stop_not_stop_or_user_prompt_submit_and_hooks_see_its_parent() {
    let m = mock::serve(child_runs_bash("subagent"));
    let b = Sandbox::new("subagent-stop", &m.url);
    let log = b.root.join("hooks.jsonl");
    let record = format!("input=$(cat); printf '%s\\n' \"$input\" >> '{}'", log.display());
    // SubagentStop blocks once: the subagent goes on, reading why.
    let block_once = format!("{record}; case \"$input\" in *'\"hook_event_name\":\"SubagentStop\"'*'\"stop_hook_active\":false'*) echo 'check your work' >&2; exit 2;; esac");
    let hooks = json!({"hooks": {
        "UserPromptSubmit": [{"hooks": [{"type": "command", "command": record}]}],
        "Stop": [{"hooks": [{"type": "command", "command": record}]}],
        "SubagentStop": [{"hooks": [{"type": "command", "command": block_once}]}],
        "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": record}]}],
    }});
    let r = run_in_process(&b, b.host_with(hooks, false), None, "run it in a subagent", PermissionMode::BypassPermissions);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    let child = children_of(&b, &r.session_id).remove(0);
    let fired: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let events: Vec<(String, String)> = fired.iter().map(|f| (f["hook_event_name"].as_str().unwrap().to_string(), f["session_id"].as_str().unwrap().to_string())).collect();
    let parent = r.session_id.clone();
    assert_eq!(
        events,
        [("UserPromptSubmit".into(), parent.clone()), ("PreToolUse".into(), parent.clone()), ("SubagentStop".into(), parent.clone()), ("SubagentStop".into(), parent.clone()), ("Stop".into(), parent.clone())],
        "the parent's prompt, the child's bash, the child's end twice (blocked once), the parent's end — every one under the parent's session"
    );
    for f in &fired[1..4] {
        assert_eq!(f["agent_session_id"], child.as_str(), "the subagent's own id beside it: {f}");
        assert!(f["agent_transcript_path"].as_str().unwrap().contains(&child));
        assert!(f["transcript_path"].as_str().unwrap().contains(&parent));
    }
    assert!(fired[0].get("agent_session_id").is_none() && fired[4].get("agent_session_id").is_none());
    assert_eq!((fired[2]["stop_hook_active"].as_bool(), fired[3]["stop_hook_active"].as_bool()), (Some(false), Some(true)));
    // Blocked, the subagent read why and made one more call.
    let seen = m.seen.lock().unwrap();
    let last_child = seen.iter().map(|s| &s.body).rfind(|b| is_child(b)).unwrap();
    assert!(last_child.to_string().contains("check your work"), "{last_child}");
}

#[test]
fn r_sub_5_a_definition_spelled_in_another_case_never_dodges_a_task_deny() {
    // The reviewer's case: the person denies `Task(reviewer)`; the
    // repository defines `Reviewer`, found first; the model asks for it by
    // either spelling. And the same with the rule spelled in capitals, and
    // with the person's own `reviewer` shadowed by the repository's.
    for (rule, asked, user_def) in [
        ("Task(reviewer)", "Reviewer", false),
        ("Task(reviewer)", "reviewer", false),
        ("Task(reviewer)", " REVIEWER ", true),
        ("Task(REVIEWER)", "Reviewer", false),
        ("Task(rev*)", "Reviewer", true),
    ] {
        let m = mock::serve(names_agent(Box::leak(asked.to_string().into_boxed_str())));
        let b = Sandbox::new("case-deny", &m.url);
        std::fs::create_dir_all(b.root.join("repo/.krowk/agents")).unwrap();
        std::fs::write(b.root.join("repo/.krowk/agents/Reviewer.md"), "---\nname: Reviewer\ndescription: the repository's\n---\n").unwrap();
        let mut cfg = b.host_with(json!({"permissions": {"deny": [rule]}}), false);
        if user_def {
            let dir = b.root.join("home/.krowk/agents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("reviewer.md"), "---\nname: reviewer\ndescription: the person's\n---\n").unwrap();
            cfg.agents.user_dirs = vec![dir];
        }
        let r = run_in_process(&b, cfg, None, "review it", PermissionMode::BypassPermissions);
        let (out, err) = parent_result(&b, &r.session_id);
        assert!(err && out.contains("is denied by the rule"), "{rule} / {asked:?}: {out}");
        assert!(children_of(&b, &r.session_id).is_empty(), "{rule} / {asked:?} started no subagent");
    }
    // An agent the rule does not name still runs.
    let m = mock::serve(names_agent("Reviewer"));
    let b = Sandbox::new("case-allow", &m.url);
    std::fs::create_dir_all(b.root.join("repo/.krowk/agents")).unwrap();
    std::fs::write(b.root.join("repo/.krowk/agents/Reviewer.md"), "---\nname: Reviewer\ndescription: the repository's\n---\n").unwrap();
    let r = run_in_process(&b, b.host_with(json!({"permissions": {"deny": ["Task(writer)"]}}), false), None, "review it", PermissionMode::BypassPermissions);
    assert_eq!(children_of(&b, &r.session_id).len(), 1);
}

#[test]
fn r_sub_1_task_hooks_read_claude_codes_input_and_can_filter_on_subagent_type() {
    for (asked, blocked) in [("reviewer", true), ("", false)] {
        let m = mock::serve(names_agent(asked));
        let b = Sandbox::new("task-hook-type", &m.url);
        std::fs::create_dir_all(b.root.join("repo/.krowk/agents")).unwrap();
        std::fs::write(b.root.join("repo/.krowk/agents/reviewer.md"), "---\nname: reviewer\ndescription: reviews\n---\n").unwrap();
        let seen = b.root.join("task-input.json");
        // Blocks only the reviewer, reading tool_input.subagent_type as a
        // hook written for Claude Code does.
        let command = format!("input=$(cat); printf '%s' \"$input\" > '{}'; case \"$input\" in *'\"subagent_type\":\"reviewer\"'*) echo 'no reviews today' >&2; exit 2;; esac", seen.display());
        let hooks = json!({"hooks": {"PreToolUse": [{"matcher": "Task", "hooks": [{"type": "command", "command": command}]}]}});
        let r = run_in_process(&b, b.host_with(hooks, false), None, "review it", PermissionMode::BypassPermissions);
        let input: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
        let want = if blocked { "reviewer" } else { "general-purpose" };
        assert_eq!(input["tool_input"], json!({"description": "review it", "prompt": "TASK-R", "subagent_type": want}), "Claude Code's Task input");
        let (out, err) = parent_result(&b, &r.session_id);
        assert_eq!(err, blocked, "{out}");
        assert_eq!(children_of(&b, &r.session_id).len(), usize::from(!blocked));
        if blocked {
            assert!(out.contains("no reviews today"), "{out}");
        }
    }
}

#[test]
fn r_sub_1_a_subagents_file_tools_are_fenced_from_krowks_home_as_its_parents_are() {
    // The child goes for the credentials file by name, then searches the
    // home, under allow rules for both, in acceptEdits, with nobody to ask:
    // both are refused, and the key never reaches a model.
    const KEY: &str = "sk-ant-SUBAGENT-FENCE-SENTINEL";
    let creds = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let path = creds.clone();
    let m = mock::serve(move |body, _| {
        if is_child(body) {
            let got = results(body);
            return match got.len() {
                0 if !answered(body) => mock::Reply::sse(&tool_calls(&[
                    ("toolu_cr", "read", json!({"path": *path.lock().unwrap()})),
                    ("toolu_cg", "grep", json!({"pattern": "SENTINEL", "path": std::path::Path::new(&*path.lock().unwrap()).parent().unwrap()})),
                ])),
                _ => mock::Reply::sse(&mock::text_stream(&format!("CHILD-SAW {}", got.iter().map(|(_, o, e)| format!("error={e} {o}")).collect::<Vec<_>>().join(" | ")))),
            };
        }
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        mock::Reply::sse(&tool_calls(&[("toolu_sub", "subagent", json!({"description": "look around", "prompt": "TASK-F: read the key"}))]))
    });
    let b = Sandbox::new("child-fence", &m.url);
    let file = b.root.join("home/.krowk/credentials.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, json!({"keys": {"anthropic": {"literal": KEY}}}).to_string()).unwrap();
    *creds.lock().unwrap() = file.display().to_string();
    let allow = json!({"permissions": {"allow": ["Read", "Grep"]}});
    let r = run_in_process(&b, b.host_with(allow, false), None, "look around in a subagent", PermissionMode::AcceptEdits);
    let (out, _) = parent_result(&b, &r.session_id);
    assert_eq!(out.matches("error=true").count(), 2, "{out}");
    assert_eq!(out.matches("inside krowk's home").count(), 2, "{out}");
    for s in m.seen.lock().unwrap().iter() {
        assert!(!s.body.to_string().contains(KEY), "the key reached a model: {}", s.body);
    }
}

/// git in the sandbox's repository, through krowk's own git, with none of
/// this machine's config: what it prints, trimmed.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let o = krowk_api::git::command(dir).unwrap().args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Every `tool_result` of the parent's log: the call it answers, its
/// output and whether it is an error.
fn parent_results(b: &Sandbox, session: &str) -> Vec<(String, String, bool)> {
    b.log(session)
        .into_iter()
        .filter(|e| e["item"]["kind"] == "toolResult")
        .map(|e| (e["item"]["callId"].as_str().unwrap_or_default().to_string(), e["item"]["output"].as_str().unwrap().to_string(), e["item"]["isError"].as_bool().unwrap()))
        .collect()
}

/// The parent starts two subagents in worktrees in one response: one
/// writes a file, the other changes nothing.
fn two_in_worktrees(body: &Value, _: usize) -> mock::Reply {
    if is_child(body) {
        if answered(body) || task(body).contains("TASK-NOOP") {
            return mock::Reply::sse(&mock::text_stream("done."));
        }
        return mock::Reply::sse(&mock::tool_use("toolu_w", "write", &json!({"path": "NEW.md", "content": "from the child\n"})));
    }
    if answered(body) {
        return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
    }
    mock::Reply::sse(&tool_calls(&[
        ("toolu_edit", "subagent", json!({"description": "write a file", "prompt": "TASK-EDIT", "isolation": "worktree"})),
        ("toolu_noop", "subagent", json!({"description": "do nothing", "prompt": "TASK-NOOP", "isolation": "worktree"})),
    ]))
}

/// Worktrees WT3, WT13: each child in its own worktree and branch,
/// recorded as its directory; the one that changed nothing leaves nothing
/// behind, the one that wrote a file has it applied to the parent's
/// working tree, uncommitted, its summary naming it, and its worktree and
/// branch gone too.
#[test]
fn wt3_two_subagents_in_worktrees_each_get_their_own_and_a_changed_ones_work_is_applied() {
    let m = mock::serve(two_in_worktrees);
    let b = Sandbox::new("worktrees", &m.url);
    let repo = b.root.join("repo");
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let root = b.root.join("worktrees");
    let cfg = HostConfig { agents: krowk_harness::subagent::AgentsConfig { worktrees: Some(root.clone()), ..krowk_harness::subagent::AgentsConfig::none() }, ..b.host() };
    let r = run_in_process(&b, cfg, None, "two subagents in worktrees", PermissionMode::AcceptEdits);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    let cwd = |child: &str| b.log(child).iter().find(|e| e["type"] == "session.started").unwrap()["cwd"].as_str().unwrap().to_string();
    let children = children_of(&b, &r.session_id);
    assert_eq!(children.len(), 2);
    let dirs: Vec<String> = children.iter().map(|c| cwd(c)).collect();
    assert_ne!(dirs[0], dirs[1], "a directory each");
    for d in &dirs {
        assert!(std::path::Path::new(d).starts_with(&root), "under the worktrees root: {d}");
        assert!(!std::path::Path::new(d).exists(), "removed: {d}");
    }
    let results = parent_results(&b, &r.session_id);
    let (edit, noop) = (results.iter().find(|(c, ..)| c == "toolu_edit").unwrap(), results.iter().find(|(c, ..)| c == "toolu_noop").unwrap());
    assert!(!noop.2 && noop.1 == "done.", "nothing to say of a removed worktree: {noop:?}");
    assert_eq!(edit.1.lines().last().unwrap(), "Changes applied to your working tree: NEW.md", "{edit:?}");
    assert_eq!(std::fs::read_to_string(repo.join("NEW.md")).unwrap(), "from the child\n", "in the parent's checkout");
    assert_eq!((git(&repo, &["rev-parse", "HEAD"]), git(&repo, &["diff", "--cached"])), (head, String::new()), "uncommitted, unstaged");
    assert_eq!(git(&repo, &["branch", "--list", "krowk/*"]), "", "both branches are gone");
    assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count(), 1);
}

/// Worktrees WT3: outside a git repository a worktree is refused by name
/// and no subagent starts; `"none"` runs in the parent's directory as
/// before.
#[test]
fn wt3_isolation_outside_a_repository_is_refused_and_none_is_todays() {
    let m = mock::serve(|body: &Value, _: usize| {
        if is_child(body) {
            return mock::Reply::sse(&mock::text_stream("done."));
        }
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        mock::Reply::sse(&tool_calls(&[
            ("toolu_wt", "subagent", json!({"description": "isolated", "prompt": "TASK-W", "isolation": "worktree"})),
            ("toolu_none", "subagent", json!({"description": "here", "prompt": "TASK-N", "isolation": "none"})),
        ]))
    });
    // The sandbox's `.git` is an empty directory: no repository to git.
    let b = Sandbox::new("no-repo", &m.url);
    let root = b.root.join("worktrees");
    let cfg = HostConfig { agents: krowk_harness::subagent::AgentsConfig { worktrees: Some(root.clone()), ..krowk_harness::subagent::AgentsConfig::none() }, ..b.host() };
    let r = run_in_process(&b, cfg, None, "try a worktree", PermissionMode::AcceptEdits);
    let results = parent_results(&b, &r.session_id);
    let wt = results.iter().find(|(c, ..)| c == "toolu_wt").unwrap();
    assert_eq!((wt.1.as_str(), wt.2), ("isolation: worktree needs a git repository", true));
    let none = results.iter().find(|(c, ..)| c == "toolu_none").unwrap();
    assert_eq!((none.1.as_str(), none.2), ("done.", false));
    let children = children_of(&b, &r.session_id);
    assert_eq!(children.len(), 1, "only the one with no worktree started");
    let cwd = b.log(&children[0]).iter().find(|e| e["type"] == "session.started").unwrap()["cwd"].as_str().unwrap().to_string();
    assert_eq!(cwd, b.root.join("repo").display().to_string());
    assert!(!root.exists());
}

/// Worktrees WT3: a repository's `/`-anchored deny holds in a subagent's
/// worktree as it does in the checkout, in bypassPermissions too.
#[test]
fn wt3_a_repositorys_anchored_deny_holds_inside_the_worktree() {
    let m = mock::serve(|body: &Value, _: usize| {
        if is_child(body) {
            if let Some((_, out, err)) = results(body).into_iter().next() {
                return mock::Reply::sse(&mock::text_stream(&format!("CHILD-SAW error={err} {}", out.replace('\n', " "))));
            }
            return mock::Reply::sse(&mock::tool_use("toolu_w", "write", &json!({"path": "blocked/x.md", "content": "no\n"})));
        }
        if answered(body) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        mock::Reply::sse(&tool_calls(&[("toolu_sub", "subagent", json!({"description": "write blocked", "prompt": "TASK-B", "isolation": "worktree"}))]))
    });
    let b = Sandbox::new("worktree-deny", &m.url);
    let repo = b.root.join("repo");
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    std::fs::write(repo.join(".claude/settings.json"), json!({"permissions": {"deny": ["Edit(/blocked/**)"]}}).to_string()).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "README.md", ".claude/settings.json"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let cfg = HostConfig { agents: krowk_harness::subagent::AgentsConfig { worktrees: Some(b.root.join("worktrees")), ..krowk_harness::subagent::AgentsConfig::none() }, ..b.host() };
    let r = run_in_process(&b, cfg, None, "write where the repository forbids", PermissionMode::BypassPermissions);
    let (out, _) = parent_result(&b, &r.session_id);
    assert!(out.starts_with("CHILD-SAW error=true") && out.contains("Edit(/blocked/**)"), "{out}");
    assert!(!out.contains("Worktree:"), "nothing written, nothing kept: {out}");
}

/// The parent starts one subagent, which answers at once.
fn one_subagent(body: &Value, _: usize) -> mock::Reply {
    if is_child(body) {
        return mock::Reply::sse(&mock::text_stream("done."));
    }
    if answered(body) {
        return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
    }
    mock::Reply::sse(&tool_calls(&[("toolu_1", "subagent", json!({"description": "one", "prompt": "TASK-1"}))]))
}

/// WT12: with every agent slot of the machine held — here by the test, as
/// another krowk would — a subagent says so on its call and waits, starting
/// once the slot is let go.
#[test]
fn wt12_a_subagent_waits_for_an_agent_slot_another_process_holds() {
    let m = mock::serve(one_subagent);
    let b = Sandbox::new("agent-slot", &m.url);
    let pool = krowk_harness::slots::Pool::new(b.root.join("run"), krowk_harness::subagent::AGENT_SLOTS, 1);
    let mut held = Some(pool.try_take().unwrap().expect("the one slot"));
    let mut registry = Registry::resolve(&InstancesConfig::default(), &b.env());
    registry.agents = Some(pool);
    let host = Host::new(HostConfig { registry, ..b.host() });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mut waited = false;
    let result = rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let cmd = Command::Prompt { session_id: None, text: "one subagent".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        // The slot is let go a while after the wait is said, the turn
        // running all the while.
        let release = tokio::time::sleep(Duration::from_secs(3600));
        tokio::pin!(release);
        loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    if let StreamLine::Live(LiveEvent::ItemDelta { delta: krowk_harness::protocol::Delta::Text { text }, .. }) = &line
                        && text.contains("waiting for an agent slot (1 in use)")
                    {
                        waited = true;
                        release.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(600));
                    }
                }
                () = &mut release, if held.is_some() => {
                    assert!(!m.seen.lock().unwrap().iter().any(|s| is_child(&s.body)), "the child started while every slot was held");
                    held = None;
                }
                r = &mut exec => break r,
            }
        }
    });
    let result = result.unwrap().unwrap();
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.error);
    assert!(waited && held.is_none(), "the wait was said on the call");
    assert!(m.seen.lock().unwrap().iter().any(|s| is_child(&s.body)), "it ran once the slot was free");
}

/// WT12: with `maxHost = 1` and its one slot held by the session itself —
/// a `--worktree` session's — the session's subagent runs on that slot
/// rather than waiting forever for another, and finishes.
#[test]
fn wt12_a_subagent_of_a_session_on_the_last_agent_slot_runs_on_it() {
    let m = mock::serve(one_subagent);
    let b = Sandbox::new("own-slot", &m.url);
    let pool = krowk_harness::slots::Pool::new(b.root.join("run"), krowk_harness::subagent::AGENT_SLOTS, 1);
    let _session = pool.try_take().unwrap().expect("the session's slot");
    let mut registry = Registry::resolve(&InstancesConfig::default(), &b.env());
    registry.agents = Some(pool);
    let session = krowk_harness::host::SessionSetup { on_agent_slot: true, ..Default::default() };
    let host = Host::new(HostConfig { registry, session, ..b.host() });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let model = host.registry().parse_model("claude-sonnet-4-6").unwrap();
        let cmd = Command::Prompt { session_id: None, text: "one subagent".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let drain = async { while rx.recv().await.is_some() {} };
        let (r, ()) = tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(host.execute(cmd, tx), drain) }).await.expect("the subagent waited for a slot its parent holds");
        r
    });
    let result = result.unwrap().unwrap();
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.error);
    assert!(m.seen.lock().unwrap().iter().any(|s| is_child(&s.body)), "the subagent ran");
}
