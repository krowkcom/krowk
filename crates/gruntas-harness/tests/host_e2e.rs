//! A whole headless session against the fake `claude` (`common/fake.rs`):
//! a prompt that reads a file, a resumed follow-up, and what both leave
//! behind — the log and the rows krowk.db derives from it — and an image
//! kept beside the log.

#[path = "common/fake.rs"]
mod fake;

use gruntas_harness::catalog::ModelInfo;
use gruntas_harness::headless::{self, OutputFormat};
use gruntas_harness::host::HostConfig;
use gruntas_harness::instances::{InstancesConfig, Registry};
use gruntas_harness::log;
use gruntas_harness::protocol::{Item, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine, TurnStatus, Usage};
use gruntas_import::Source;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Home {
    root: PathBuf,
    instances: InstancesConfig,
}

impl Home {
    /// A home with no instance yet.
    fn new(name: &str) -> Home {
        let root = std::env::temp_dir().join(format!("gruntas-harness-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        Home { root, instances: InstancesConfig::default() }
    }

    /// A home whose one instance, `claude`, is the fake playing `scenario`.
    fn fake(name: &str, scenario: Option<&str>) -> Home {
        let mut home = Home::new(name);
        home.instances = InstancesConfig { instances: [("claude".to_string(), fake::claude(&home.root, "claude", scenario))].into_iter().collect(), ..Default::default() };
        home
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "PATH" => std::env::var("PATH").unwrap_or_default(),
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
            registry: Registry::resolve(&self.instances, &self.env()),
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
            credentials: self.root.join("home/.gruntas/credentials.json"),
            trust: gruntas_harness::trust::allow_all(),
            publisher: None,
            permissions: Default::default(),
            agents: gruntas_harness::agents::AgentsConfig::none(),
            session: Default::default(),
        }
    }

    fn run_on(&self, prompt: &str, resume: Option<&str>, model: &str, toolset: Option<&str>) -> (Vec<StreamLine>, RunResult) {
        self.run_as(prompt, resume, model, toolset, PermissionMode::Default)
    }

    fn run_as(&self, prompt: &str, resume: Option<&str>, model: &str, toolset: Option<&str>, permission_mode: PermissionMode) -> (Vec<StreamLine>, RunResult) {
        let mut out = Vec::new();
        let reg = Registry::resolve(&self.instances, &self.env());
        let opts = headless::Options {
            rollover: None,
            images: Vec::new(),
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

/// What Claude Code does with "read README.md": its Read call, the file's
/// text as its result, then the answer; asked the language, one more
/// answer (in the resumed process, which plays the scenario afresh).
fn reads_the_readme() -> String {
    let call = serde_json::json!({"type": "assistant", "message": {"id": "msg_read", "model": "{{MODEL}}", "role": "assistant", "content": [{"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "README.md"}}], "usage": {"input_tokens": 9, "output_tokens": 20}}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"});
    let read = serde_json::json!({"type": "user", "message": {"role": "user", "content": [{"tool_use_id": "toolu_read", "type": "tool_result", "content": "# krowk\n\nPermalinks for agent output.\n"}]}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"});
    format!("@when what language\n{}\n---\n{call}\n{read}\n{}", fake::answer("It is written in Rust."), fake::answer("krowk turns agent output into permalinks."))
}

/// R-LOG-1, R-LOG-2, R-LOG-5: a session on Claude Code, through the host:
/// its turn streams items, a resume continues it, every log line validates
/// and hangs from the one before, and the log projects into krowk.db, a
/// rebuild from the JSONL alone giving the same rows.
#[test]
fn a_session_streams_its_items_resumes_and_projects_its_log_into_krowk_db() {
    let home = Home::fake("session-fake", Some(&reads_the_readme()));
    let (lines, first) = home.run_on("read README.md and summarise it in one line", None, "claude/sonnet", None);
    assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.error);
    assert_eq!(first.result, "krowk turns agent output into permalinks.");
    assert!(matches!(lines.last(), Some(StreamLine::Live(LiveEvent::Result(r))) if *r == first), "the stream ends with the result");
    let read = lines.iter().any(|l| matches!(l, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, is_error: false, .. }, .. }, .. }) if output.contains("Permalinks for agent output.")));
    assert!(read, "the read's result is logged");

    let (_, second) = home.run_on("what language is it written in?", Some(&first.session_id), "claude/sonnet", None);
    assert_eq!(second.session_id, first.session_id);
    assert_eq!(second.result, "It is written in Rust.");

    let dir = log::sessions_dir(&home.env()).unwrap().join(&first.session_id);
    assert_log_chain(&dir, &lines, &first);

    let env = home.env();
    let listed = project_all(&env);
    let row = listed.iter().find(|r| r.id == first.session_id).expect("the session in the listing, under the log's own id");
    assert_eq!((row.turn_count, row.foreign_session_id.as_str()), (2, first.session_id.as_str()));
    assert_eq!(row.title, "read README.md and summarise it in one line");
    let before = detail(&env, &row.id);
    std::fs::remove_file(gruntas_store::db_path(&env).unwrap()).unwrap();
    let rebuilt = project_all(&env);
    let row2 = rebuilt.iter().find(|r| r.id == first.session_id).expect("and under the same id");
    assert_eq!(detail(&env, &row2.id), before, "rebuilt from the JSONL alone");
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

fn project_all(env: &dyn Fn(&str) -> String) -> Vec<gruntas_store::SessionRow> {
    let conn = gruntas_store::open(env).unwrap();
    let w = gruntas_store::Writer::new(&conn);
    let src = gruntas_harness::project::Gruntas;
    for r in src.discover(env).unwrap() {
        let (th, cursor, _) = src.read(env, &r, "").unwrap();
        w.ingest_with_cursor(&th, &r.key(), &cursor).unwrap();
        assert!(src.unchanged(env, &r, &cursor));
    }
    gruntas_store::list_sessions(&conn, "", "", 50).unwrap()
}

/// The session's content with the store's own ids and clocks left out.
fn detail(env: &dyn Fn(&str) -> String, id: &str) -> String {
    let conn = gruntas_store::open(env).unwrap();
    let d = gruntas_store::load_session_detail(&conn, id).unwrap();
    let turns: Vec<String> = d.turns.iter().map(|t| format!("{} {} {} {} {} {}", t.status, t.input, t.output, t.cache_read, t.cache_write, t.model)).collect();
    let msgs: Vec<String> = d
        .messages
        .iter()
        .map(|m| format!("{} {} [{}]", m.role, m.model, m.parts.iter().map(|p| format!("{}:{}:{}", p.kind, p.tool_call_id, p.data)).collect::<Vec<_>>().join(", ")))
        .collect();
    format!("{}\n{}\n{}", d.session.title, turns.join("\n"), msgs.join("\n"))
}

/// An interactive client draws and reads keys on a single-threaded
/// runtime; a turn whose key comes from a slow command (a password manager
/// asking gpg-agent) must not stop it. Here a ticker on the same
/// current-thread runtime keeps ticking through a turn on a key's Claude
/// Code account whose command takes 1.5 s, and the key it printed is what
/// Claude Code is handed (R-CRED-1, R-NAT-2).
#[test]
fn r_cred_1_a_stored_keys_command_never_blocks_the_hosts_runtime() {
    let mut home = Home::new("keycmd");
    let creds = home.root.join("home/.gruntas/credentials.json");
    std::fs::create_dir_all(creds.parent().unwrap()).unwrap();
    std::fs::write(&creds, serde_json::json!({"version": 1, "keys": {"anthropic": {"command": "sleep 1.5; echo sk-from-command"}}}).to_string()).unwrap();
    // A gateway's account: the key goes on as ANTHROPIC_AUTH_TOKEN, which
    // the fake logs whole.
    let gruntas_harness::instances::InstanceKind::ClaudeCode { binary, config_dir, mut env, args, effort, .. } = fake::claude(&home.root, "anthropic", None) else { unreachable!() };
    env.insert("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:9".into());
    let kind = gruntas_harness::instances::InstanceKind::ClaudeCode { binary, config_dir, env, args, api_key_env: Some("ANTHROPIC_API_KEY".into()), effort };
    home.instances = InstancesConfig { instances: [("anthropic".to_string(), kind)].into_iter().collect(), keys_from: Some(creds), ..Default::default() };
    let cfg = home.config();
    let model = ModelRef { instance: "anthropic".into(), model: "sonnet".into() };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (ticks, took, result) = rt.block_on(async {
        let host = gruntas_harness::host::Host::new(cfg);
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
        let cmd = gruntas_harness::protocol::Command::Prompt { session_id: None, text: "hi".into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, rollover: None, budget: None };
        let r = host.execute(cmd, tx).await;
        ticker.abort();
        let took = started.elapsed();
        host.shutdown().await;
        (ticks.load(std::sync::atomic::Ordering::Relaxed), took, r)
    });
    let r = result.unwrap_or_else(|e| panic!("{e:?}")).unwrap();
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    assert!(took >= std::time::Duration::from_millis(1400), "the command ran: {took:?}");
    // A tick every 50 ms through the whole turn, give or take a loaded
    // machine: a runtime the command blocked misses its 1.5 s of them.
    assert!(ticks as u128 >= took.as_millis() / 50 * 3 / 4, "the runtime kept running while the command did: {ticks} ticks in {took:?}");
    assert!(fake::log(&home.root, "anthropic").contains("env ANTHROPIC_AUTH_TOKEN=sk-from-command"), "the command's key is handed to Claude Code: {}", fake::log(&home.root, "anthropic"));
}

#[test]
fn an_image_sent_with_a_prompt_is_kept_beside_the_log_and_reaches_the_model() {
    use base64::Engine as _;
    use gruntas_harness::host::Host;
    use gruntas_harness::protocol::{Command, ImageInput};
    use tokio::sync::mpsc;
    let home = Home::fake("image", Some(&fake::answer("A red dot.")));
    let host = Host::new(home.config());
    let model = ModelRef { instance: "claude".into(), model: "sonnet".into() };
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0";
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let prompt = |images: Vec<ImageInput>| Command::Prompt { session_id: None, text: "what is in [Image #1]?".into(), images, model: Some(model.clone()), permission_mode: PermissionMode::Default, toolset: None, effort: None, rollover: None, budget: None };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let sessions = log::sessions_dir(&home.env()).unwrap();
    // A file that is not the image it says is refused before any session is made.
    let (tx, _rx) = mpsc::channel(1024);
    let bad = ImageInput { number: 1, media_type: "image/jpeg".into(), data: data.clone(), sha256: None };
    assert_eq!(rt.block_on(host.execute(prompt(vec![bad]), tx)).unwrap_err().code, "bad_image");
    assert!(log::list(&sessions).unwrap_or_default().is_empty(), "no session for a refused prompt");
    // Nor for one the catalog says the model cannot read.
    let blind = Host::new(HostConfig { catalog: Arc::new(|_, _| Some(ModelInfo { images: Some(false), ..ModelInfo::default() })), ..home.config() });
    let (tx, _rx) = mpsc::channel(1024);
    let image = ImageInput { number: 1, media_type: "image/png".into(), data: data.clone(), sha256: None };
    assert_eq!(rt.block_on(blind.execute(prompt(vec![image]), tx)).unwrap_err().code, "model_reads_no_images");
    assert!(log::list(&sessions).unwrap_or_default().is_empty(), "no session for a refused prompt");

    let (tx, mut rx) = mpsc::channel(1024);
    let image = ImageInput { number: 1, media_type: "image/png".into(), data: data.clone(), sha256: None };
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
    let sent = fake::prompts(&home.root, "claude");
    let content = &sent[0]["message"]["content"];
    // The prompt, its label, then the image it names.
    assert_eq!(content[1]["text"], "[Image #1]", "{content}");
    assert_eq!(content[2]["type"], "image", "{content}");
    assert_eq!(content[2]["source"]["data"], data.as_str(), "{content}");
}

