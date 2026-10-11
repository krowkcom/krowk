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
            session: Default::default(),
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
            worktree: None,
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
    assert_first_stream(&lines, &first);

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

    let dir = log::sessions_dir(&home.env()).unwrap().join(&first.session_id);
    assert_log_chain(&dir, &lines, &first);
    assert_context_records(&dir, &m);

    // R-LOG-2, R-LOG-5: the log projects into krowk.db, and a rebuild from
    // the JSONL alone gives the same rows.
    let env = home.env();
    let listed = project_all(&env);
    let row = listed.iter().find(|r| r.harness == "krowk").expect("a krowk session in the listing");
    assert_eq!((row.turn_count, row.foreign_session_id.as_str()), (2, first.session_id.as_str()));
    assert_eq!(row.title, "read README.md and summarise it in one line");
    // One id everywhere: the row the person lists is the log's session,
    // which `krowk sync host` and `--resume` name.
    assert_eq!(row.id, first.session_id, "stored under the log's own id");
    let before = detail(&env, &row.id);
    std::fs::remove_file(krowk_store::db_path(&env).unwrap()).unwrap();
    let rebuilt = project_all(&env);
    let row2 = rebuilt.iter().find(|r| r.harness == "krowk").unwrap();
    assert_eq!(detail(&env, &row2.id), before, "rebuilt from the JSONL alone");
    assert_eq!(row2.id, first.session_id, "and under the same id");
}

/// The first turn's stream: start and delta frames, the result last, and the
/// read tool's output from the working directory.
fn assert_first_stream(lines: &[StreamLine], first: &RunResult) {
    let started = lines.iter().filter(|l| matches!(l, StreamLine::Live(LiveEvent::ItemStarted { .. }))).count();
    let deltas = lines.iter().filter(|l| matches!(l, StreamLine::Live(LiveEvent::ItemDelta { .. }))).count();
    assert!(started >= 5 && deltas >= 7, "start and delta frames stream: {started} {deltas}");
    assert!(matches!(lines.last(), Some(StreamLine::Live(LiveEvent::Result(r))) if r == first), "the stream ends with the result");
    let result_read = lines.iter().any(|l| {
        matches!(l, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, is_error: false, .. }, .. }, .. }) if output.contains("Permalinks for agent output."))
    });
    assert!(result_read, "the read tool ran against the working directory");
}

/// R-LOG-1: every log line validates against the generated schema, carries a
/// UUIDv7 id, and hangs from the line before it; only the root has no parent.
fn assert_log_chain(dir: &Path, lines: &[StreamLine], first: &RunResult) {
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
    for l in lines {
        assert!(stream.is_valid(&serde_json::to_value(l).unwrap()), "{l:?}");
    }
}

/// R-LOG-4: each turn's exact system prompt and tools, beside the log.
fn assert_context_records(dir: &Path, m: &mock::Mock) {
    let ctx: Vec<ContextRecord> = std::fs::read_to_string(dir.join(log::CONTEXT_FILE)).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(ctx.len(), 2);
    assert_eq!(ctx[0].system, seen_system(m));
    assert_eq!(ctx[0].tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["read", "write", "str_replace", "bash", "bash_output", "kill_bash", "grep", "glob", "todo_write", "publish", "subagent"]);
    assert_eq!(ctx[0].toolset, "claude");
    assert!(ctx[0].system_tokens > 0 && ctx[0].tools_tokens > ctx[0].system_tokens, "{} {}", ctx[0].system_tokens, ctx[0].tools_tokens);
    let context_schema = schema("context-record.schema.json");
    for l in std::fs::read_to_string(dir.join(log::CONTEXT_FILE)).unwrap().lines() {
        assert!(context_schema.is_valid(&serde_json::from_str(l).unwrap()), "{l}");
    }
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
        assert_eq!(ctx.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["read", "write", edit, "bash", "bash_output", "kill_bash", "grep", "glob", "todo_write", "publish", "subagent"], "{model} {toolset:?}");
        assert!(ctx.system.contains(&format!("Change existing files with {edit};")), "the system prompt names the edit tool");
        // No grammar tool on the Messages API: apply_patch is a JSON function there.
        assert!(ctx.tools.iter().all(|t| t.grammar.is_none() && t.input_schema["type"] == "object"));
        let seen = m.seen.lock().unwrap();
        let sent: Vec<&str> = seen[n].body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(sent, ["read", "write", edit, "bash", "bash_output", "kill_bash", "grep", "glob", "todo_write", "publish", "subagent"], "what was recorded is what was sent");
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
        worktree: None,
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
        let err = host.execute(Command::Steer { session_id: "none".into(), text: "x".into(), images: Vec::new() }, tx).await.unwrap_err();
        assert_eq!(err.code, "no_running_turn");

        let (tx, mut rx) = mpsc::channel(1024);
        let cmd = Command::Prompt { session_id: None, text: "read README.md and summarise it in one line".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let mut lines = Vec::new();
        let mut steered = false;
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    if !steered && let StreamLine::Live(LiveEvent::ItemStarted { session_id, .. }) = &line {
                        let (tx, _rx) = mpsc::channel(8);
                        host.execute(Command::Steer { session_id: session_id.clone(), text: "and say which licence it has".into(), images: Vec::new() }, tx).await.unwrap();
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
        let cmd = Command::Prompt { session_id: None, text: "wait".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
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
                        host.execute(Command::Steer { session_id: id.clone(), text: "and the docs".into(), images: Vec::new() }, t).await.unwrap();
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

/// R-CRED-1: a stored key's `!command` runs off the host's runtime thread.
/// The TUI draws and reads keys on a single-threaded runtime; a turn whose
/// key comes from a slow command (a password manager asking gpg-agent) must
/// not stop it. Here a ticker on the same current-thread runtime keeps
/// ticking through a turn whose command takes 1.5 s.
#[test]
fn r_cred_1_a_stored_keys_command_never_blocks_the_hosts_runtime() {
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::text_stream("keyed")));
    let home = Home::new("keycmd", &m.url);
    let creds = home.root.join("home/.krowk/credentials.json");
    std::fs::create_dir_all(creds.parent().unwrap()).unwrap();
    std::fs::write(&creds, serde_json::json!({"version": 1, "keys": {"anthropic": {"command": "sleep 1.5; echo sk-from-command"}}}).to_string()).unwrap();
    let mut cfg = home.config();
    cfg.registry = Registry::resolve(&InstancesConfig { keys_from: Some(creds), ..InstancesConfig::default() }, &home.env());
    let model = cfg.registry.parse_model("anthropic/claude-sonnet-4-6").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (ticks, took) = rt.block_on(async {
        let host = krowk_harness::host::Host::new(cfg);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let t = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                t.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let started = std::time::Instant::now();
        let cmd = krowk_harness::protocol::Command::Prompt { session_id: None, text: "hi".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let r = host.execute(cmd, tx).await;
        ticker.abort();
        assert!(r.is_ok(), "{:?}", r.err());
        (ticks.load(std::sync::atomic::Ordering::Relaxed), started.elapsed())
    });
    assert!(took >= std::time::Duration::from_millis(1400), "the command ran: {took:?}");
    assert!(ticks >= 15, "the runtime kept running while the command did: {ticks} ticks in {took:?}");
    assert_eq!(m.seen.lock().unwrap()[0].header("x-api-key"), Some("sk-from-command"));
}

#[test]
fn an_image_sent_with_a_prompt_is_kept_beside_the_log_and_reaches_the_model() {
    use base64::Engine as _;
    use krowk_harness::host::Host;
    use krowk_harness::protocol::{Command, ImageInput};
    use tokio::sync::mpsc;
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::text_stream("A red dot.")));
    let home = Home::new("image", &m.url);
    let host = Host::new(home.config());
    let model = Registry::resolve(&InstancesConfig::default(), &home.env()).parse_model("claude-sonnet-4-6").unwrap();
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0";
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let prompt = |images: Vec<ImageInput>| Command::Prompt { session_id: None, text: "what is in [Image #1]?".into(), images, model: Some(model.clone()), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let sessions = log::sessions_dir(&home.env()).unwrap();
    // A file that is not the image it says is refused before any session is made.
    let (tx, _rx) = mpsc::channel(1024);
    let bad = ImageInput { number: 1, media_type: "image/jpeg".into(), data: data.clone() };
    assert_eq!(rt.block_on(host.execute(prompt(vec![bad]), tx)).unwrap_err().code, "bad_image");
    assert!(log::list(&sessions).unwrap_or_default().is_empty(), "no session for a refused prompt");
    // Nor for one the catalog says the model cannot read.
    let blind = Host::new(HostConfig { catalog: Arc::new(|_, _| Some(ModelInfo { images: Some(false), ..ModelInfo::default() })), ..home.config() });
    let (tx, _rx) = mpsc::channel(1024);
    let image = ImageInput { number: 1, media_type: "image/png".into(), data: data.clone() };
    assert_eq!(rt.block_on(blind.execute(prompt(vec![image]), tx)).unwrap_err().code, "model_reads_no_images");
    assert!(log::list(&sessions).unwrap_or_default().is_empty(), "no session for a refused prompt");

    let (tx, mut rx) = mpsc::channel(1024);
    let image = ImageInput { number: 1, media_type: "image/png".into(), data: data.clone() };
    let result = rt.block_on(host.execute(prompt(vec![image]), tx)).unwrap().unwrap();
    assert_eq!(result.status, TurnStatus::Completed);
    let mut logged = None;
    while let Ok(l) = rx.try_recv() {
        if let StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::UserText { images, .. }, .. }, .. }) = l {
            logged = Some(images);
        }
    }
    let refs = logged.expect("the prompt is logged");
    assert_eq!(refs.len(), 1);
    assert_eq!((refs[0].number, refs[0].media_type.as_str()), (1, "image/png"));
    let file = sessions.join(&result.session_id).join("images").join(&refs[0].file);
    assert_eq!(std::fs::read(&file).unwrap(), png, "the bytes, kept as sent");
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&file).unwrap().permissions()) & 0o777, 0o600);
    let seen = m.seen.lock().unwrap();
    let content = &seen[0].body["messages"][0]["content"];
    assert_eq!(content[1]["text"], "[Image #1]");
    assert_eq!(content[2]["source"]["data"], data.as_str(), "{content}");
}

/// The last message of a request, as JSON text.
fn last_message(m: &mock::Mock, n: usize) -> String {
    m.seen.lock().unwrap()[n].body["messages"].as_array().unwrap().last().unwrap().to_string()
}

/// R-STEER-2: a command started in the background is answered at once; a
/// job that ends while the turn runs reaches the model as a note at its next
/// call, logged where it landed; and `bash_output` reads what it printed.
#[test]
fn r_steer_2_a_background_job_answers_at_once_and_its_end_reaches_the_next_call() {
    let m = mock::serve(|_, n| {
        mock::Reply::sse(&match n {
            0 => mock::tool_use("t1", "bash", &serde_json::json!({"command": "sleep 1; echo hi", "run_in_background": true})),
            1 => mock::tool_use("t2", "bash", &serde_json::json!({"command": "sleep 2"})),
            2 => mock::tool_use("t3", "bash_output", &serde_json::json!({"id": "b1"})),
            _ => mock::text_stream("done"),
        })
    });
    let home = Home::new("bg-job", &m.url);
    let started = std::time::Instant::now();
    let (lines, result) = home.run_as("run it in the background", None, "claude-sonnet-4-6", None, PermissionMode::BypassPermissions);
    assert_eq!(result.status, TurnStatus::Completed, "{result:?}");
    let tools: Vec<String> = m.seen.lock().unwrap()[0].body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    let bash = tools.iter().position(|t| t == "bash").unwrap();
    assert_eq!(tools[bash + 1..bash + 3], ["bash_output", "kill_bash"], "offered after bash: {tools:?}");
    let first = last_message(&m, 1);
    assert!(first.contains("started background job b1"), "{first}");
    let noted = last_message(&m, 2);
    assert!(noted.contains("<background-done id=\\\"b1\\\" status=\\\"exited 0\\\">\\nhi\\n</background-done>"), "the note, read at the next call: {noted}");
    let read = last_message(&m, 3);
    assert!(read.contains("hi\\nexited 0"), "{read}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    // Logged where it landed: after the call it ended during.
    let items: Vec<Item> = lines.iter().filter_map(|l| if let StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item, .. }, .. }) = l { Some(item.clone()) } else { None }).collect();
    let note = items.iter().position(|i| matches!(i, Item::UserText { text, .. } if text.starts_with("<background-done"))).expect("logged");
    assert!(matches!(&items[note - 1], Item::ToolResult { output, .. } if output.contains("exit code 0")), "{items:?}");
}

/// R-STEER-2: `kill_bash` stops a running job, and the job says so.
#[test]
fn r_steer_2_kill_bash_stops_a_running_job() {
    let m = mock::serve(|_, n| {
        mock::Reply::sse(&match n {
            0 => mock::tool_use("t1", "bash", &serde_json::json!({"command": "sleep 30", "run_in_background": true})),
            1 => mock::tool_use("t2", "kill_bash", &serde_json::json!({"id": "b1"})),
            2 => mock::tool_use("t3", "bash", &serde_json::json!({"command": "sleep 0.5"})),
            3 => mock::tool_use("t4", "bash_output", &serde_json::json!({"id": "b1"})),
            _ => mock::text_stream("done"),
        })
    });
    let home = Home::new("bg-kill", &m.url);
    let started = std::time::Instant::now();
    let (_, result) = home.run_as("start and stop it", None, "claude-sonnet-4-6", None, PermissionMode::BypassPermissions);
    assert_eq!(result.status, TurnStatus::Completed);
    assert!(last_message(&m, 2).contains("stopped background job b1"));
    let read = last_message(&m, 4);
    assert!(read.contains("killed"), "{read}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "not the 30 s it would have run");
}

/// Whether a request is a subagent's: its system prompt says so.
fn is_child(body: &serde_json::Value) -> bool {
    body["system"].to_string().contains("You are a subagent")
}

/// A child's answer, held before its end until `gate` opens.
fn held_child(gate: &mock::Gate) -> mock::Reply {
    mock::Reply { hold: Some(("message_stop", gate.clone())), ..mock::Reply::paced(mock::text_stream("The repo has one README."), std::time::Duration::from_millis(1)) }
}

/// The parent's requests, in order.
fn parent_seen(m: &mock::Mock) -> Vec<serde_json::Value> {
    m.seen.lock().unwrap().iter().filter(|s| !is_child(&s.body)).map(|s| s.body.clone()).collect()
}

/// R-STEER-3: a parent that starts a child in the background is answered at
/// once and makes another model call while the child runs. Its model then
/// finishes first: the turn waits, reads the child's summary as a note,
/// logged where it landed, and completes after one more call. The child's
/// spend is the tree's (R-SUB-4).
#[test]
fn r_steer_3_a_background_child_answers_at_once_and_its_summary_holds_the_turn_open() {
    let gate = mock::Gate::default();
    let parent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (g, p) = (gate.clone(), parent.clone());
    let m = mock::serve(move |body, _| {
        if is_child(body) {
            return held_child(&g);
        }
        match p.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => mock::Reply::sse(&mock::tool_use("t1", "subagent", &serde_json::json!({"description": "survey", "prompt": "survey the repo", "run_in_background": true}))),
            // The child still runs: the parent's model says it is done, and
            // the child may end now.
            1 => {
                let g = g.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    g.open();
                });
                mock::Reply::sse(&mock::text_stream("I will wait for the survey."))
            }
            _ => mock::Reply::sse(&mock::text_stream("All done: the repo has one README.")),
        }
    });
    let home = Home::new("bg-child", &m.url);
    let (lines, result) = home.run("survey in the background", None);
    assert_eq!((result.status, result.result.as_str()), (TurnStatus::Completed, "All done: the repo has one README."), "{result:?}");
    let seen = parent_seen(&m);
    assert_eq!(seen.len(), 3, "one more call after the note");
    let started = seen[1]["messages"].as_array().unwrap().last().unwrap().to_string();
    let child = started.split("started background agent ").nth(1).map(|s| s[..36].to_string()).expect(&started);
    let noted = seen[2]["messages"].as_array().unwrap().last().unwrap().to_string();
    assert!(noted.contains(&format!("<background-done id=\\\"{child}\\\" status=\\\"completed\\\">\\nThe repo has one README.\\n</background-done>")), "{noted}");
    let items: Vec<Item> = lines.iter().filter_map(|l| if let StreamLine::Log(LogEvent { session_id, body: LogBody::ItemCompleted { item, .. }, .. }) = l { (*session_id == result.session_id).then(|| item.clone()) } else { None }).collect();
    let note = items.iter().position(|i| matches!(i, Item::UserText { text, .. } if text.starts_with("<background-done"))).expect("logged");
    assert!(matches!(&items[note - 1], Item::AssistantText { text } if text == "I will wait for the survey."), "after the answer it waited behind: {items:?}");
    // The tree's cost: the parent's own share is less than the turn's.
    let own = lines.iter().rev().find_map(|l| if let StreamLine::Live(LiveEvent::Cost { session_id, turn_cost_usd, .. }) = l { (*session_id == result.session_id).then_some(*turn_cost_usd) } else { None }).flatten().unwrap();
    assert!(result.cost_usd.unwrap() > own, "the child's spend is counted with the parent's: {:?} vs {own}", result.cost_usd);
}

/// R-STEER-3: interrupting the parent interrupts its background child, and
/// the child's log ends with its turn.
#[test]
fn r_steer_3_interrupting_the_parent_interrupts_its_background_child() {
    use krowk_harness::host::Host;
    use krowk_harness::protocol::Command;
    use tokio::sync::mpsc;
    let gate = mock::Gate::default();
    let parent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (g, p) = (gate.clone(), parent.clone());
    let m = mock::serve(move |body, _| {
        if is_child(body) {
            return held_child(&g);
        }
        match p.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => mock::Reply::sse(&mock::tool_use("t1", "subagent", &serde_json::json!({"description": "survey", "prompt": "survey the repo", "run_in_background": true}))),
            _ => mock::Reply::sse(&mock::text_stream("Waiting.")),
        }
    });
    let home = Home::new("bg-child-interrupt", &m.url);
    let host = Host::new(home.config());
    let model = Registry::resolve(&InstancesConfig::default(), &home.env()).parse_model("claude-sonnet-4-6").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (result, child) = rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let cmd = Command::Prompt { session_id: None, text: "survey in the background".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let (mut session, mut child, mut interrupted) = (String::new(), None, false);
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    if let StreamLine::Log(LogEvent { session_id, body: LogBody::SubagentStarted { subagent_session_id, .. }, .. }) = &line {
                        session.clone_from(session_id);
                        child = Some(subagent_session_id.clone());
                    }
                    // The parent's model has answered and waits on the child.
                    if !interrupted && child.is_some() && matches!(&line, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::AssistantText { text }, .. }, .. }) if text == "Waiting.") {
                        interrupted = true;
                        let (itx, _irx) = mpsc::channel(1);
                        host.execute(Command::Interrupt { session_id: session.clone() }, itx).await.unwrap();
                    }
                }
                r = &mut exec => break r.unwrap().unwrap(),
            }
        };
        (result, child.unwrap())
    });
    assert_eq!(result.status, TurnStatus::Interrupted);
    let events = log::read_events(&log::sessions_dir(&home.env()).unwrap().join(&child).join(log::EVENTS_FILE)).unwrap();
    assert!(matches!(&events.last().unwrap().body, LogBody::TurnCompleted { status: TurnStatus::Interrupted, .. }), "{:?}", events.last());
}

/// R-STEER-3: a parent turn that fails stops the background children it
/// leaves, rather than waiting for them; each child's log ends with its
/// turn.
#[test]
fn r_steer_3_a_failed_parent_turn_stops_its_background_children() {
    use krowk_harness::host::Host;
    use krowk_harness::protocol::Command;
    use tokio::sync::mpsc;
    let gate = mock::Gate::default();
    let parent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (g, p) = (gate.clone(), parent.clone());
    let m = mock::serve(move |body, _| {
        if is_child(body) {
            return held_child(&g);
        }
        match p.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => mock::Reply::sse(&mock::tool_use("t1", "subagent", &serde_json::json!({"description": "survey", "prompt": "survey the repo", "run_in_background": true}))),
            _ => mock::Reply::json(400, &serde_json::json!({"type": "error", "error": {"type": "invalid_request_error", "message": "no"}})),
        }
    });
    let home = Home::new("bg-child-failed", &m.url);
    let host = Host::new(home.config());
    let model = Registry::resolve(&InstancesConfig::default(), &home.env()).parse_model("claude-sonnet-4-6").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (result, child) = rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let cmd = Command::Prompt { session_id: None, text: "survey in the background".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let mut child = None;
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    Some(line) = rx.recv() => {
                        if let StreamLine::Log(LogEvent { body: LogBody::SubagentStarted { subagent_session_id, .. }, .. }) = &line {
                            child = Some(subagent_session_id.clone());
                        }
                    }
                    r = &mut exec => break r.unwrap().unwrap(),
                }
            }
        })
        .await
        .expect("the failed turn did not wait for its child");
        (result, child.unwrap())
    });
    assert_eq!(result.status, TurnStatus::Failed, "{result:?}");
    let events = log::read_events(&log::sessions_dir(&home.env()).unwrap().join(&child).join(log::EVENTS_FILE)).unwrap();
    assert!(matches!(&events.last().unwrap().body, LogBody::TurnCompleted { status: TurnStatus::Interrupted, .. }), "{:?}", events.last());
}

/// Runs a prompt, steering it with `steer` `after` the turn's first tool
/// call is logged: its result, and when that call started.
fn steered_after(home: &Home, steer: &str, after: std::time::Duration) -> (RunResult, std::time::Instant) {
    use krowk_harness::host::Host;
    use krowk_harness::protocol::Command;
    use tokio::sync::mpsc;
    let host = Host::new(home.config());
    let model = Registry::resolve(&InstancesConfig::default(), &home.env()).parse_model("claude-sonnet-4-6").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let cmd = Command::Prompt { session_id: None, text: "run it".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::BypassPermissions, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let (mut session, mut at, mut call_started) = (String::new(), None::<tokio::time::Instant>, None);
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    if let StreamLine::Log(LogEvent { session_id, body: LogBody::ItemCompleted { item: Item::ToolCall { .. }, .. }, .. }) = &line
                        && call_started.is_none()
                    {
                        session.clone_from(session_id);
                        call_started = Some(std::time::Instant::now());
                        at = Some(tokio::time::Instant::now() + after);
                    }
                }
                _ = krowk_harness::engine::sleep_until(at) => {
                    at = None;
                    let (stx, _srx) = mpsc::channel(8);
                    host.execute(Command::Steer { session_id: session.clone(), text: steer.into(), images: Vec::new() }, stx).await.unwrap();
                }
                r = &mut exec => break r.unwrap().unwrap(),
            }
        };
        host.shutdown().await;
        (result, call_started.unwrap())
    })
}

/// A mock that answers in order and keeps when each request came.
fn timed(replies: Vec<String>) -> (mock::Mock, Arc<std::sync::Mutex<Vec<std::time::Instant>>>) {
    let times = Arc::new(std::sync::Mutex::new(Vec::new()));
    let t = times.clone();
    let m = mock::serve(move |_, n| {
        t.lock().unwrap().push(std::time::Instant::now());
        mock::Reply::sse(replies.get(n).map_or_else(|| mock::text_stream("done"), Clone::clone).as_str())
    });
    (m, times)
}

/// R-STEER-4: a steer at 1 s into a long foreground command moves it to the
/// background at 2 s, nothing killed: the model's next call carries what it
/// printed so far, the move, and the steer; the job then ends on its own,
/// and what the answer showed plus `bash_output` is the whole output.
#[test]
fn r_steer_4_a_steer_moves_a_long_bash_call_to_the_background_at_two_seconds() {
    let (m, times) = timed(vec![
        mock::tool_use("t1", "bash", &serde_json::json!({"command": "echo before; sleep 3; echo after"})),
        mock::tool_use("t2", "bash", &serde_json::json!({"command": "sleep 3.5"})),
        mock::tool_use("t3", "bash_output", &serde_json::json!({"id": "b1"})),
    ]);
    let home = Home::new("move-bash", &m.url);
    let (result, call) = steered_after(&home, "stop, just say hi", std::time::Duration::from_secs(1));
    assert_eq!(result.status, TurnStatus::Completed, "{result:?}");
    let t = times.lock().unwrap().clone();
    assert!(t[1] - call < std::time::Duration::from_millis(2500), "the next call within 2.5 s of the call's start: {:?}", t[1] - call);
    let next = last_message(&m, 1);
    assert!(next.contains("before\\nmoved to background as job b1 because the person sent a message"), "{next}");
    assert!(next.contains("stop, just say hi"), "the steer, read at the same call: {next}");
    let read = last_message(&m, 3);
    assert!(read.contains("after\\nexited 0") && !read.contains("before"), "the rest, and nothing twice: {read}");
}

/// R-STEER-4: a command under 2 s just finishes; the steer waits for it.
#[test]
fn r_steer_4_a_steer_during_a_short_command_waits_for_it() {
    let (m, _) = timed(vec![mock::tool_use("t1", "bash", &serde_json::json!({"command": "sleep 1; echo done"}))]);
    let home = Home::new("short-bash", &m.url);
    let (result, _) = steered_after(&home, "and then say hi", std::time::Duration::from_millis(300));
    assert_eq!(result.status, TurnStatus::Completed);
    let next = last_message(&m, 1);
    assert!(next.contains("done\\nexit code 0") && !next.contains("moved to background"), "{next}");
    assert!(next.contains("and then say hi"), "{next}");
}

/// A response that calls several tools at once.
fn tool_uses(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let mut out = format!(
        "event: message_start\ndata: {}\n\n",
        serde_json::json!({"type": "message_start", "message": {"id": "msg_fan", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}})
    );
    for (i, (id, name, input)) in calls.iter().enumerate() {
        out += &format!("event: content_block_start\ndata: {}\n\n", serde_json::json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        out += &format!("event: content_block_delta\ndata: {}\n\n", serde_json::json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
        out += &format!("event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{i}}}\n\n");
    }
    out + "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":40}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
}

/// Two children at once, a quick one and a slow one held until `gate`;
/// the parent's own requests answered by `parent` in order.
fn two_children(gate: &mock::Gate, parent: impl Fn(usize) -> mock::Reply + Send + 'static) -> mock::Mock {
    let (g, n) = (gate.clone(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    mock::serve(move |body, _| {
        if is_child(body) {
            return match body["messages"].to_string().contains("the slow one") {
                true => mock::Reply { hold: Some(("message_stop", g.clone())), ..mock::Reply::paced(mock::text_stream("The slow one found two files."), std::time::Duration::from_millis(1)) },
                false => mock::Reply::sse(&mock::text_stream("The quick one found a README.")),
            };
        }
        match n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => mock::Reply::sse(&tool_uses(&[
                ("t1", "subagent", serde_json::json!({"description": "quick", "prompt": "the quick one: look at the README"})),
                ("t2", "subagent", serde_json::json!({"description": "slow", "prompt": "the slow one: list every file"})),
            ])),
            i => parent(i),
        }
    })
}

/// R-STEER-4: a steer at 3 s into a batch of two children moves the one
/// still running to the background: the model's next call carries the
/// quick child's summary, the slow one's move and the steer; the turn
/// waits for the slow one's note, reads it, and completes after it.
#[test]
fn r_steer_4_a_steer_moves_the_children_still_running_to_the_background() {
    let gate = mock::Gate::default();
    let g = gate.clone();
    let m = two_children(&gate, move |i| match i {
        1 => {
            let g = g.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                g.open();
            });
            mock::Reply::sse(&mock::text_stream("Noted; waiting for the slow one."))
        }
        _ => mock::Reply::sse(&mock::text_stream("Both are done.")),
    });
    let home = Home::new("move-children", &m.url);
    let (result, _) = steered_after(&home, "just tell me what the quick one found", std::time::Duration::from_secs(3));
    assert_eq!((result.status, result.result.as_str()), (TurnStatus::Completed, "Both are done."), "{result:?}");
    let seen = parent_seen(&m);
    assert_eq!(seen.len(), 3, "the move, then the note");
    let next = seen[1]["messages"].as_array().unwrap().last().unwrap().to_string();
    assert!(next.contains("The quick one found a README.") && next.contains("moved to background as agent ") && next.contains("just tell me what the quick one found"), "{next}");
    let noted = seen[2]["messages"].as_array().unwrap().last().unwrap().to_string();
    assert!(noted.contains("status=\\\"completed\\\">\\nThe slow one found two files.\\n</background-done>"), "{noted}");
}

/// R-STEER-4: interrupting after the move still interrupts the moved child.
#[test]
fn r_steer_4_interrupting_after_the_move_interrupts_the_moved_child() {
    use krowk_harness::host::Host;
    use krowk_harness::protocol::Command;
    use tokio::sync::mpsc;
    let gate = mock::Gate::default();
    let m = two_children(&gate, |_| mock::Reply::sse(&mock::text_stream("Waiting.")));
    let home = Home::new("move-children-interrupt", &m.url);
    let host = Host::new(home.config());
    let model = Registry::resolve(&InstancesConfig::default(), &home.env()).parse_model("claude-sonnet-4-6").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (result, slow) = rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let cmd = Command::Prompt { session_id: None, text: "two at once".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
        let exec = host.execute(cmd, tx);
        tokio::pin!(exec);
        let (mut session, mut slow, mut steer_at, mut stopped) = (String::new(), None, None::<tokio::time::Instant>, false);
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => match &line {
                    StreamLine::Log(LogEvent { session_id, body: LogBody::SubagentStarted { subagent_session_id, description, .. }, .. }) => {
                        session.clone_from(session_id);
                        if description == "slow" {
                            slow = Some(subagent_session_id.clone());
                            steer_at = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
                        }
                    }
                    StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::AssistantText { text }, .. }, .. }) if text == "Waiting." && !stopped => {
                        stopped = true;
                        let (itx, _irx) = mpsc::channel(1);
                        host.execute(Command::Interrupt { session_id: session.clone() }, itx).await.unwrap();
                    }
                    _ => {}
                },
                _ = krowk_harness::engine::sleep_until(steer_at) => {
                    steer_at = None;
                    let (stx, _srx) = mpsc::channel(8);
                    host.execute(Command::Steer { session_id: session.clone(), text: "stop waiting".into(), images: Vec::new() }, stx).await.unwrap();
                }
                r = &mut exec => break r.unwrap().unwrap(),
            }
        };
        (result, slow.unwrap())
    });
    assert_eq!(result.status, TurnStatus::Interrupted);
    let events = log::read_events(&log::sessions_dir(&home.env()).unwrap().join(&slow).join(log::EVENTS_FILE)).unwrap();
    assert!(matches!(&events.last().unwrap().body, LogBody::TurnCompleted { status: TurnStatus::Interrupted, .. }), "{:?}", events.last());
}
