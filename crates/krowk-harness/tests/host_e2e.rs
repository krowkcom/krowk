//! A whole headless session against a stand-in Anthropic API: a prompt that
//! reads a file, a resumed follow-up, and what both leave behind — the log,
//! its context record, and the rows krowk.db derives from it.

#[path = "common/mock.rs"]
mod mock;

use krowk_harness::catalog::ModelInfo;
use krowk_harness::headless::{self, OutputFormat};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{ContextRecord, Item, LiveEvent, LogBody, LogEvent, PermissionMode, RunResult, StreamLine, TurnStatus, Usage};
use krowk_import::Source;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SIGNATURE: &str = "EqQBCkYIBxgCKkD3xG+4n0t/7i2rQzYkWmV9pL+/aX0c3R8s1QvT2nB4k==/+Zq9Hc8JtUe0yW5rN6mF7gD2lXoPiAv+EQ==";

struct Home {
    root: PathBuf,
    url: String,
}

impl Home {
    fn new(name: &str, url: &str) -> Home {
        let root = std::env::temp_dir().join(format!("krowk-harness-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        Home { root, url: url.into() }
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "ANTHROPIC_API_KEY" => "sk-test".into(),
            "ANTHROPIC_BASE_URL" => self.url.clone(),
            _ => String::new(),
        }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn config(&self) -> HostConfig {
        HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.repo(),
            registry: Registry::resolve(&InstancesConfig::default(), &self.env()),
            krowk_version: "test".into(),
            // Sonnet's list prices, per million tokens.
            pricer: Arc::new(|_, model, u: &Usage| {
                (model == "claude-sonnet-4-6").then(|| {
                    (u.input_tokens as f64 * 3.0 + u.output_tokens as f64 * 15.0 + u.cache_read_tokens as f64 * 0.3 + u.cache_write_tokens as f64 * 3.75) / 1e6
                })
            }),
            // A catalog that knows one model a router serves under a name
            // that says nothing of its family.
            catalog: Arc::new(|_, model| (model == "house-coder").then(|| ModelInfo { family: Some("grok-build".into()), ..ModelInfo::default() })),
            credentials: self.root.join("home/.krowk/credentials.json"),
            trust: krowk_harness::trust::allow_all(),
            publisher: None,
            permissions: Default::default(),
            agents: krowk_harness::subagent::AgentsConfig::none(),
        }
    }

    fn run(&self, prompt: &str, resume: Option<&str>) -> (Vec<StreamLine>, RunResult) {
        self.run_on(prompt, resume, "claude-sonnet-4-6", None)
    }

    fn run_on(&self, prompt: &str, resume: Option<&str>, model: &str, toolset: Option<&str>) -> (Vec<StreamLine>, RunResult) {
        self.run_as(prompt, resume, model, toolset, PermissionMode::Default)
    }

    fn run_as(&self, prompt: &str, resume: Option<&str>, model: &str, toolset: Option<&str>, permission_mode: PermissionMode) -> (Vec<StreamLine>, RunResult) {
        let mut out = Vec::new();
        let reg = Registry::resolve(&InstancesConfig::default(), &self.env());
        let opts = headless::Options {
            prompt: prompt.into(),
            resume: resume.map(String::from),
            model: Some(reg.parse_model(model).unwrap()),
            permission_mode,
            toolset: toolset.map(String::from),
            effort: None,
            budget: None,
            format: OutputFormat::StreamJson,
        };
        let outcome = headless::run(self.config(), opts, &mut out);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let lines: Vec<StreamLine> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}"))).collect();
        (lines, outcome.result.unwrap())
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn schema(name: &str) -> jsonschema::Validator {
    let raw = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("schema").join(name)).unwrap();
    jsonschema::validator_for(&serde_json::from_str(&raw).unwrap()).unwrap()
}

#[test]
fn a_prompt_reads_a_file_streams_its_items_and_a_resume_continues_on_the_cache() {
    let m = mock::serve(mock::readme_script);
    let home = Home::new("session", &m.url);

    // The first prompt: a read tool call, then the answer.
    let (lines, first) = home.run("read README.md and summarise it in one line", None);
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(first.result, "krowk turns agent output — screenshots, logs, diffs — into permalinks you can paste anywhere.");
    assert_eq!(first.num_model_calls, 2);
    assert_eq!(first.usage, Usage { input_tokens: 21, output_tokens: 107, cache_read_tokens: 2350, cache_write_tokens: 3760, reasoning_tokens: 0 });
    assert!(first.cost_usd.is_some_and(|c| c > 0.0));
    let started = lines.iter().filter(|l| matches!(l, StreamLine::Live(LiveEvent::ItemStarted { .. }))).count();
    let deltas = lines.iter().filter(|l| matches!(l, StreamLine::Live(LiveEvent::ItemDelta { .. }))).count();
    assert!(started >= 5 && deltas >= 7, "start and delta frames stream: {started} {deltas}");
    assert!(matches!(lines.last(), Some(StreamLine::Live(LiveEvent::Result(r))) if *r == first), "the stream ends with the result");
    let result_read = lines.iter().any(|l| {
        matches!(l, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, is_error: false, .. }, .. }, .. }) if output.contains("Permalinks for agent output."))
    });
    assert!(result_read, "the read tool ran against the working directory");

    // The second call carried the tool result, and the thinking block with
    // its signature untouched.
    {
        let seen = m.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let msgs = seen[1].body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"][0]["signature"], SIGNATURE);
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert!(seen[1].body["system"][0]["cache_control"].is_object());
    }

    // A resumed prompt continues the same session, rebuilt from the log.
    let (_, second) = home.run("what language is it written in?", Some(&first.session_id));
    assert_eq!(second.session_id, first.session_id);
    assert_eq!(second.result, "It is written in Rust.");
    assert!(second.usage.cache_read_tokens > 0, "the resumed call reads the cached prefix");
    {
        let seen = m.seen.lock().unwrap();
        let msgs = seen[2].body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5, "prompt, tool call, tool result, answer, follow-up");
        assert_eq!(msgs[1]["content"][0]["signature"], SIGNATURE, "the signature survives the log byte for byte");
        assert_eq!(seen[2].body["system"], seen[0].body["system"], "the cached prefix is byte-identical across the resume");
        assert_eq!(seen[2].body["tools"], seen[0].body["tools"]);
    }

    // R-LOG-1: every line validates against the generated schema, carries a
    // UUIDv7 id, and hangs from the line before it; only the root has no parent.
    let dir = log::sessions_dir(&home.env()).unwrap().join(&first.session_id);
    let raw = std::fs::read_to_string(dir.join(log::EVENTS_FILE)).unwrap();
    let validator = schema("log-event.schema.json");
    let events: Vec<LogEvent> = raw
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(validator.is_valid(&v), "{l}");
            serde_json::from_value(v).unwrap()
        })
        .collect();
    assert!(events.iter().all(|e| log::valid_id(&e.id)));
    assert!(events[0].parent_id.is_none() && events[0].id == first.session_id);
    for w in events.windows(2) {
        assert_eq!(w[1].parent_id.as_deref(), Some(w[0].id.as_str()));
    }
    let turns = events.iter().filter(|e| matches!(e.body, LogBody::TurnCompleted { .. })).count();
    assert_eq!(turns, 2);
    // Every stream line validates against the stream schema too.
    let stream = schema("stream-line.schema.json");
    for l in &lines {
        assert!(stream.is_valid(&serde_json::to_value(l).unwrap()), "{l:?}");
    }

    // R-LOG-4: each turn's exact system prompt and tools, beside the log.
    let ctx: Vec<ContextRecord> = std::fs::read_to_string(dir.join(log::CONTEXT_FILE)).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(ctx.len(), 2);
    assert_eq!(ctx[0].system, seen_system(&m));
    assert_eq!(ctx[0].tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["read", "write", "str_replace", "bash", "grep", "glob", "todo_write", "publish", "subagent"]);
    assert_eq!(ctx[0].toolset, "claude");
    assert!(ctx[0].system_tokens > 0 && ctx[0].tools_tokens > ctx[0].system_tokens, "{} {}", ctx[0].system_tokens, ctx[0].tools_tokens);
    let context_schema = schema("context-record.schema.json");
    for l in std::fs::read_to_string(dir.join(log::CONTEXT_FILE)).unwrap().lines() {
        assert!(context_schema.is_valid(&serde_json::from_str(l).unwrap()), "{l}");
    }

    // R-LOG-2, R-LOG-5: the log projects into krowk.db, and a rebuild from
    // the JSONL alone gives the same rows.
    let env = home.env();
    let listed = project_all(&env);
    let row = listed.iter().find(|r| r.harness == "krowk").expect("a krowk session in the listing");
    assert_eq!((row.turn_count, row.foreign_session_id.as_str()), (2, first.session_id.as_str()));
    assert_eq!(row.title, "read README.md and summarise it in one line");
    let before = detail(&env, &row.id);
    std::fs::remove_file(krowk_store::db_path(&env).unwrap()).unwrap();
    let rebuilt = project_all(&env);
    let row2 = rebuilt.iter().find(|r| r.harness == "krowk").unwrap();
    assert_eq!(detail(&env, &row2.id), before, "rebuilt from the JSONL alone");
}

/// R-TOOL-2: the recorded tool definitions carry each family's edit tool —
/// chosen from the model id, from the catalog's family, or by `--toolset` —
/// and the request the provider was sent carries the same tools.
#[test]
fn r_tool_2_the_recorded_tools_carry_each_model_familys_edit_tool_and_toolset_overrides_it() {
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::fixture("turn2_answer.sse")));
    let home = Home::new("toolset", &m.url);
    let cases = [
        ("claude-sonnet-4-6", None, "claude", "str_replace"),
        ("anthropic/gpt-5.1-codex", None, "gpt", "apply_patch"),
        ("anthropic/openai/gpt-5", None, "gpt", "apply_patch"),
        ("anthropic/grok-code-fast-1", None, "grok", "search_replace"),
        // Known to the catalog only, as a grok-build.
        ("house-coder", None, "grok", "search_replace"),
        // An unknown family gets the plain JSON edit tool.
        ("llama-4-maverick", None, "claude", "str_replace"),
        // --toolset wins over the family, either way round.
        ("claude-sonnet-4-6", Some("gpt"), "gpt", "apply_patch"),
        ("anthropic/gpt-5", Some("grok"), "grok", "search_replace"),
    ];
    for (n, (model, toolset, preset, edit)) in cases.iter().enumerate() {
        let (_, r) = home.run_on("hello", None, model, *toolset);
        assert_eq!(r.status, TurnStatus::Completed, "{model}");
        let dir = log::sessions_dir(&home.env()).unwrap().join(&r.session_id);
        let ctx: ContextRecord = serde_json::from_str(std::fs::read_to_string(dir.join(log::CONTEXT_FILE)).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(ctx.toolset, *preset, "{model} {toolset:?}");
        assert_eq!(ctx.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["read", "write", edit, "bash", "grep", "glob", "todo_write", "publish", "subagent"], "{model} {toolset:?}");
        assert!(ctx.system.contains(&format!("Change existing files with {edit};")), "the system prompt names the edit tool");
        // No grammar tool on the Messages API: apply_patch is a JSON function there.
        assert!(ctx.tools.iter().all(|t| t.grammar.is_none() && t.input_schema["type"] == "object"));
        let seen = m.seen.lock().unwrap();
        let sent: Vec<&str> = seen[n].body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(sent, ["read", "write", edit, "bash", "grep", "glob", "todo_write", "publish", "subagent"], "what was recorded is what was sent");
    }
    // A toolset that does not exist is refused before any session is made.
    let mut out = Vec::new();
    let opts = headless::Options {
        prompt: "hello".into(),
        resume: None,
        model: None,
        permission_mode: PermissionMode::Default,
        toolset: Some("vim".into()),
        effort: None,
        budget: None,
        format: OutputFormat::Json,
    };
    let outcome = headless::run(home.config(), opts, &mut out);
    let e = outcome.error.expect("refused");
    assert_eq!(e.code, "bad_toolset");
    assert!(e.message.contains("claude, gpt, grok"), "{}", e.message);
    assert!(outcome.session_id.is_none());
}

/// Each family's model edits a file through the whole loop in its own
/// format, and the edit lands; where edits are not allowed, it does not.
#[test]
fn r_tool_2_each_edit_format_runs_through_the_loop() {
    let m = mock::serve(mock::edit_script);
    let home = Home::new("edit-loop", &m.url);
    let readme = home.repo().join("README.md");
    for (model, edit) in [("claude-sonnet-4-6", "str_replace"), ("anthropic/gpt-5.1-codex", "apply_patch"), ("anthropic/grok-code-fast-1", "search_replace")] {
        std::fs::write(&readme, "# krowk\n\nPermalinks for agent output.\n").unwrap();
        let (lines, r) = home.run_as("reword the README", None, model, None, PermissionMode::AcceptEdits);
        assert_eq!(r.status, TurnStatus::Completed, "{model}");
        let result = lines.iter().find_map(|l| match l {
            StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, is_error, .. }, .. }, .. }) => Some((output.clone(), *is_error)),
            _ => None,
        });
        let (output, is_error) = result.expect("a tool result");
        assert!(!is_error, "{edit}: {output}");
        assert_eq!(std::fs::read_to_string(&readme).unwrap(), "# krowk\n\nPermalinks for everything agents make.\n", "{edit}");
        let call = lines.iter().any(|l| matches!(l, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolCall { name, .. }, .. }, .. }) if name == edit));
        assert!(call, "the call was logged as {edit}");
    }
    // In the default mode the edit is refused, and the model is told why.
    std::fs::write(&readme, "# krowk\n\nPermalinks for agent output.\n").unwrap();
    let (lines, _) = home.run_as("reword the README", None, "anthropic/gpt-5", None, PermissionMode::Default);
    let refused = lines.iter().any(|l| {
        matches!(l, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, is_error: true, .. }, .. }, .. }) if output.contains("acceptEdits"))
    });
    assert!(refused);
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "# krowk\n\nPermalinks for agent output.\n");
}

fn seen_system(m: &mock::Mock) -> String {
    m.seen.lock().unwrap()[0].body["system"][0]["text"].as_str().unwrap().to_string()
}

fn project_all(env: &dyn Fn(&str) -> String) -> Vec<krowk_store::SessionRow> {
    let conn = krowk_store::open(env).unwrap();
    let w = krowk_store::Writer::new(&conn);
    let src = krowk_harness::project::Krowk;
    for r in src.discover(env).unwrap() {
        let (th, cursor, _) = src.read(env, &r, "").unwrap();
        w.ingest_with_cursor(&th, &r.key(), &cursor).unwrap();
        assert!(src.unchanged(env, &r, &cursor));
    }
    krowk_store::list_sessions(&conn, "", "", 50).unwrap()
}

/// The session's content with the store's own ids and clocks left out.
fn detail(env: &dyn Fn(&str) -> String, id: &str) -> String {
    let conn = krowk_store::open(env).unwrap();
    let d = krowk_store::load_session_detail(&conn, id).unwrap();
    let turns: Vec<String> = d.turns.iter().map(|t| format!("{} {} {} {} {} {}", t.status, t.input, t.output, t.cache_read, t.cache_write, t.model)).collect();
    let msgs: Vec<String> = d
        .messages
        .iter()
        .map(|m| format!("{} {} [{}]", m.role, m.model, m.parts.iter().map(|p| format!("{}:{}:{}", p.kind, p.tool_call_id, p.data)).collect::<Vec<_>>().join(", ")))
        .collect();
    format!("{}\n{}\n{}", d.session.title, turns.join("\n"), msgs.join("\n"))
}

#[test]
fn r_proto_1_steer_joins_the_running_turn_before_its_next_model_call() {
    use krowk_harness::host::Host;
    use krowk_harness::protocol::Command;
    use tokio::sync::mpsc;
    // The first call streams slowly, so the steering lands while it runs.
    let m = mock::serve(|body, n| {
        let r = mock::readme_script(body, n);
        if n == 0 { mock::Reply::paced(r.body, std::time::Duration::from_millis(60)) } else { r }
    });
    let home = Home::new("steer", &m.url);
    let host = Host::new(home.config());
    let model = Registry::resolve(&InstancesConfig::default(), &home.env()).parse_model("claude-sonnet-4-6").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (lines, result) = rt.block_on(async {
        // No turn is running yet: there is nothing to steer.
        let (tx, _rx) = mpsc::channel(8);
        let err = host.execute(Command::Steer { session_id: "none".into(), text: "x".into() }, tx).await.unwrap_err();
        assert_eq!(err.code, "no_running_turn");

        let (tx, mut rx) = mpsc::channel(1024);
        let cmd = Command::Prompt { session_id: None, text: "read README.md and summarise it in one line".into(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let mut lines = Vec::new();
        let mut steered = false;
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    if !steered && let StreamLine::Live(LiveEvent::ItemStarted { session_id, .. }) = &line {
                        let (tx, _rx) = mpsc::channel(8);
                        host.execute(Command::Steer { session_id: session_id.clone(), text: "and say which licence it has".into() }, tx).await.unwrap();
                        steered = true;
                    }
                    lines.push(line);
                }
                r = &mut exec => break r.unwrap().unwrap(),
            }
        };
        while let Ok(l) = rx.try_recv() {
            lines.push(l);
        }
        (lines, result)
    });
    assert_eq!(result.status, TurnStatus::Completed);
    // The model read it on its next call, after the tool's result.
    let seen = m.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one call per step, and no extra one");
    let last = seen[1].body["messages"].as_array().unwrap().last().unwrap().clone();
    let blocks: Vec<&str> = last["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(blocks, ["tool_result", "text"], "{last}");
    assert_eq!(last["content"][1]["text"], "and say which licence it has");
    // And the log shows where in the turn it landed.
    let kinds: Vec<String> = lines
        .iter()
        .filter_map(|l| match l {
            StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item, .. }, .. }) => Some(serde_json::to_value(item).unwrap()["kind"].as_str().unwrap().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["userText", "reasoning", "assistantText", "toolCall", "toolResult", "userText", "assistantText"]);
}

#[test]
fn r_proto_1_steering_an_interrupted_turn_never_read_comes_back_on_its_result() {
    use krowk_harness::host::Host;
    use krowk_harness::protocol::Command;
    use tokio::sync::mpsc;
    // A provider that takes the request and never answers: the turn waits.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for c in l.incoming().flatten() {
            held.push(c);
        }
    });
    let home = Home::new("steer-unread", &url);
    let host = Host::new(home.config());
    let model = Registry::resolve(&InstancesConfig::default(), &home.env()).parse_model("claude-sonnet-4-6").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(1024);
        let cmd = Command::Prompt { session_id: None, text: "wait".into(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let mut session = None;
        loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    if let StreamLine::Log(LogEvent { body: LogBody::TurnStarted { .. }, session_id, .. }) = &line {
                        session = Some(session_id.clone());
                    }
                    if let Some(id) = session.take() {
                        // Let the request go out, then steer and interrupt.
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        let (t, _r) = mpsc::channel(8);
                        host.execute(Command::Steer { session_id: id.clone(), text: "and the docs".into() }, t).await.unwrap();
                        let (t, _r) = mpsc::channel(8);
                        host.execute(Command::Interrupt { session_id: id }, t).await.unwrap();
                    }
                }
                r = &mut exec => break r.unwrap().unwrap(),
            }
        }
    });
    assert_eq!(result.status, TurnStatus::Interrupted);
    assert_eq!(result.unread_steers, ["and the docs"], "handed back, not dropped");
    let json = serde_json::to_value(LiveEvent::Result(result)).unwrap();
    assert_eq!(json["unreadSteers"], serde_json::json!(["and the docs"]), "in the stream's result too");
}
