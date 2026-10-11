//! The Claude Code backend through the host, against a fake `claude` (a
//! script replaying recorded stream-json, `fixtures/claude/fake-claude`), so
//! no test needs a real login: a turn that calls krowk's injected
//! session_info tool, a denied Bash, one process serving the whole session
//! and switching models in place, an interrupt the session survives, a
//! resume from a new host, two instances with their own config
//! directories, and the refusals — an untrusted repository, a missing
//! login, a missing binary. And what goes on between turns: a background
//! agent Claude Code runs, and the turn it begins by itself to answer it.

#![cfg(unix)]

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstanceKind, InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{
    Billing, Command, ContextRecord, Item, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine, TurnStatus, WireApi,
};
use krowk_harness::trust;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

const VENDOR_SESSION: &str = "fa4e0000-0000-4000-8000-000000000001";

/// One test at a time in this binary: a process another test is spawning
/// holds a copy of every open descriptor between its fork and its exec —
/// a session log's lock included — so a turn re-opening its log in that
/// instant would read another krowk's lock. One krowk runs one session's
/// turns; tests running several at once in one process are the exception.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude").join(name)
}

struct Home {
    root: PathBuf,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Home {
    fn new(name: &str) -> Home {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("krowk-claude-backend-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        // The fake, installed as `claude`, executable whatever git kept.
        let bin = root.join("bin/claude");
        // Linked, not copied: a copy is a file open for writing that a test
        // forking beside it can inherit, and running it then fails with
        // "Text file busy" (ETXTBSY) — read as a vendor that could not be checked.
        std::os::unix::fs::symlink(fixture("fake-claude"), &bin).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Home { root: root.canonicalize().unwrap(), _serial: guard }
    }

    fn log_file(&self) -> PathBuf {
        self.root.join("fake.log")
    }

    fn fake_log(&self) -> String {
        std::fs::read_to_string(self.log_file()).unwrap_or_default()
    }

    /// A signed-in Claude Code account: the fake's marker, never a credential.
    fn signed_in(&self, dir: &str) -> PathBuf {
        let d = self.root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("fake-login"), "").unwrap();
        d
    }

    /// An instance: the fake, a config directory, and the scenario in its
    /// environment — the instance's own env, as a router's base URL would be.
    fn instance(&self, config_dir: &Path, scenario: Option<&str>) -> InstanceKind {
        let mut env = BTreeMap::new();
        env.insert("FAKE_CLAUDE_LOG".to_string(), self.log_file().display().to_string());
        if let Some(s) = scenario {
            env.insert("FAKE_CLAUDE_SCENARIO".to_string(), fixture(s).display().to_string());
        }
        InstanceKind::ClaudeCode { binary: Some(self.root.join("bin/claude").display().to_string()), config_dir: Some(config_dir.display().to_string()), env, args: Vec::new(), api_key_env: None, effort: None }
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "PATH" => std::env::var("PATH").unwrap_or_default(),
            "ROUTER_KEY" => "sk-or-fake".into(),
            _ => String::new(),
        }
    }

    fn host(&self, instances: Vec<(&str, InstanceKind)>, gate: trust::Gate) -> Host {
        self.host_publishing(instances, gate, None)
    }

    fn host_publishing(&self, instances: Vec<(&str, InstanceKind)>, gate: trust::Gate, publisher: Option<krowk_harness::evidence::Publisher>) -> Host {
        Host::new(self.config(instances, gate, publisher))
    }

    /// A host whose clients answer approval requests, as the TUI's does.
    fn host_answering(&self, instances: Vec<(&str, InstanceKind)>) -> Host {
        let mut cfg = self.config(instances, trust::allow_all(), None);
        cfg.permissions.approvals = true;
        Host::new(cfg)
    }

    fn config(&self, instances: Vec<(&str, InstanceKind)>, gate: trust::Gate, publisher: Option<krowk_harness::evidence::Publisher>) -> HostConfig {
        let cfg = InstancesConfig { instances: instances.into_iter().map(|(n, k)| (n.to_string(), k)).collect(), ..Default::default() };
        HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.root.join("repo"),
            registry: Registry::resolve(&cfg, &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.krowk/credentials.json"),
            trust: gate,
            publisher,
            permissions: krowk_harness::permissions::Config { home: Some(self.root.join("home")), ..Default::default() },
            agents: krowk_harness::subagent::AgentsConfig::none(),
            session: Default::default(),
        }
    }

    fn events(&self, session_id: &str) -> Vec<LogEvent> {
        log::read_events(&log::sessions_dir(&self.env()).unwrap().join(session_id).join(log::EVENTS_FILE)).unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn prompt(session_id: Option<&str>, text: &str, model: &str, mode: PermissionMode) -> Command {
    prompt_within(session_id, text, model, mode, None)
}

fn prompt_within(session_id: Option<&str>, text: &str, model: &str, mode: PermissionMode, budget: Option<krowk_harness::protocol::BudgetLimits>) -> Command {
    let (instance, model) = model.split_once('/').unwrap();
    Command::Prompt {
        session_id: session_id.map(String::from),
        text: text.into(), images: Vec::new(),
        model: Some(ModelRef { instance: instance.into(), model: model.into() }),
        permission_mode: mode,
        toolset: None,
        effort: None,
        budget,
    }
}

/// Runs one command to its end, with every stream line it produced.
async fn run(host: &Host, cmd: Command) -> (Vec<StreamLine>, Result<Option<RunResult>, krowk_harness::engine::EngineError>) {
    let (tx, mut rx) = mpsc::channel(4096);
    let collect = async {
        let mut lines = Vec::new();
        while let Some(l) = rx.recv().await {
            lines.push(l);
        }
        lines
    };
    let (r, lines) = tokio::join!(host.execute(cmd, tx), collect);
    (lines, r)
}

fn completed(lines: &[StreamLine]) -> Vec<Item> {
    lines.iter().filter_map(|l| if let StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item, .. }, .. }) = l { Some(item.clone()) } else { None }).collect()
}

fn processes(fake_log: &str) -> usize {
    fake_log.lines().filter(|l| l.starts_with("argv -p ")).count()
}

#[test]
fn r_back_1_a_session_runs_on_one_claude_process_calls_krowks_tool_and_is_denied_bash() {
    let home = Home::new("session");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("session_info.jsonl")))], trust::allow_all());
    rt().block_on(async {
        // Turn 1: the model calls mcp__krowk__session_info, which krowk
        // answers over the control protocol.
        let (lines, r) = run(&host, prompt(None, "what session is this?", "claude:work/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.error);
        assert_eq!(first.result, "The session_info tool answered.");
        assert_eq!(first.num_model_calls, 2);
        assert_eq!(first.model, ModelRef { instance: "claude:work".into(), model: "sonnet".into() });
        let items = completed(&lines);
        let Item::Reasoning { blob: Some(b), text } = &items[1] else { panic!("{items:?}") };
        assert_eq!((b.provider.as_str(), b.wire_api, text.as_str()), ("anthropic", WireApi::ClaudeCode, "The person wants the session id."));
        assert!(matches!(&items[2], Item::ToolCall { name, .. } if name == "mcp__krowk__session_info"));
        let Item::ToolResult { output, is_error: false, .. } = &items[3] else { panic!("{items:?}") };
        assert!(output.contains(&format!("krowk session: {}", first.session_id)) && output.contains("instance: claude:work"), "the tool ran inside krowk, for this session: {output}");
        assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ItemDelta { .. }))), "deltas stream live");

        // Turn 2, same session: Bash is asked of krowk, and refused under
        // the default mode.
        let (_, r) = run(&host, prompt(Some(&first.session_id), "clean the build", "claude:work/sonnet", PermissionMode::Default)).await;
        let second = r.unwrap().unwrap();
        assert_eq!((second.status, second.result.as_str()), (TurnStatus::Completed, "I was not allowed to run that."));
        let events = home.events(&first.session_id);
        let denied = events.iter().any(|e| matches!(&e.body, LogBody::ItemCompleted { item: Item::ToolResult { is_error: true, output, .. }, .. } if output.contains("bypassPermissions")));
        assert!(denied, "the refusal is what Claude Code was told, and what the log has");

        // Turn 3 on another model of the same instance: switched in place.
        let (_, r) = run(&host, prompt(Some(&first.session_id), "and now?", "claude:work/haiku", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().result, "ok");
        host.shutdown().await;
    });

    let fake = home.fake_log();
    assert_eq!(processes(&fake), 1, "one process served the three turns:\n{fake}");
    assert!(fake.contains("set_model haiku"), "{fake}");
    assert!(fake.contains(&format!("config {}", dir.display())), "the instance's CLAUDE_CONFIG_DIR");
    let argv = fake.lines().find(|l| l.starts_with("argv -p ")).unwrap();
    for want in ["--input-format stream-json", "--output-format stream-json", "--verbose", "--include-partial-messages", "--permission-prompt-tool stdio", r#""type":"sdk""#, "--strict-mcp-config", "--model sonnet"] {
        assert!(argv.contains(want), "{want} missing from {argv}");
    }
    assert!(fake.contains(r#""behavior":"allow""#), "krowk's own tool is allowed");
    assert!(fake.contains(r#""name":"session_info""#), "tools/list named it");
    assert!(fake.lines().last().unwrap() == "eof", "shutdown closed stdin and the process exited");

    // R-BACK-5: the log has the Claude session behind it, once, with the
    // transcript Claude Code wrote and what it is billed to (R-INST-3).
    let sid = home.events_first_session();
    let events = home.events(&sid);
    let backend: Vec<&LogBody> = events.iter().map(|e| &e.body).filter(|b| matches!(b, LogBody::BackendSession { .. })).collect();
    assert_eq!(backend.len(), 1, "logged when new, not again: {backend:?}");
    let LogBody::BackendSession { backend, vendor_session_id, transcript_path, billing, .. } = backend[0] else { unreachable!() };
    assert_eq!((backend.as_str(), vendor_session_id.as_str(), *billing), ("claude-code", VENDOR_SESSION, Some(Billing::Subscription)));
    let transcript = PathBuf::from(transcript_path.as_ref().unwrap());
    assert!(transcript.starts_with(&dir) && transcript.is_file(), "{transcript:?}");
    let turns: Vec<&LogBody> = events.iter().map(|e| &e.body).filter(|b| matches!(b, LogBody::TurnStarted { .. })).collect();
    assert!(turns.iter().all(|t| matches!(t, LogBody::TurnStarted { wire_api: WireApi::ClaudeCode, provider, .. } if provider == "anthropic")));

    // Every line validates against the generated schema.
    let raw = std::fs::read_to_string(log::sessions_dir(&home.env()).unwrap().join(&sid).join(log::EVENTS_FILE)).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/log-event.schema.json")).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for l in raw.lines() {
        assert!(validator.is_valid(&serde_json::from_str(l).unwrap()), "{l}");
    }
    // The context record: Claude Code's prompt is its own; the tools it
    // announced include krowk's, with their schema.
    let ctx: Vec<ContextRecord> = std::fs::read_to_string(log::sessions_dir(&home.env()).unwrap().join(&sid).join(log::CONTEXT_FILE)).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(ctx[0].toolset, "claude-code");
    let info = ctx[0].tools.iter().find(|t| t.name == "mcp__krowk__session_info").unwrap();
    assert_eq!(info.input_schema["type"], "object");
}

#[test]
fn a_gh_post_with_a_bare_card_link_is_denied_by_krowks_hook_in_every_mode() {
    for mode in [PermissionMode::Default, PermissionMode::BypassPermissions, PermissionMode::Unhinged] {
        let home = Home::new("paste-guard");
        let dir = home.signed_in("cfg");
        let host = home.host(vec![("claude", home.instance(&dir, Some("paste_guard.jsonl")))], trust::allow_all());
        let lines = rt().block_on(async {
            let (lines, r) = run(&host, prompt(None, "post the result", "claude/sonnet", mode)).await;
            assert_eq!(r.unwrap().unwrap().result, "Posted the krowk block.", "{mode:?}");
            host.shutdown().await;
            lines
        });
        let results: Vec<(bool, String)> = completed(&lines).into_iter().filter_map(|i| if let Item::ToolResult { is_error, output, .. } = i { Some((is_error, output)) } else { None }).collect();
        let [(true, refused), (false, _)] = results.as_slice() else { panic!("{mode:?}: the bare link is refused, the block is not: {results:?}") };
        assert!(refused.contains("bare krowk card link") && refused.contains("krowk block"), "{mode:?}: the model reads why: {refused}");
        let fake = home.fake_log();
        let init = fake.lines().find(|l| l.starts_with("in ") && l.contains(r#""subtype":"initialize""#)).unwrap();
        assert!(init.contains(r#""hooks":{"PreToolUse":[{"matcher":"Bash","hookCallbackIds":["krowk_paste_guard"]}]}"#), "registered on every process: {init}");
        // R-SUB-7: an agent's own text reaches the stream, for its child session.
        assert!(init.contains(r#""forwardSubagentText":true"#), "{init}");
    }
}

#[test]
fn r_compat_1_a_slash_skill_claude_code_does_not_have_reaches_it_loaded_ahead_of_the_prompt() {
    let home = Home::new("slash-skill");
    let dir = home.signed_in("cfg");
    // In the shared directory only: Claude Code's own `skills` has no such
    // skill, and a bare `/implement` would be refused as unknown.
    let skill = home.root.join("home/.agents/skills/implement");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: implement\ndescription: Build a feature end to end\n---\nSTEP ONE: branch first.").unwrap();
    let host = home.host(vec![("claude", home.instance(&dir, Some("session_info.jsonl")))], trust::allow_all());
    let lines = rt().block_on(async {
        let (lines, r) = run(&host, prompt(None, "/implement the login page", "claude/sonnet", PermissionMode::Default)).await;
        r.unwrap().unwrap();
        host.shutdown().await;
        lines
    });

    let items = completed(&lines);
    assert!(matches!(&items[0], Item::UserText { text, .. } if text == "/implement the login page"), "the person's words, as they typed them: {items:?}");
    assert!(matches!(&items[1], Item::UserText { text, .. } if text.starts_with(krowk_harness::compat::skills::INVOKED)), "the skill, logged as krowk's: {items:?}");
    let fake = home.fake_log();
    let sent = fake.lines().find(|l| l.starts_with("in ") && l.contains(r#""type":"user""#)).unwrap_or_else(|| panic!("{fake}"));
    let msg: serde_json::Value = serde_json::from_str(&sent[3..]).unwrap();
    let text = msg["message"]["content"].as_str().map(String::from).unwrap_or_else(|| msg["message"]["content"][0]["text"].as_str().unwrap_or_default().to_string());
    assert!(text.starts_with("<skill name=\"implement\">") && text.contains("STEP ONE: branch first."), "the skill comes first, so no leading `/` reaches Claude Code: {text}");
    assert!(text.ends_with("/implement the login page"), "and the person's words follow it: {text}");
}

impl Home {
    fn events_first_session(&self) -> String {
        log::list(&log::sessions_dir(&self.env()).unwrap()).unwrap()[0].0.clone()
    }
}

#[test]
fn r_back_1_an_interrupt_stops_the_turn_and_the_session_continues() {
    let home = Home::new("interrupt");
    let dir = home.signed_in("cfg");
    let host = home.host(vec![("claude", home.instance(&dir, Some("interrupt.jsonl")))], trust::allow_all());
    rt().block_on(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let exec = host.execute(prompt(None, "count to a thousand", "claude/haiku", PermissionMode::Default), tx);
        tokio::pin!(exec);
        let mut session: Option<String> = None;
        let mut interrupted = false;
        let result = loop {
            tokio::select! {
                biased;
                Some(line) = rx.recv() => {
                    if let StreamLine::Log(ev) = &line {
                        session.get_or_insert(ev.session_id.clone());
                    }
                    if matches!(line, StreamLine::Live(LiveEvent::ItemDelta { .. })) && !interrupted {
                        interrupted = true;
                        let (itx, _irx) = mpsc::channel(1);
                        host.execute(Command::Interrupt { session_id: session.clone().unwrap() }, itx).await.unwrap();
                    }
                }
                r = &mut exec => break r.unwrap().unwrap(),
            }
        };
        assert_eq!(result.status, TurnStatus::Interrupted);
        assert_eq!(result.result, "1\n2\n3\n", "what streamed before the interrupt is kept");
        // The session continues, on the same process.
        let (_, r) = run(&host, prompt(Some(&result.session_id), "go on", "claude/haiku", PermissionMode::Default)).await;
        let next = r.unwrap().unwrap();
        assert_eq!((next.status, next.result.as_str()), (TurnStatus::Completed, "Carrying on from 3."));
        host.shutdown().await;
    });
    let fake = home.fake_log();
    assert!(fake.contains("\ninterrupt\n"), "the control protocol's interrupt: {fake}");
    assert_eq!(processes(&fake), 1, "the process lived through it");
}

#[test]
fn r_back_5_a_new_host_resumes_the_claude_session_the_log_holds() {
    let home = Home::new("resume");
    let dir = home.signed_in("cfg");
    let inst = || vec![("claude:work", home.instance(&dir, None))];
    let sid = rt().block_on(async {
        let host = home.host(inst(), trust::allow_all());
        let (_, r) = run(&host, prompt(None, "hello", "claude:work/sonnet", PermissionMode::Default)).await;
        host.shutdown().await;
        r.unwrap().unwrap().session_id
    });
    // A second krowk: a new host, a new process, on --resume.
    rt().block_on(async {
        let host = home.host(inst(), trust::allow_all());
        let (_, r) = run(&host, prompt(Some(&sid), "again", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        host.shutdown().await;
    });
    let fake = home.fake_log();
    assert_eq!(processes(&fake), 2);
    assert!(fake.contains(&format!("resume {VENDOR_SESSION}")), "{fake}");
    let backend = home.events(&sid).iter().filter(|e| matches!(e.body, LogBody::BackendSession { .. })).count();
    assert_eq!(backend, 1, "the same Claude session is not logged twice");
}

#[test]
fn r_inst_1_two_claude_instances_run_with_their_own_config_directories() {
    let home = Home::new("instances");
    let (work, personal) = (home.signed_in("cfg-work"), home.signed_in("cfg-personal"));
    let host = home.host(vec![("claude:work", home.instance(&work, None)), ("claude:personal", home.instance(&personal, None))], trust::allow_all());
    rt().block_on(async {
        for model in ["claude:work/sonnet", "claude:personal/sonnet"] {
            let (_, r) = run(&host, prompt(None, "hello", model, PermissionMode::Default)).await;
            let r = r.unwrap().unwrap();
            assert_eq!((r.status, r.model.instance.as_str()), (TurnStatus::Completed, model.split_once('/').unwrap().0));
        }
        host.shutdown().await;
    });
    let fake = home.fake_log();
    assert!(fake.contains(&format!("config {}", work.display())) && fake.contains(&format!("config {}", personal.display())), "{fake}");
    assert_eq!(processes(&fake), 2, "one process per session");
    assert!(work.join("projects").is_dir() && personal.join("projects").is_dir(), "each account keeps its own transcripts");
}

#[test]
fn r_back_6_an_untrusted_repository_is_refused_before_claude_is_spawned() {
    let home = Home::new("untrusted");
    let dir = home.signed_in("cfg");
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = asked.clone();
    let gate: trust::Gate = Arc::new(move |root: &Path| {
        seen.lock().unwrap().push(root.to_path_buf());
        Err(trust::untrusted(root, "Pass --trust."))
    });
    let host = home.host(vec![("claude", home.instance(&dir, None))], gate);
    let err = rt().block_on(async { run(&host, prompt(None, "hello", "claude/sonnet", PermissionMode::Default)).await.1 }).unwrap_err();
    assert_eq!(err.code, "untrusted_directory");
    assert_eq!(*asked.lock().unwrap(), [home.root.join("repo")], "asked with the repository's root");
    assert_eq!(processes(&home.fake_log()), 0, "claude was never started");
    assert!(log::list(&log::sessions_dir(&home.env()).unwrap()).unwrap_or_default().is_empty(), "a refusal leaves no session behind");
}

#[test]
fn a_missing_login_or_binary_is_named_with_its_fix() {
    let home = Home::new("refusals");
    let dir = home.root.join("cfg-nobody");
    std::fs::create_dir_all(&dir).unwrap();
    let host = home.host(vec![("claude:personal", home.instance(&dir, None))], trust::allow_all());
    // The readiness check asks `claude auth status` first: refused before
    // a session or a Claude process exists, with the fix.
    let e = rt().block_on(async { run(&host, prompt(None, "hello", "claude:personal/sonnet", PermissionMode::Default)).await.1 }).unwrap_err();
    assert_eq!(e.code, "not_authenticated");
    assert!(e.message.contains("krowk connect anthropic --method subscription --name personal") && e.message.contains("Claude's own login"), "{}", e.message);
    assert_eq!(processes(&home.fake_log()), 0, "no turn process was started");
    assert!(home.fake_log().contains("argv auth status --json"), "{}", home.fake_log());
    assert!(log::list(&log::sessions_dir(&home.env()).unwrap()).unwrap_or_default().is_empty(), "a refusal leaves no session behind");

    let missing = InstanceKind::ClaudeCode { binary: Some(home.root.join("bin/nope").display().to_string()), config_dir: None, env: BTreeMap::new(), args: Vec::new(), api_key_env: None, effort: None };
    let host = home.host(vec![("claude:gone", missing)], trust::allow_all());
    let err = rt().block_on(async { run(&host, prompt(None, "hello", "claude:gone/sonnet", PermissionMode::Default)).await.1 }).unwrap_err();
    assert_eq!(err.code, "backend_not_found");
    assert!(err.message.contains("--binary"), "{}", err.message);
}

#[test]
fn r_inst_3_a_session_is_bound_to_its_instance_and_says_whether_it_bills_a_subscription_or_a_key() {
    let home = Home::new("billing");
    let dir = home.signed_in("cfg");
    let mut keyed = home.instance(&dir, None);
    if let InstanceKind::ClaudeCode { env, .. } = &mut keyed {
        env.insert("ANTHROPIC_API_KEY".into(), "sk-test-not-a-real-key".into());
    }
    let host = home.host(vec![("claude:sub", home.instance(&dir, None)), ("claude:keyed", keyed)], trust::allow_all());
    let billed = |model: &str| {
        let r = rt().block_on(async { run(&host, prompt(None, "hello", model, PermissionMode::Default)).await.1 }).unwrap().unwrap();
        let events = home.events(&r.session_id);
        let instance = events.iter().find_map(|e| if let LogBody::TurnStarted { model, .. } = &e.body { Some(model.instance.clone()) } else { None });
        let billing = events.iter().find_map(|e| if let LogBody::BackendSession { billing, .. } = &e.body { *billing } else { None });
        (instance.unwrap(), billing)
    };
    assert_eq!(billed("claude:sub/sonnet"), ("claude:sub".to_string(), Some(Billing::Subscription)));
    assert_eq!(billed("claude:keyed/sonnet"), ("claude:keyed".to_string(), Some(Billing::ApiKey)));
    rt().block_on(host.shutdown());
}

/// Whether a process is still there. A zombie waiting for its parent to
/// read it has already stopped, so it counts as gone.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    !stat.split(") ").nth(1).is_some_and(|rest| rest.starts_with('Z'))
}

fn gone_soon(pid: i32) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < until {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

fn grandchild(fake_log: &str) -> i32 {
    fake_log.lines().find_map(|l| l.strip_prefix("grandchild ")).expect("the fake started one").parse().unwrap()
}

#[test]
fn r_back_1_claude_runs_in_krowks_mode_and_a_looser_one_is_stopped_before_it_runs_anything() {
    let home = Home::new("mode");
    let dir = home.signed_in("cfg");
    // A settings file made Claude Code come up in acceptEdits.
    let mut loose = home.instance(&dir, Some("session_info.jsonl"));
    if let InstanceKind::ClaudeCode { env, .. } = &mut loose {
        env.insert("FAKE_CLAUDE_PERMISSION_MODE".into(), "acceptEdits".into());
    }
    let host = home.host(vec![("claude", home.instance(&dir, None)), ("claude:loose", loose)], trust::allow_all());
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "hello", "claude/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        let (_, r) = run(&host, prompt(None, "plan it", "claude/sonnet", PermissionMode::Plan)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        let (lines, r) = run(&host, prompt(None, "what session is this?", "claude:loose/sonnet", PermissionMode::Default)).await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Failed);
        let e = r.error.unwrap();
        assert_eq!(e.code, "backend_permission_mode");
        assert!(e.message.contains("`acceptEdits`") && e.message.contains("`default`") && e.message.contains("permissions.defaultMode"), "{}", e.message);
        assert!(completed(&lines).iter().all(|i| matches!(i, Item::UserText { .. })), "nothing ran: {:?}", completed(&lines));
        // krowk in bypassPermissions allows any mode Claude Code reports.
        let (_, r) = run(&host, prompt(None, "hello", "claude:loose/sonnet", PermissionMode::BypassPermissions)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        host.shutdown().await;
    });
    let fake = home.fake_log();
    let modes: Vec<&str> = fake.lines().filter_map(|l| l.strip_prefix("permission-mode ")).collect();
    assert_eq!(modes, ["default", "plan", "default", "default"], "the mode is always named, so settings cannot pick it");
    assert_eq!(fake.matches("\"tool_use_id\":\"toolu_fake_info\",\"type\":\"tool_result\"").count(), 1, "only the bypassPermissions turn got as far as a tool:\n{fake}");
}

#[test]
fn r_back_1_what_claude_started_is_stopped_with_it() {
    let home = Home::new("grandchild");
    let dir = home.signed_in("cfg");
    let host = home.host(vec![("claude", home.instance(&dir, Some("grandchild.jsonl")))], trust::allow_all());
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "start a server", "claude/sonnet", PermissionMode::BypassPermissions)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        let pid = grandchild(&home.fake_log());
        assert!(alive(pid), "the background process runs while the session does");
        host.shutdown().await;
        assert!(gone_soon(pid), "shutdown stopped Claude Code's whole process group");
    });
}

#[test]
fn r_back_5_a_claude_that_crashes_mid_turn_fails_the_turn_and_the_session_resumes() {
    let home = Home::new("crash");
    let dir = home.signed_in("cfg");
    let host = home.host(vec![("claude", home.instance(&dir, Some("crash.jsonl")))], trust::allow_all());
    rt().block_on(async {
        // The crash leaves a background process holding Claude Code's
        // output open: the exit is noticed anyway, and the group stopped.
        let (_, r) = run(&host, prompt(None, "do the thing", "claude/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        assert_eq!(first.status, TurnStatus::Failed);
        let e = first.error.clone().unwrap();
        assert_eq!(e.code, "backend_exited");
        assert!(e.message.contains("--resume"), "{}", e.message);
        assert_eq!(first.result, "Starting on it", "what streamed before the crash is kept");
        assert!(gone_soon(grandchild(&home.fake_log())), "what the crashed process started is stopped");
        // The session is kept, and its next turn resumes the Claude session.
        let (_, r) = run(&host, prompt(Some(&first.session_id), "go on", "claude/sonnet", PermissionMode::Default)).await;
        let next = r.unwrap().unwrap();
        assert_eq!((next.status, next.result.as_str()), (TurnStatus::Completed, "Picked up where it stopped."));
        host.shutdown().await;
        let turns: Vec<TurnStatus> = home.events(&first.session_id).iter().filter_map(|e| if let LogBody::TurnCompleted { status, .. } = e.body { Some(status) } else { None }).collect();
        assert_eq!(turns, [TurnStatus::Failed, TurnStatus::Completed]);
    });
    let fake = home.fake_log();
    assert!(fake.contains("crash") && fake.contains(&format!("resume {VENDOR_SESSION}")), "{fake}");
    assert_eq!(processes(&fake), 2);
}

#[test]
fn a_long_lived_host_lets_go_of_an_idle_sessions_claude() {
    let home = Home::new("idle");
    let dir = home.signed_in("cfg");
    let host = home.host(vec![("claude", home.instance(&dir, None))], trust::allow_all()).with_backend_idle(std::time::Duration::ZERO);
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "first", "claude/sonnet", PermissionMode::Default)).await;
        let a = r.unwrap().unwrap().session_id;
        assert_eq!(home.fake_log().lines().filter(|l| *l == "eof").count(), 0, "kept after its turn");
        // Another session's prompt sweeps the idle one away.
        let (_, r) = run(&host, prompt(None, "second", "claude/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        assert_eq!(home.fake_log().lines().filter(|l| *l == "eof").count(), 1, "the idle process was let go cleanly");
        // And the idle session comes back on --resume.
        let (_, r) = run(&host, prompt(Some(&a), "again", "claude/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        host.shutdown().await;
    });
    let fake = home.fake_log();
    assert_eq!(processes(&fake), 3);
    assert!(fake.contains(&format!("resume {VENDOR_SESSION}")));
}

#[test]
fn r_back_1_a_plan_turn_after_exit_plan_mode_gets_a_process_in_plan_again() {
    let home = Home::new("plan");
    let dir = home.signed_in("cfg");
    let host = home.host(vec![("claude", home.instance(&dir, Some("exit_plan.jsonl")))], trust::allow_all());
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "plan it", "claude/sonnet", PermissionMode::Plan)).await;
        let first = r.unwrap().unwrap();
        assert_eq!((first.status, first.result.as_str()), (TurnStatus::Completed, "Here is the plan."), "{:?}", first.error);
        // Claude Code now reports default: the next plan turn does not
        // inherit it, and does not fail on it either.
        let (_, r) = run(&host, prompt(Some(&first.session_id), "plan the next step", "claude/sonnet", PermissionMode::Plan)).await;
        let next = r.unwrap().unwrap();
        assert_eq!(next.status, TurnStatus::Completed, "{:?}", next.error);
        host.shutdown().await;
    });
    let fake = home.fake_log();
    assert!(fake.contains("mode now default"), "{fake}");
    assert_eq!(processes(&fake), 2, "a new process, in plan, on --resume");
    let second = fake.lines().filter(|l| l.starts_with("argv -p ")).nth(1).unwrap();
    assert!(second.contains(&format!("--resume {VENDOR_SESSION}")) && second.ends_with("--permission-mode plan"), "{second}");
}

#[test]
fn r_inst_1_a_router_instance_gets_its_key_from_the_variable_it_names() {
    let home = Home::new("router");
    let dir = home.root.join("cfg-router");
    std::fs::create_dir_all(&dir).unwrap();
    let mut router = home.instance(&dir, None);
    if let InstanceKind::ClaudeCode { env, api_key_env, .. } = &mut router {
        env.insert("ANTHROPIC_BASE_URL".into(), "https://router.example/api".into());
        *api_key_env = Some("ROUTER_KEY".into());
    }
    let mut unset = router.clone();
    if let InstanceKind::ClaudeCode { api_key_env, .. } = &mut unset {
        *api_key_env = Some("NOT_EXPORTED".into());
    }
    let host = home.host(vec![("claude:router", router), ("claude:unset", unset)], trust::allow_all());
    rt().block_on(async {
        // No Claude login in its directory: the router's key is what it runs on.
        let (_, r) = run(&host, prompt(None, "hello", "claude:router/anthropic/claude-sonnet-4.5", PermissionMode::Default)).await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
        let billing = home.events(&r.session_id).iter().find_map(|e| if let LogBody::BackendSession { billing, .. } = &e.body { *billing } else { None });
        assert_eq!(billing, Some(Billing::ApiKey));
        // A key variable that is not set is named before anything starts.
        let err = run(&host, prompt(None, "hello", "claude:unset/sonnet", PermissionMode::Default)).await.1.unwrap_err();
        assert_eq!(err.code, "not_authenticated");
        assert!(err.message.contains("NOT_EXPORTED"), "{}", err.message);
        host.shutdown().await;
    });
    let fake = home.fake_log();
    assert!(fake.contains("env ANTHROPIC_AUTH_TOKEN=sk-or-fake") && fake.contains("ANTHROPIC_BASE_URL=https://router.example/api"), "{fake}");
    assert!(fake.contains("env ANTHROPIC_API_KEY= "), "the token goes where a router reads it, not as an API key: {fake}");
    assert_eq!(processes(&fake), 1);
}

/// R-EVID-1: a backend is offered `publish` through the bridge, and it runs
/// the host's publisher for the krowk session, in its working directory —
/// the run it opens logged once, like a native session's.
#[test]
fn r_evid_1_claude_code_publishes_through_krowks_bridge() {
    let home = Home::new("publish");
    let dir = home.signed_in("cfg-work");
    let asked: Arc<std::sync::Mutex<Vec<krowk_harness::evidence::PublishRequest>>> = Arc::default();
    let seen = asked.clone();
    let publisher: krowk_harness::evidence::Publisher = Arc::new(move |r: &krowk_harness::evidence::PublishRequest| {
        seen.lock().unwrap().push(r.clone());
        Ok(krowk_harness::evidence::Published { text: "Artifact art_1 — shot.png\nhttps://krowk.com/a/art_1".into(), run: Some("run_1".into()), for_person: Vec::new() })
    });
    let host = home.host_publishing(vec![("claude:work", home.instance(&dir, Some("publish.jsonl")))], trust::allow_all(), Some(publisher));
    rt().block_on(async {
        let (lines, r) = run(&host, prompt(None, "publish the screenshot", "claude:work/sonnet", PermissionMode::AcceptEdits)).await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
        let items = completed(&lines);
        let result = items.iter().find_map(|i| match i {
            Item::ToolResult { output, is_error: false, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(result.as_deref(), Some("Artifact art_1 — shot.png\nhttps://krowk.com/a/art_1"), "{items:?}");
        let asked = asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!((asked[0].session_id.as_str(), asked[0].files.clone(), asked[0].root.clone()), (r.session_id.as_str(), vec!["shot.png".to_string()], home.root.join("repo")));
        let runs: Vec<String> = home
            .events(&r.session_id)
            .into_iter()
            .filter_map(|e| match e.body {
                LogBody::RunOpened { run, .. } => Some(run),
                _ => None,
            })
            .collect();
        assert_eq!(runs, ["run_1"]);
        host.shutdown().await;
    });
    assert!(home.fake_log().contains(r#""name":"publish""#), "tools/list offered it");
}

/// R-BUDGET-1 for a backend: its calls are Claude Code's to make, so the
/// host interrupts the turn once a metered call has gone past the limit,
/// and refuses the next turn before it starts.
#[test]
fn r_budget_1_a_backend_turn_over_its_budget_is_interrupted_and_the_next_refused() {
    let home = Home::new("budget");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("session_info.jsonl")))], trust::allow_all());
    let limits = Some(krowk_harness::protocol::BudgetLimits { max_tokens: Some(50), max_usd: None });
    rt().block_on(async {
        // The first call generates 74 tokens, past 50: the turn stops there.
        let (_, r) = run(&host, prompt_within(None, "what session is this?", "claude:work/sonnet", PermissionMode::Default, limits)).await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Failed);
        let e = r.error.unwrap();
        assert_eq!(e.code, "budget_exceeded");
        assert!(e.message.contains("74 tokens generated, over --max-tokens 50") && e.message.contains("interrupted"), "{}", e.message);
        let before = home.fake_log().matches("in {\"type\":\"user\"").count();
        let (_, next) = run(&host, prompt_within(Some(&r.session_id), "and again", "claude:work/sonnet", PermissionMode::Default, limits)).await;
        let next = next.unwrap().unwrap();
        assert_eq!(next.error.map(|e| e.code).as_deref(), Some("budget_exceeded"));
        assert_eq!(home.fake_log().matches("in {\"type\":\"user\"").count(), before, "the refused turn never reached Claude Code");
        host.shutdown().await;
    });
}

/// R-BUDGET-1: what Claude Code's own subagent (`Task`) spends is the
/// session's spend. Its calls are metered and logged apart from the
/// conversation, and Claude Code's reported total counts when krowk could
/// not price the calls itself — here it prices nothing.
#[test]
fn r_budget_1_claude_codes_subagent_calls_count_toward_the_budget() {
    let home = Home::new("subagent");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("subagent.jsonl")))], trust::allow_all());
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt_within(None, "look around", "claude:work/sonnet", PermissionMode::Default, Some(krowk_harness::protocol::BudgetLimits { max_tokens: Some(100), max_usd: None }))).await;
        let r = r.unwrap().unwrap();
        // 9 + 8 of its own, 600 of its subagent's: over 100, so interrupted.
        assert_eq!(r.error.as_ref().map(|e| e.code.as_str()), Some("budget_exceeded"), "{r:?}");
        assert!(r.error.as_ref().unwrap().message.contains("tokens generated, over --max-tokens 100"), "{r:?}");
        let events = home.events(&r.session_id);
        let sub: Vec<i64> = events.iter().filter_map(|e| match &e.body {
            LogBody::SubagentResponse { usage, .. } => Some(usage.output_tokens),
            _ => None,
        }).collect();
        assert_eq!(sub, [600], "logged once, apart from the conversation");
        assert!(!completed(&lines).iter().any(|i| matches!(i, Item::AssistantText { text } if text == "the subagent looked")));
        assert!(r.usage.output_tokens >= 609, "{:?}", r.usage);
        host.shutdown().await;
    }));
    // One Home at a time: the next takes the lock this one holds.
    drop(host);
    drop(home);
    // Without a limit: Claude Code's reported total prices what krowk could not.
    let home = Home::new("subagent-cost");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("subagent.jsonl")))], trust::allow_all());
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "look around", "claude:work/sonnet", PermissionMode::Default)).await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
        assert_eq!(r.cost_usd, Some(0.3));
        let last_cost = lines.iter().rev().find_map(|l| match l {
            StreamLine::Live(LiveEvent::Cost { cost_usd, .. }) => Some(*cost_usd),
            _ => None,
        });
        assert_eq!(last_cost, Some(Some(0.3)));
        let events = home.events(&r.session_id);
        assert!(events.iter().any(|e| matches!(&e.body, LogBody::TurnCompleted { reported_cost_usd: Some(c), .. } if *c == 0.3)));
        host.shutdown().await;
    }));
}

/// Runs `f` on a runtime of its own, and fails rather than hangs when it
/// does not finish in 30 s: a turn that never ends is a test failure.
fn within<F: std::future::Future>(f: F) -> F::Output {
    rt().block_on(async { tokio::time::timeout(std::time::Duration::from_secs(30), f).await.expect("finished within 30 s") })
}

/// Its instance changed while a session's process is up — the TUI's
/// `/disconnect` or `/connect` of it — the process is not taken at its
/// word: the next turn asks the vendor again, and a sign-out since is
/// refused rather than run on the old, still signed-in process. Instances
/// read again with nothing changed (a cancelled `/connect`), or another
/// instance changed, keep it.
#[test]
fn a_session_process_is_not_reused_once_its_instance_is_replaced() {
    let home = Home::new("replaced");
    let dir = home.signed_in("cfg");
    let inst = || vec![("claude:work", home.instance(&dir, None))];
    let host = home.host(inst(), trust::allow_all());
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "hello", "claude:work/sonnet", PermissionMode::Default)).await;
        let sid = r.unwrap().unwrap().session_id;
        let (_, r) = run(&host, prompt(Some(&sid), "again", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        assert_eq!(processes(&home.fake_log()), 1, "one process serves the session");
        let cfg = InstancesConfig { instances: inst().into_iter().map(|(n, k)| (n.to_string(), k)).collect(), ..Default::default() };
        // Nothing changed, then another instance: the process is kept.
        host.set_registry(Registry::resolve(&cfg, &home.env()), None);
        host.set_registry(Registry::resolve(&cfg, &home.env()), Some("openai"));
        let (_, r) = run(&host, prompt(Some(&sid), "still", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        assert_eq!(processes(&home.fake_log()), 1, "the same process, after a change to nothing of its own");
        // Signed out, as `/disconnect claude:work` does, and the instances
        // read again.
        std::fs::remove_file(dir.join("fake-login")).unwrap();
        krowk_harness::readiness::forget("claude:work");
        host.set_registry(Registry::resolve(&cfg, &home.env()), Some("claude:work"));
        let e = run(&host, prompt(Some(&sid), "after", "claude:work/sonnet", PermissionMode::Default)).await.1.unwrap_err();
        assert_eq!(e.code, "not_authenticated", "{}", e.message);
        // Signed in again: a new process, on the vendor's resume.
        home.signed_in("cfg");
        host.set_registry(Registry::resolve(&cfg, &home.env()), Some("claude:work"));
        let (_, r) = run(&host, prompt(Some(&sid), "back", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        host.shutdown().await;
    });
    let fake = home.fake_log();
    assert_eq!(processes(&fake), 2, "{fake}");
    assert!(fake.contains(&format!("resume {VENDOR_SESSION}")), "{fake}");
}

/// Renamed while a session's process is up — `krowk providers rename
/// claude:work claude:personal`, or from `/connect` — it is the same account: the process goes on under
/// the new name, and a turn naming the old one runs there too.
#[test]
fn a_renamed_instance_keeps_its_session_process() {
    let home = Home::new("renamed");
    let dir = home.signed_in("cfg");
    let host = home.host(vec![("claude:work", home.instance(&dir, None))], trust::allow_all());
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "hello", "claude:work/sonnet", PermissionMode::Default)).await;
        let sid = r.unwrap().unwrap().session_id;
        let cfg = InstancesConfig {
            instances: [("claude:personal".to_string(), home.instance(&dir, None))].into(),
            renamed: [("claude:work".to_string(), "claude:personal".to_string())].into(),
            ..Default::default()
        };
        host.set_registry_renamed(Registry::resolve(&cfg, &home.env()), "claude:work", "claude:personal");
        let (lines, r) = run(&host, Command::Prompt { session_id: Some(sid.clone()), text: "again".into(), images: Vec::new(), model: None, permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }).await;
        let r = r.unwrap().unwrap();
        assert_eq!((r.status, r.model.instance.as_str()), (TurnStatus::Completed, "claude:personal"), "{lines:?}");
        let (_, r) = run(&host, prompt(Some(&sid), "old name", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().model.instance, "claude:personal", "an old name from a client is the instance it became");
        host.shutdown().await;
    });
    assert_eq!(processes(&home.fake_log()), 1, "one process, before and after the rename: {}", home.fake_log());
}

/// Every `backend.agents` list a stream carried, as the agents' words.
fn agents_said(lines: &[StreamLine]) -> Vec<Vec<String>> {
    lines
        .iter()
        .filter_map(|l| match l {
            StreamLine::Live(LiveEvent::BackendAgents { agents, .. }) => Some(agents.iter().map(|a| a.description.clone()).collect()),
            _ => None,
        })
        .collect()
}

/// What the host's watchers were told of `session` until Claude Code began
/// a turn by itself: the lists of agents on the way, and why it began.
async fn until_unprompted(watch: &mut broadcast::Receiver<StreamLine>, session: &str) -> (Vec<Vec<String>>, String) {
    let mut said = Vec::new();
    loop {
        match watch.recv().await.expect("the host is watched") {
            StreamLine::Live(LiveEvent::TurnUnprompted { session_id, reason }) if session_id == session => return (said, reason),
            l @ StreamLine::Live(LiveEvent::BackendAgents { .. }) => said.extend(agents_said(&[l])),
            // A child of the session's, ending between turns.
            l if l.session_id() != session => {}
            other => panic!("only between-turn frames are watched: {other:?}"),
        }
    }
}

fn continue_turn(session_id: &str) -> Command {
    Command::Continue { session_id: session_id.into(), budget: None }
}

/// R-SUB-3 for Claude Code's own agents: one it runs in the background is
/// listed from the turn that starts it until it finishes between turns —
/// its subagent's shell never counted — and what it asks meanwhile is
/// answered under the last turn's permissions. The turn Claude Code then
/// begins by itself waits for `continue`, which runs it as a turn of the
/// session with krowk's note as its prompt; the agent's calls, made while
/// no turn ran, are metered once, in it (R-BUDGET-1).
#[test]
fn r_sub_3_a_background_agent_is_listed_while_idle_and_the_turn_claude_code_begins_waits_for_continue() {
    let home = Home::new("bg-agent");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        assert_eq!((first.status, first.result.as_str()), (TurnStatus::Completed, "launched"), "{:?}", first.error);
        assert_eq!(agents_said(&lines), [vec!["survey the repo".to_string()]], "listed from the turn that started it");
        let listed = lines.iter().find_map(|l| match l {
            StreamLine::Live(LiveEvent::BackendAgents { agents, .. }) => agents.first().cloned(),
            _ => None,
        });
        assert_eq!(listed.map(|a| (a.task_id, a.agent)), Some(("a_bg_1".to_string(), Some("general-purpose".to_string()))));

        let (said, reason) = until_unprompted(&mut watch, &first.session_id).await;
        assert_eq!(said, [Vec::<String>::new()], "gone when it finished, and the subagent's shell never counted");
        assert_eq!(reason, "background agent “survey the repo” completed");
        let fake = home.fake_log();
        assert!(fake.lines().any(|l| l.starts_with("answer ") && l.contains(r#""behavior":"allow""#) && l.contains("README.md")), "its Read, asked between turns, was answered: {fake}");
        assert_eq!(fake.matches("in {\"type\":\"user\"").count(), 1, "nobody prompted the turn it began");

        let (lines, r) = run(&host, continue_turn(&first.session_id)).await;
        let second = r.unwrap().unwrap();
        assert_eq!((second.status, second.result.as_str()), (TurnStatus::Completed, "The agent is done: the repo is small."));
        assert_eq!(completed(&lines).first(), Some(&Item::user(krowk_harness::claude::unprompted(&reason))), "its prompt is krowk's note of why");
        assert_eq!(home.fake_log().matches("in {\"type\":\"user\"").count(), 1, "and nothing was sent for it");
        let events = home.events(&first.session_id);
        let metered: Vec<i64> = events.iter().filter_map(|e| match &e.body {
            LogBody::SubagentResponse { usage, .. } => Some(usage.output_tokens),
            _ => None,
        }).collect();
        assert_eq!(metered, [600], "the agent's call, made between turns, counted once");
        assert_eq!(events.iter().filter(|e| matches!(e.body, LogBody::TurnStarted { .. })).count(), 2);

        let (_, again) = run(&host, continue_turn(&first.session_id)).await;
        assert_eq!(again.unwrap_err().code, "nothing_pending");
        host.shutdown().await;
    }));
}

/// R-BACK-5: a prompt that arrives while a turn Claude Code began is
/// waiting runs that turn first, as its own, and ends at its own result —
/// never at the answer to the agent.
#[test]
fn r_back_5_a_prompt_runs_the_waiting_turn_first_and_ends_at_its_own_result() {
    let home = Home::new("bg-prompt");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (_, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        let _ = until_unprompted(&mut watch, &first.session_id).await;
        let (lines, r) = run(&host, prompt(Some(&first.session_id), "anything else?", "claude:work/sonnet", PermissionMode::Default)).await;
        let r = r.unwrap().unwrap();
        assert_eq!((r.status, r.result.as_str()), (TurnStatus::Completed, "ok"), "the prompt's own answer");
        let results = lines.iter().filter(|l| matches!(l, StreamLine::Live(LiveEvent::Result(_)))).count();
        assert_eq!(results, 1, "one result for one prompt");
        let texts: Vec<String> = completed(&lines)
            .into_iter()
            .filter_map(|i| match i {
                Item::UserText { text, .. } => Some(if text.starts_with(krowk_harness::claude::UNPROMPTED) { "<note>".into() } else { text }),
                Item::AssistantText { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["<note>", "The agent is done: the repo is small.", "anything else?", "ok"], "the waiting turn, then the prompt's");
        let turns = home.events(&first.session_id).iter().filter(|e| matches!(e.body, LogBody::TurnCompleted { .. })).count();
        assert_eq!(turns, 3);
        host.shutdown().await;
    }));
}

/// R-BACK-5: a turn Claude Code begins as a prompt is on its way is read
/// inside the prompt's turn — its `result` carries `origin`, so it is not
/// the prompt's end — with krowk's note where it ended, and the prompt's
/// answer after it.
#[test]
fn r_back_5_a_turn_claude_code_begins_as_a_prompt_arrives_is_folded_into_the_prompts() {
    let home = Home::new("bg-fold");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_fold.jsonl")))], trust::allow_all());
    within(Box::pin(async {
        let (_, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        let (lines, r) = run(&host, prompt(Some(&first.session_id), "anything else?", "claude:work/sonnet", PermissionMode::Default)).await;
        let r = r.unwrap().unwrap();
        assert_eq!((r.status, r.result.as_str()), (TurnStatus::Completed, "Nothing else."), "{:?}", r.error);
        let items = completed(&lines);
        let note = items.iter().position(|i| matches!(i, Item::UserText { text, .. } if text.starts_with(krowk_harness::claude::UNPROMPTED)));
        let agent = items.iter().position(|i| matches!(i, Item::AssistantText { text } if text == "The agent is done: the repo is small."));
        let own = items.iter().position(|i| matches!(i, Item::AssistantText { text } if text == "Nothing else."));
        assert!(agent < note && note < own && agent.is_some(), "Claude Code's turn, the note, the prompt's answer: {items:?}");
        assert_eq!(agents_said(&lines), [Vec::<String>::new()], "the agent finished inside the turn");
        let (_, again) = run(&host, continue_turn(&first.session_id)).await;
        assert_eq!(again.unwrap_err().code, "nothing_pending", "nothing is left waiting");
        host.shutdown().await;
    }));
}

/// A process with an agent running, or a turn it began waiting, is not let
/// go as idle however long it has been; once neither is left, it is.
#[test]
fn r_back_1_a_process_with_a_background_agent_is_not_let_go_as_idle() {
    let home = Home::new("bg-evict");
    let dir = home.signed_in("cfg-work");
    let other = home.signed_in("cfg-other");
    // The other instance logs apart, so this log's `eof` is the agent's process.
    let mut quiet = home.instance(&other, None);
    if let InstanceKind::ClaudeCode { env, .. } = &mut quiet {
        env.insert("FAKE_CLAUDE_LOG".into(), home.root.join("other.log").display().to_string());
    }
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl"))), ("claude:other", quiet)], trust::allow_all()).with_backend_idle(std::time::Duration::ZERO);
    let mut watch = host.watch();
    let let_go = |home: &Home| home.fake_log().lines().any(|l| l == "eof");
    within(Box::pin(async {
        let (_, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let a = r.unwrap().unwrap();
        // Another session's prompt sweeps the idle processes.
        let (_, r) = run(&host, prompt(None, "hello", "claude:other/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().result, "ok");
        assert!(!let_go(&home), "kept while its agent runs");
        let _ = until_unprompted(&mut watch, &a.session_id).await;
        let (_, r) = run(&host, prompt(None, "hello", "claude:other/sonnet", PermissionMode::Default)).await;
        r.unwrap().unwrap();
        assert!(!let_go(&home), "kept while the turn it began waits");
        let (_, r) = run(&host, continue_turn(&a.session_id)).await;
        r.unwrap().unwrap();
        let (_, r) = run(&host, prompt(None, "hello", "claude:other/sonnet", PermissionMode::Default)).await;
        r.unwrap().unwrap();
        assert!(let_go(&home), "let go once nothing of its own runs");
        host.shutdown().await;
    }));
}

/// A plan turn whose ExitPlanMode was approved leaves Claude Code in
/// default: the turn it then begins by itself runs in that mode, which
/// krowk accepted, and is not stopped as looser than the plan asked for —
/// nor are the process and its agents killed for it.
#[test]
fn r_back_1_the_turn_claude_code_begins_after_an_approved_plan_runs_in_default() {
    let home = Home::new("bg-plan");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_plan.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (_, r) = run(&host, prompt(None, "plan it, and survey the repo in the background", "claude:work/sonnet", PermissionMode::Plan)).await;
        let first = r.unwrap().unwrap();
        assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.error);
        let _ = until_unprompted(&mut watch, &first.session_id).await;
        let (_, r) = run(&host, continue_turn(&first.session_id)).await;
        let second = r.unwrap().unwrap();
        assert_eq!((second.status, second.result.as_str()), (TurnStatus::Completed, "The agent is done: the repo is small."), "{:?}", second.error);
        let modes: Vec<PermissionMode> = home.events(&first.session_id).iter().filter_map(|e| match &e.body {
            LogBody::TurnStarted { permission_mode, .. } => Some(*permission_mode),
            _ => None,
        }).collect();
        assert_eq!(modes, [PermissionMode::Plan, PermissionMode::Default], "the mode the process was in, which krowk accepted");
        host.shutdown().await;
    }));
    assert_eq!(processes(&home.fake_log()), 1, "never restarted");
}

/// A session moved to another instance lets its old Claude Code go, and
/// the background agents it ran with it: the watchers are told the list is
/// empty, and why, so no client goes on counting them.
#[test]
fn r_sub_3_background_agents_stopped_with_their_process_are_said_to_have_gone() {
    let home = Home::new("bg-switch");
    let dir = home.signed_in("cfg-work");
    let other = home.signed_in("cfg-other");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl"))), ("claude:other", home.instance(&other, None))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (_, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let a = r.unwrap().unwrap();
        // While its agent runs, the session moves to the other account.
        let (_, r) = run(&host, prompt(Some(&a.session_id), "go on over there", "claude:other/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        let mut said = Vec::new();
        let notice = loop {
            match watch.recv().await.unwrap() {
                StreamLine::Live(LiveEvent::Notice { session_id, text, .. }) if session_id == a.session_id => break text,
                l @ StreamLine::Live(LiveEvent::BackendAgents { .. }) => said.extend(agents_said(&[l])),
                _ => {}
            }
        };
        assert_eq!(said, [Vec::<String>::new()], "the list, emptied");
        assert!(notice.contains("claude:work") && notice.contains("background agents it ran stopped"), "{notice}");
        host.shutdown().await;
    }));
}

/// The order seen live: the agent finishes while idle, a prompt is sent
/// and answered, and only then does Claude Code begin its own turn. The
/// prompt ends at its own answer, and the turn that follows still says
/// which agent it answers.
#[test]
fn r_back_5_a_turn_claude_code_begins_after_a_prompt_still_names_the_agent_it_answers() {
    let home = Home::new("bg-after");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_after_prompt.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (_, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        // The agent's end, read between turns.
        loop {
            if let StreamLine::Live(LiveEvent::BackendAgents { agents, .. }) = watch.recv().await.unwrap()
                && agents.is_empty()
            {
                break;
            }
        }
        let (_, r) = run(&host, prompt(Some(&first.session_id), "say second", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().result, "second", "the prompt's own answer");
        let (_, reason) = until_unprompted(&mut watch, &first.session_id).await;
        assert_eq!(reason, "background agent “survey the repo” completed");
        let (_, r) = run(&host, continue_turn(&first.session_id)).await;
        assert_eq!(r.unwrap().unwrap().result, "The agent is done: the repo is small.");
        host.shutdown().await;
    }));
}

/// The steer ST0's recordings wrote.
const STEER: &str = "Also: end your answer with the word PINEAPPLE.";

impl Home {
    /// One of ST0's live recordings (`fixtures/claude/recorded`) as a
    /// scenario the fake plays. Its `init` and the prompt's replay are the
    /// fake's own. Where the steer was written, the fake waits for it
    /// (`@await-steer`) when Claude Code read it in the same turn, and its
    /// replay is `@replay`; when Claude Code read it only in a turn of its
    /// own after the `result`, the turn pauses there (`@sleep 1`) so krowk
    /// writes it in time, and that turn is the scenario's next, begun by
    /// the fake for the line it reads next, as Claude Code does.
    fn scenario_from(&self, recording: &str) -> String {
        let raw = std::fs::read_to_string(fixture("recorded").join(recording)).unwrap();
        let lines: Vec<(&str, serde_json::Value)> = raw.lines().map(|l| (&l[..3], serde_json::from_str(&l[3..]).unwrap())).collect();
        let is_steer = |v: &serde_json::Value| v["type"] == "user" && v.pointer("/message/content/0/text").or(v.pointer("/message/content")).and_then(serde_json::Value::as_str) == Some(STEER);
        let replayed = lines.iter().position(|(d, v)| *d == "<< " && v["isReplay"] == true && is_steer(v)).unwrap();
        let first_result = lines.iter().position(|(d, v)| *d == "<< " && v["type"] == "result").unwrap();
        let same_turn = replayed < first_result;
        let mut out = Vec::new();
        let mut results = 0;
        for (d, v) in &lines {
            match *d {
                ">> " if is_steer(v) => out.push(if same_turn { "@await-steer" } else { "@sleep 1" }.to_string()),
                ">> " => {}
                _ if v["type"] == "system" && v["subtype"] == "init" => {
                    if results > 0 {
                        out.push("---".into());
                    }
                }
                _ if v["isReplay"] == true => {
                    if is_steer(v) && same_turn {
                        out.push("@replay".into());
                    }
                }
                _ => {
                    results += usize::from(v["type"] == "result");
                    out.push(v.to_string());
                }
            }
        }
        let path = self.root.join(recording.replace(".txt", ".jsonl"));
        std::fs::write(&path, out.join("\n") + "\n").unwrap();
        path.display().to_string()
    }
}

/// Runs a prompt and steers it once with `text`, at the first line `when`
/// picks out of those seen so far.
async fn run_steered(host: &Host, cmd: Command, text: &str, when: impl Fn(&[StreamLine]) -> bool) -> (Vec<StreamLine>, Result<Option<RunResult>, krowk_harness::engine::EngineError>) {
    let (tx, mut rx) = mpsc::channel(4096);
    let exec = host.execute(cmd, tx);
    tokio::pin!(exec);
    let mut lines = Vec::new();
    let mut session = String::new();
    let mut steered = false;
    let mut done = None;
    loop {
        tokio::select! {
            l = rx.recv() => match l {
                Some(l) => {
                    if let StreamLine::Log(e) = &l {
                        session.clone_from(&e.session_id);
                    }
                    lines.push(l);
                    if !steered && when(&lines) {
                        steered = true;
                        let (stx, _srx) = mpsc::channel(8);
                        host.execute(Command::Steer { session_id: session.clone(), text: text.into(), images: Vec::new() }, stx).await.unwrap();
                    }
                }
                None => break,
            },
            r = &mut exec, if done.is_none() => done = Some(r),
        }
    }
    (lines, done.expect("the command ran"))
}

fn delta(lines: &[StreamLine]) -> bool {
    matches!(lines.last(), Some(StreamLine::Live(LiveEvent::ItemDelta { .. })))
}

fn user_texts(items: &[Item]) -> Vec<String> {
    items.iter().filter_map(|i| if let Item::UserText { text, .. } = i { Some(text.clone()) } else { None }).collect()
}

/// R-STEER-1, against ST0's mid-turn recording: a steer queued while a
/// Bash call runs is written to Claude Code at once, and logged where
/// Claude Code replayed it — after the call's result, before the answer
/// that read it — once; the prompt's own replay is not logged again. The
/// turn completes with nothing unread, so nothing goes back to the prompt.
#[test]
fn r_steer_1_a_steer_during_a_tool_call_is_logged_where_claude_code_replayed_it_and_none_is_left_unread() {
    let home = Home::new("steer-mid");
    let dir = home.signed_in("cfg");
    let scenario = home.scenario_from("steer_mid_turn.txt");
    let host = home.host(vec![("claude", home.instance(&dir, Some(&scenario)))], trust::allow_all());
    within(Box::pin(async {
        let (_, r) = run_steered(&host, prompt(None, "run sleep 20 && echo done", "claude/haiku", PermissionMode::Default), STEER, delta).await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Completed, "{r:?}");
        assert!(r.unread_steers.is_empty(), "{:?}", r.unread_steers);
        assert!(r.result.contains("PINEAPPLE"), "{}", r.result);
        let items: Vec<Item> = home.events(&r.session_id).iter().filter_map(|e| if let LogBody::ItemCompleted { item, .. } = &e.body { Some(item.clone()) } else { None }).collect();
        assert_eq!(user_texts(&items), ["run sleep 20 && echo done", STEER], "each once");
        let at = |f: &dyn Fn(&Item) -> bool| items.iter().position(f).unwrap();
        let result = at(&|i| matches!(i, Item::ToolResult { output, .. } if output == "done"));
        let steer = at(&|i| matches!(i, Item::UserText { text, .. } if text == STEER));
        let answer = at(&|i| matches!(i, Item::AssistantText { text } if text.contains("PINEAPPLE")));
        assert!(result < steer && steer < answer, "result {result}, steer {steer}, answer {answer}: {items:?}");
        host.shutdown().await;
    }));
    let fake = home.fake_log();
    let written = lines_of(&fake, "steer ");
    assert!(written.len() == 1 && written[0].contains(STEER) && written[0].contains(r#""uuid":""#), "written mid-call, under a uuid: {fake}");
    assert!(fake.contains("--replay-user-messages"));
}

/// R-STEER-1, against ST0's late recording: a steer written as the model
/// writes its last answer is read in a turn Claude Code begins after the
/// `result`; the krowk turn reads on into it and completes with the steer
/// read, logged where it was replayed.
#[test]
fn r_steer_1_a_steer_during_the_last_answer_is_read_in_a_second_claude_code_turn_inside_the_krowk_turn() {
    let home = Home::new("steer-late");
    let dir = home.signed_in("cfg");
    let scenario = home.scenario_from("steer_late.txt");
    let host = home.host(vec![("claude", home.instance(&dir, Some(&scenario)))], trust::allow_all());
    within(Box::pin(async {
        // The first delta of the answer after the tool's result.
        let after_result = |lines: &[StreamLine]| delta(lines) && completed(lines).iter().any(|i| matches!(i, Item::ToolResult { .. }));
        let (_, r) = run_steered(&host, prompt(None, "run it, then write a story", "claude/haiku", PermissionMode::Default), STEER, after_result).await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Completed, "{r:?}");
        assert!(r.unread_steers.is_empty(), "{:?}", r.unread_steers);
        assert_eq!(r.result, "PINEAPPLE", "the turn ends at the answer that read it");
        let items: Vec<Item> = home.events(&r.session_id).iter().filter_map(|e| if let LogBody::ItemCompleted { item, .. } = &e.body { Some(item.clone()) } else { None }).collect();
        assert_eq!(user_texts(&items), ["run it, then write a story", STEER]);
        let story = items.iter().position(|i| matches!(i, Item::AssistantText { text } if text.contains("Lighthouse"))).unwrap();
        let steer = items.iter().position(|i| matches!(i, Item::UserText { text, .. } if text == STEER)).unwrap();
        assert!(story < steer, "logged after the story it arrived during: {items:?}");
        host.shutdown().await;
    }));
    let fake = home.fake_log();
    assert_eq!(lines_of(&fake, "in ").iter().filter(|l| l.contains(STEER)).count(), 1, "written once: {fake}");
    assert_eq!(processes(&fake), 1);
}

/// An interrupted Claude Code turn hands back the steering it never read:
/// one written and then cancelled by the interrupt (`cancel_queued`), and
/// one that arrived after the interrupt, never written at all.
#[test]
fn r_steer_1_an_interrupted_turn_hands_back_steering_claude_code_never_read() {
    let home = Home::new("steer-interrupt");
    let dir = home.signed_in("cfg");
    let host = home.host(vec![("claude", home.instance(&dir, Some("interrupt.jsonl")))], trust::allow_all());
    let log = home.log_file();
    within(Box::pin(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let exec = host.execute(prompt(None, "count to a thousand", "claude/haiku", PermissionMode::Default), tx);
        tokio::pin!(exec);
        let mut session = String::new();
        let mut steered = false;
        let mut interrupted = false;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
        let steer = |session: &str, text: &str| Command::Steer { session_id: session.into(), text: text.into(), images: Vec::new() };
        let result = loop {
            tokio::select! {
                biased;
                Some(line) = rx.recv() => {
                    if let StreamLine::Log(ev) = &line {
                        session.clone_from(&ev.session_id);
                    }
                    if matches!(line, StreamLine::Live(LiveEvent::ItemDelta { .. })) && !steered {
                        steered = true;
                        let (stx, _srx) = mpsc::channel(8);
                        host.execute(steer(&session, "first"), stx).await.unwrap();
                    }
                }
                r = &mut exec => break r.unwrap().unwrap(),
                // Once the fake has read the first, written: the interrupt,
                // and a steer after it.
                _ = tick.tick(), if steered && !interrupted => {
                    if std::fs::read_to_string(&log).unwrap_or_default().lines().any(|l| l.starts_with("in ") && l.contains(r#""content":"first""#)) {
                        interrupted = true;
                        let (stx, _srx) = mpsc::channel(8);
                        host.execute(Command::Interrupt { session_id: session.clone() }, stx.clone()).await.unwrap();
                        host.execute(steer(&session, "second"), stx).await.unwrap();
                    }
                }
            }
        };
        assert_eq!(result.status, TurnStatus::Interrupted);
        assert_eq!(result.unread_steers, ["first", "second"], "oldest first");
        host.shutdown().await;
    }));
    let fake = home.fake_log();
    assert!(fake.lines().any(|l| l.starts_with("cancelled \"")), "the interrupt cancelled what was queued: {fake}");
    assert!(!lines_of(&fake, "in ").iter().any(|l| l.contains(r#""content":"second""#)), "never written: {fake}");
}

/// The lines of the fake's log with a prefix, e.g. `in ` or `steer `.
fn lines_of(log: &str, prefix: &str) -> Vec<String> {
    log.lines().filter_map(|l| l.strip_prefix(prefix)).map(String::from).collect()
}

/// A Claude Code too old to cancel what it has not read on an interrupt
/// (no `interrupt_cancel_queued_v1`) is written no steering: it comes back
/// on the turn's result, as before R-STEER-1.
#[test]
fn r_steer_1_a_claude_that_cannot_cancel_steering_is_written_none_and_hands_it_back() {
    let home = Home::new("steer-old");
    let dir = home.signed_in("cfg");
    let mut instance = home.instance(&dir, Some("slow.jsonl"));
    if let InstanceKind::ClaudeCode { env, .. } = &mut instance {
        env.insert("FAKE_CLAUDE_CAPABILITIES".into(), r#""interrupt_receipt_v1""#.into());
    }
    let host = home.host(vec![("claude", instance)], trust::allow_all());
    within(Box::pin(async {
        let started = |lines: &[StreamLine]| lines.iter().any(|l| matches!(l, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { .. }, .. })));
        let (_, r) = run_steered(&host, prompt(None, "take your time", "claude/haiku", PermissionMode::Default), STEER, started).await;
        let r = r.unwrap().unwrap();
        assert_eq!((r.status, r.unread_steers.as_slice()), (TurnStatus::Completed, [STEER.to_string()].as_slice()));
        host.shutdown().await;
    }));
    assert!(!home.fake_log().contains(STEER), "never written");
}

/// R-SUB-7: a Claude Code agent's child session is held as a session
/// running here while the agent runs, as a native child's turn is: a
/// prompt to it is refused as busy, and a steer of it is answered for what
/// it is — Claude Code's to run — rather than taken and dropped (its
/// interrupt is a `stop_task`, R-SUB-10). Once it has ended, nothing holds
/// it.
#[test]
fn r_sub_7_a_claude_agents_child_session_is_held_as_running_while_the_agent_runs() {
    let home = Home::new("kids-held");
    let dir = home.signed_in("cfg-work");
    // The recorded parallel turn after `init`, a pause once both agents
    // have started.
    let raw = std::fs::read_to_string(fixture("recorded/agents_parallel.txt")).unwrap();
    let printed: Vec<serde_json::Value> = raw.lines().filter_map(|l| l.strip_prefix("<< ")).map(|j| serde_json::from_str(j).unwrap()).collect();
    let init = printed.iter().position(|m| m["subtype"] == "init").unwrap();
    let lines: Vec<&serde_json::Value> = printed[init + 1..].iter().filter(|m| !matches!(m["type"].as_str(), Some("control_request" | "control_response")) && m["isReplay"] != true).collect();
    let last_start = lines.iter().rposition(|m| m["subtype"] == "task_started" && m["task_type"] == "local_agent").unwrap();
    let mut scenario = String::new();
    for (i, m) in lines.iter().enumerate() {
        scenario.push_str(&format!("{m}\n"));
        if i == last_start {
            scenario.push_str("@sleep 2\n");
        }
    }
    let path = home.root.join("agents.jsonl");
    std::fs::write(&path, scenario).unwrap();
    let host = Arc::new(home.host(vec![("claude:work", home.instance(&dir, Some(path.to_str().unwrap())))], trust::allow_all()));
    rt().block_on(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let h = host.clone();
        let turn = tokio::spawn(async move { h.execute(prompt(None, "run two agents", "claude:work/sonnet", PermissionMode::Default), tx).await });
        let mut kids = Vec::new();
        while kids.len() < 2 {
            if let StreamLine::Log(LogEvent { body: LogBody::SubagentStarted { subagent_session_id, .. }, .. }) = rx.recv().await.expect("the turn goes on") {
                kids.push(subagent_session_id);
            }
        }
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let code = |r: Result<Option<RunResult>, krowk_harness::engine::EngineError>| r.err().map(|e| e.code);
        let out = || mpsc::channel(64).0;
        for kid in &kids {
            assert_eq!(code(host.execute(prompt(Some(kid), "hello", "claude:work/sonnet", PermissionMode::Default), out()).await).as_deref(), Some("session_busy"), "{kid}");
            assert_eq!(code(host.execute(Command::Steer { session_id: kid.clone(), text: "not you".into(), images: Vec::new() }, out()).await).as_deref(), Some("not_steerable"), "{kid}");
        }
        let r = turn.await.unwrap().unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Completed, "the parent's turn is untouched: {:?}", r.error);
        drain.await.unwrap();
        for kid in &kids {
            assert_eq!(code(host.execute(Command::Interrupt { session_id: kid.clone() }, out()).await).as_deref(), Some("no_running_turn"), "{kid} let go once it ended");
        }
        host.shutdown().await;
    });
}

/// The recorded parallel turn after `init` as a scenario, its asks played
/// as asks and the rest of the control lines — krowk's to answer, or its
/// answers — left out; and the recording's printed lines.
fn asking(home: &Home) -> (PathBuf, Vec<serde_json::Value>) {
    let raw = std::fs::read_to_string(fixture("recorded/agents_parallel.txt")).unwrap();
    let printed: Vec<serde_json::Value> = raw.lines().filter_map(|l| l.strip_prefix("<< ")).map(|j| serde_json::from_str(j).unwrap()).collect();
    let init = printed.iter().position(|m| m["subtype"] == "init").unwrap();
    let mut scenario = String::new();
    for m in &printed[init + 1..] {
        match m["type"].as_str() {
            Some("control_request") if m["request"]["subtype"] == "can_use_tool" => scenario.push_str(&format!("@ask {}\n", m["request"])),
            Some("control_request" | "control_response") => {}
            _ if m["isReplay"] == true => {}
            _ => scenario.push_str(&format!("{m}\n")),
        }
    }
    let path = home.root.join("agents.jsonl");
    std::fs::write(&path, scenario).unwrap();
    (path, printed)
}

/// R-SUB-9: a request handed to a Claude Code agent's child session goes
/// with the child however that ends — here its turn dropped while it
/// waited, and its process let go — and is not left waiting for ever.
#[test]
fn r_sub_9_a_claude_agents_request_is_forgotten_with_the_agent_when_its_turn_is_dropped() {
    use krowk_harness::protocol::ApprovalDecision;
    let home = Home::new("kids-forget");
    let dir = home.signed_in("cfg-work");
    let (path, _) = asking(&home);
    let host = home.host_answering(vec![("claude:work", home.instance(&dir, Some(path.to_str().unwrap())))]);
    within(Box::pin(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let req = {
            let turn = host.execute(prompt(None, "run two agents", "claude:work/sonnet", PermissionMode::Default), tx);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    _ = &mut turn => panic!("the turn waits on the person"),
                    Some(l) = rx.recv() => if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = l { break r },
                }
            }
            // The turn is dropped here, its request never answered.
        };
        host.shutdown().await;
        let answer = Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() };
        let e = host.execute(answer, mpsc::channel(1).0).await.unwrap_err();
        assert!(e.message.contains("has no approval request"), "forgotten, not left waiting: {}", e.message);
    }));
}

/// R-SUB-9: each Claude Code agent's `subagent.status`, from the recorded
/// parallel turn: the tool it is in from its call, its tokens rising from
/// Claude Code's count, and how it ended. The sleeper's `can_use_tool` is
/// its own: an approval under its child session, answered as that session
/// — not the parent's — which answers Claude Code, and its status waits on
/// the person until then, its tool's clock starting again at the answer.
#[test]
fn r_sub_9_a_claude_agents_status_follows_its_tools_tokens_and_end_and_its_approval_is_its_own() {
    use krowk_harness::protocol::{ApprovalDecision, ChildState, Waiting};
    let home = Home::new("kids-status");
    let dir = home.signed_in("cfg-work");
    let (path, printed) = asking(&home);
    let host = home.host_answering(vec![("claude:work", home.instance(&dir, Some(path.to_str().unwrap())))]);
    let task = |d: &str| printed.iter().find(|m| m["subtype"] == "task_started" && m["description"] == d).unwrap()["task_id"].as_str().unwrap().to_string();
    let (reader, sleeper) = (task("read notes"), task("sleeper"));
    let lines = within(Box::pin(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let turn = host.execute(prompt(None, "run two agents", "claude:work/sonnet", PermissionMode::Default), tx);
        let client = async {
            let mut lines: Vec<StreamLine> = Vec::new();
            while let Some(l) = rx.recv().await {
                if let StreamLine::Live(LiveEvent::ApprovalRequested(req)) = &l {
                    let answer = |session_id: &str| Command::Approve { session_id: session_id.into(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() };
                    let parent = lines.iter().find_map(|l| if let StreamLine::Log(LogEvent { session_id, body: LogBody::SubagentStarted { .. }, .. }) = l { Some(session_id.clone()) } else { None }).unwrap();
                    let refused = host.execute(answer(&parent), mpsc::channel(1).0).await.unwrap_err();
                    assert_eq!(refused.code, "no_approval_request", "not the parent's to answer");
                    host.execute(answer(&req.session_id), mpsc::channel(1).0).await.unwrap();
                }
                lines.push(l);
            }
            lines
        };
        let (r, lines) = tokio::join!(turn, client);
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        host.shutdown().await;
        lines
    }));
    let kid = |t: &str| kid_of(&lines, t);
    let (reader_kid, sleeper_kid) = (kid(&reader), kid(&sleeper));
    // Its frames, in order: (tool, waiting, tokens, status).
    let frames = |k: &str| -> Vec<(Option<String>, Option<Waiting>, i64, ChildState)> {
        lines.iter()
            .filter_map(|l| match l {
                StreamLine::Live(LiveEvent::SubagentStatus { session_id, status, tool, waiting, tokens, .. }) if session_id == k => Some((tool.as_ref().map(|t| t.name.clone()), *waiting, *tokens, *status)),
                _ => None,
            })
            .collect()
    };
    let r = frames(&reader_kid);
    assert!(r.contains(&(Some("Read".into()), None, 12422, ChildState::Running)), "{r:?}");
    assert_eq!(r.last(), Some(&(None, None, 13052, ChildState::Done)), "{r:?}");
    let tokens: Vec<i64> = r.iter().map(|f| f.2).collect();
    assert!(tokens.windows(2).all(|w| w[0] <= w[1]) && tokens[0] == 0, "rising: {tokens:?}");
    let s = frames(&sleeper_kid);
    let bash = (Some("Bash".to_string()), None, 12613, ChildState::Running);
    let waits = (None, Some(Waiting::Approval), 12613, ChildState::Running);
    let at = |f: &(Option<String>, Option<Waiting>, i64, ChildState)| s.iter().position(|x| x == f).unwrap_or_else(|| panic!("{f:?} in {s:?}"));
    let (ran, waited) = (at(&bash), at(&waits));
    assert!(ran < waited && s[waited + 1] == bash, "in Bash, waiting on the person, in Bash again once answered: {s:?}");
    assert_eq!(s.last().map(|f| f.3), Some(ChildState::Interrupted), "stopped: {s:?}");
    // The approval is the sleeper's, in its turn, and so is its answer.
    let req = lines.iter().find_map(|l| if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = l { Some(r.clone()) } else { None }).expect("asked");
    let turn_of = |k: &str| home.events(k).iter().find_map(|e| if let LogBody::TurnStarted { turn_id, .. } = &e.body { Some(turn_id.clone()) } else { None }).unwrap();
    assert_eq!((req.session_id.as_str(), req.turn_id.clone(), req.tool.as_str()), (sleeper_kid.as_str(), turn_of(&sleeper_kid), "Bash"));
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ApprovalResolved { session_id, request_id, decision: ApprovalDecision::Allow, .. }) if *session_id == sleeper_kid && *request_id == req.request_id)));
    let fake = home.fake_log();
    assert!(lines_of(&fake, "answer ").iter().any(|a| a.contains(r#""behavior":"allow","updatedInput":{"command":"sleep 30"#)), "Claude Code was answered: {fake}");
}

/// R-SUB-7: a background agent that ends between turns ends its child
/// there and then — its turn completed in its log, its last frame to the
/// host's watchers — and lets go of the child's hold: it is no longer a
/// session running here.
#[test]
fn r_sub_7_a_background_agent_ending_between_turns_ends_its_child_and_lets_it_go() {
    let home = Home::new("bg-kid");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        let kid = lines.iter().find_map(|l| match l {
            StreamLine::Log(LogEvent { body: LogBody::SubagentStarted { subagent_session_id, backend_id: Some(b), .. }, .. }) if b == "a_bg_1" => Some(subagent_session_id.clone()),
            _ => None,
        });
        let kid = kid.expect("the background agent is a child");
        let out = || mpsc::channel(64).0;
        let code = |r: Result<Option<RunResult>, krowk_harness::engine::EngineError>| r.err().map(|e| e.code);
        assert_eq!(code(host.execute(Command::Steer { session_id: kid.clone(), text: "not you".into(), images: Vec::new() }, out()).await).as_deref(), Some("not_steerable"), "running on past its turn");
        // Between turns: its end, to the watchers, as the child's.
        let mut ended = (false, false);
        while ended != (true, true) {
            match watch.recv().await.expect("the host is watched") {
                StreamLine::Log(LogEvent { session_id, body: LogBody::TurnCompleted { status: TurnStatus::Completed, .. }, .. }) if session_id == kid => ended.0 = true,
                StreamLine::Live(LiveEvent::SubagentStatus { session_id, status: krowk_harness::protocol::ChildState::Done, .. }) if session_id == kid => ended.1 = true,
                _ => {}
            }
        }
        assert_eq!(code(host.execute(Command::Interrupt { session_id: kid.clone() }, out()).await).as_deref(), Some("no_running_turn"), "let go once it ended");
        let log = home.events(&kid);
        assert!(matches!(log.last().map(|e| &e.body), Some(LogBody::TurnCompleted { status: TurnStatus::Completed, .. })), "{log:?}");
        host.shutdown().await;
    }));
}

/// The child session of the agent Claude Code knows as `task`, from the
/// turn that started it.
fn kid_of(lines: &[StreamLine], task: &str) -> String {
    let kid = lines.iter().find_map(|l| match l {
        StreamLine::Log(LogEvent { body: LogBody::SubagentStarted { subagent_session_id, backend_id: Some(b), .. }, .. }) if b == task => Some(subagent_session_id.clone()),
        _ => None,
    });
    kid.expect("the agent is a child")
}

/// The recorded parallel turn after `init` as a scenario that waits for
/// krowk's `stop_task` where the recording sent one (`@await-stop`), and
/// the task ids of its two agents, the reader's and the sleeper's. The
/// fake says the sleeper was stopped as Claude Code did; refusing, it says
/// nothing, and the recording's own words for it follow.
fn stopping(home: &Home, refused: bool) -> (PathBuf, String, String) {
    let raw = std::fs::read_to_string(fixture("recorded/agents_parallel.txt")).unwrap();
    let printed: Vec<serde_json::Value> = raw.lines().filter_map(|l| l.strip_prefix("<< ")).map(|j| serde_json::from_str(j).unwrap()).collect();
    let task = |d: &str| printed.iter().find(|m| m["subtype"] == "task_started" && m["description"] == d).unwrap()["task_id"].as_str().unwrap().to_string();
    let (reader, sleeper) = (task("read notes"), task("sleeper"));
    let init = printed.iter().position(|m| m["subtype"] == "init").unwrap();
    let mut scenario = String::new();
    for m in &printed[init + 1..] {
        let said_stopped = m["task_id"] == sleeper.as_str() && (m["patch"]["status"] == "killed" || m["status"] == "stopped");
        if m["subtype"] == "task_updated" && said_stopped {
            scenario.push_str("@await-stop\n");
        }
        match m["type"].as_str() {
            Some("control_request" | "control_response") => {}
            _ if m["isReplay"] == true => {}
            _ if said_stopped && !refused => {}
            _ => scenario.push_str(&format!("{m}\n")),
        }
    }
    let path = home.root.join("stopping.jsonl");
    std::fs::write(&path, scenario).unwrap();
    (path, reader, sleeper)
}

/// Runs the parallel turn, interrupting the sleeper's child session once
/// it has started: the turn's lines, its result, and the interrupt's
/// answer.
async fn stop_the_sleeper(host: &Host, sleeper: &str) -> (Vec<StreamLine>, RunResult, Result<Option<RunResult>, krowk_harness::engine::EngineError>) {
    let (tx, mut rx) = mpsc::channel(4096);
    let turn = host.execute(prompt(None, "run two agents", "claude:work/sonnet", PermissionMode::Default), tx);
    let client = async {
        let mut lines: Vec<StreamLine> = Vec::new();
        let mut answer = None;
        while let Some(l) = rx.recv().await {
            if let StreamLine::Log(LogEvent { body: LogBody::SubagentStarted { subagent_session_id, backend_id: Some(b), .. }, .. }) = &l
                && b == sleeper
            {
                answer = Some(host.execute(Command::Interrupt { session_id: subagent_session_id.clone() }, mpsc::channel(8).0).await);
            }
            lines.push(l);
        }
        (lines, answer.expect("the sleeper started"))
    };
    let (r, (lines, answer)) = tokio::join!(turn, client);
    (lines, r.unwrap().unwrap(), answer)
}

/// R-SUB-10: an interrupt with one Claude Code agent's child session id is
/// `stop_task` for that agent alone. Claude Code says it was killed, the
/// interrupt is answered, and the child ends interrupted — its frame and
/// its log — while the other agent completes and the parent's turn goes on
/// to its own end.
#[test]
fn r_sub_10_interrupting_one_claude_agent_stops_it_alone_and_the_turn_goes_on() {
    use krowk_harness::protocol::ChildState;
    let home = Home::new("kids-stop");
    let dir = home.signed_in("cfg-work");
    let (path, reader, sleeper) = stopping(&home, false);
    let host = home.host(vec![("claude:work", home.instance(&dir, Some(path.to_str().unwrap())))], trust::allow_all());
    let (lines, r, answer) = within(Box::pin(async {
        let out = stop_the_sleeper(&host, &sleeper).await;
        host.shutdown().await;
        out
    }));
    assert!(matches!(answer, Ok(None)), "taken: {:?}", answer.err());
    assert_eq!(r.status, TurnStatus::Completed, "the parent's turn goes on: {:?}", r.error);
    let fake = home.fake_log();
    assert_eq!(lines_of(&fake, "stop_task "), std::slice::from_ref(&sleeper), "that one alone: {fake}");
    let last = |kid: &str| {
        let frames: Vec<ChildState> = lines.iter().filter_map(|l| match l {
            StreamLine::Live(LiveEvent::SubagentStatus { session_id, status, .. }) if session_id == kid => Some(*status),
            _ => None,
        }).collect();
        let ended = home.events(kid).iter().rev().find_map(|e| if let LogBody::TurnCompleted { status, .. } = &e.body { Some(*status) } else { None });
        (frames.last().copied(), ended)
    };
    assert_eq!(last(&kid_of(&lines, &sleeper)), (Some(ChildState::Interrupted), Some(TurnStatus::Interrupted)));
    assert_eq!(last(&kid_of(&lines, &reader)), (Some(ChildState::Done), Some(TurnStatus::Completed)));
}

/// R-SUB-10: an agent waiting on the person's approval is stopped all the
/// same: the stop is written while the question waits, the question is
/// taken back — resolved, so the agent no longer reads waiting — and the
/// agent ends interrupted while the parent's turn goes on.
#[test]
fn r_sub_10_an_agent_waiting_on_an_approval_is_stopped_and_its_question_taken_back() {
    use krowk_harness::protocol::{ApprovalDecision, ChildState};
    let home = Home::new("kids-stop-asking");
    let dir = home.signed_in("cfg-work");
    let (path, printed) = asking(&home);
    let sleeper = printed.iter().find(|m| m["subtype"] == "task_started" && m["description"] == "sleeper").unwrap()["task_id"].as_str().unwrap().to_string();
    let host = home.host_answering(vec![("claude:work", home.instance(&dir, Some(path.to_str().unwrap())))]);
    let (lines, r, answer) = within(Box::pin(async {
        let (tx, mut rx) = mpsc::channel(4096);
        let turn = host.execute(prompt(None, "run two agents", "claude:work/sonnet", PermissionMode::Default), tx);
        let client = async {
            let mut lines: Vec<StreamLine> = Vec::new();
            let mut answer = None;
            while let Some(l) = rx.recv().await {
                if let StreamLine::Live(LiveEvent::ApprovalRequested(req)) = &l {
                    // Nobody answers it: the agent is stopped instead.
                    answer = Some(host.execute(Command::Interrupt { session_id: req.session_id.clone() }, mpsc::channel(8).0).await);
                }
                lines.push(l);
            }
            (lines, answer.expect("the sleeper asked"))
        };
        let (r, (lines, answer)) = tokio::join!(turn, client);
        host.shutdown().await;
        (lines, r.unwrap().unwrap(), answer)
    }));
    assert!(matches!(answer, Ok(None)), "taken while it waited: {:?}", answer.err());
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    let kid = kid_of(&lines, &sleeper);
    let req = lines.iter().find_map(|l| if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = l { Some(r.clone()) } else { None }).unwrap();
    assert_eq!(req.session_id, kid);
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ApprovalResolved { session_id, request_id, decision: ApprovalDecision::Deny, .. }) if *session_id == kid && *request_id == req.request_id)), "its question taken back");
    let last = lines.iter().rev().find_map(|l| match l {
        StreamLine::Live(LiveEvent::SubagentStatus { session_id, status, waiting, .. }) if *session_id == kid => Some((*status, *waiting)),
        _ => None,
    });
    assert_eq!(last, Some((ChildState::Interrupted, None)));
    let fake = home.fake_log();
    assert_eq!(lines_of(&fake, "stop_task "), std::slice::from_ref(&sleeper), "{fake}");
    // Written while the question waited, before its answer.
    let at = |p: &str| fake.lines().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("{p} in {fake}"));
    let denied = fake.lines().position(|l| l.starts_with("answer ") && l.contains(r#""behavior":"deny""#)).unwrap_or_else(|| panic!("denied: {fake}"));
    assert!(at("stop_task ") < denied, "{fake}");
}

/// R-SUB-10: a `stop_task` Claude Code refuses is an interrupt answered
/// `not_stoppable`, with Claude Code's words for why, and nothing else
/// happens to the turn.
#[test]
fn r_sub_10_a_stop_claude_code_refuses_is_answered_not_stoppable_with_why() {
    let home = Home::new("kids-refused");
    let dir = home.signed_in("cfg-work");
    let (path, _, sleeper) = stopping(&home, true);
    let mut instance = home.instance(&dir, Some(path.to_str().unwrap()));
    if let InstanceKind::ClaudeCode { env, .. } = &mut instance {
        env.insert("FAKE_CLAUDE_STOP_TASK".into(), "refuse".into());
    }
    let host = home.host(vec![("claude:work", instance)], trust::allow_all());
    let (_, r, answer) = within(Box::pin(async {
        let out = stop_the_sleeper(&host, &sleeper).await;
        host.shutdown().await;
        out
    }));
    let e = answer.expect_err("refused");
    assert_eq!(e.code, "not_stoppable");
    assert!(e.message.contains("Claude Code did not stop that agent: stop_task is not available in this mode"), "{}", e.message);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    assert_eq!(lines_of(&home.fake_log(), "stop_task "), [sleeper]);
}

/// R-SUB-10: a background agent running on between turns is stopped the
/// same way, by the loop that reads Claude Code then: its child ends
/// interrupted, written by the session's own writer, and is let go.
#[test]
fn r_sub_10_a_background_claude_agent_is_stopped_between_turns() {
    let home = Home::new("bg-stop");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_stop.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        let kid = kid_of(&lines, "a_bg_1");
        let out = || mpsc::channel(64).0;
        assert!(matches!(host.execute(Command::Interrupt { session_id: kid.clone() }, out()).await, Ok(None)));
        let mut ended = (false, false);
        while ended != (true, true) {
            match watch.recv().await.expect("the host is watched") {
                StreamLine::Log(LogEvent { session_id, body: LogBody::TurnCompleted { status: TurnStatus::Interrupted, .. }, .. }) if session_id == kid => ended.0 = true,
                StreamLine::Live(LiveEvent::SubagentStatus { session_id, status: krowk_harness::protocol::ChildState::Interrupted, .. }) if session_id == kid => ended.1 = true,
                _ => {}
            }
        }
        let e = host.execute(Command::Interrupt { session_id: kid.clone() }, out()).await.unwrap_err();
        assert_eq!(e.code, "no_running_turn", "let go once it ended");
        host.shutdown().await;
    }));
    assert_eq!(lines_of(&home.fake_log(), "stop_task "), ["a_bg_1"]);
}

/// R-SUB-7, R-SUB-9: a background agent's lines between turns, when no
/// turn runs to write them, are the session's writer's: each item into
/// its child log, in the order Claude Code said it, sent to the host's
/// watchers as the child's — a frame each time its state changes, as in a
/// turn — then its end. The turn Claude Code begins to answer it
/// (`origin.kind` task-notification) finds it already ended: its log
/// holds its whole turn before that turn's first line is sent.
#[test]
fn r_sub_7_a_background_agents_lines_between_turns_are_written_to_its_child_log_in_order() {
    let home = Home::new("bg-lines");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        let kid = kid_of(&lines, "a_bg_1");
        // Between turns: what the watchers are told of the child, until
        // Claude Code begins its turn.
        let mut told = Vec::new();
        loop {
            match watch.recv().await.expect("the host is watched") {
                StreamLine::Live(LiveEvent::TurnUnprompted { session_id, .. }) if session_id == first.session_id => break,
                l if l.session_id() == kid => told.push(l),
                _ => {}
            }
        }
        let read = told.iter().position(|l| matches!(l, StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolCall { name, .. }, .. }, .. }) if name == "Read"));
        let running = told.iter().position(|l| matches!(l, StreamLine::Live(LiveEvent::SubagentStatus { status: krowk_harness::protocol::ChildState::Running, .. })));
        let ended = told.iter().position(|l| matches!(l, StreamLine::Log(LogEvent { body: LogBody::TurnCompleted { status: TurnStatus::Completed, .. }, .. })));
        let done = told.iter().position(|l| matches!(l, StreamLine::Live(LiveEvent::SubagentStatus { status: krowk_harness::protocol::ChildState::Done, .. })));
        assert!(read.is_some() && read < running && running < ended && ended < done, "its call, its row's frame, its end, its last frame: {told:?}");
        // Its log: the turn begun in the parent's, its prompt, then what it
        // did between turns, as sent, and its end.
        let log = home.events(&kid);
        let sent: Vec<&str> = told.iter().filter_map(|l| if let StreamLine::Log(e) = l { Some(e.id.as_str()) } else { None }).collect();
        let tail: Vec<&str> = log[log.len() - sent.len()..].iter().map(|e| e.id.as_str()).collect();
        assert_eq!(tail, sent, "written as sent, in order: {log:?}");
        assert!(matches!(&log[2].body, LogBody::ItemCompleted { item: Item::UserText { text, .. }, .. } if text == "look around"), "{log:?}");
        assert!(log.iter().all(|e| e.session_id == kid));
        // Claude Code's turn: by its first line the child had ended.
        let (tx, mut rx) = mpsc::channel(256);
        let going = host.execute(continue_turn(&first.session_id), tx);
        let (r, ended_first) = tokio::join!(going, async {
            rx.recv().await.expect("the turn sends a line");
            matches!(home.events(&kid).last().map(|e| &e.body), Some(LogBody::TurnCompleted { status: TurnStatus::Completed, .. }))
        });
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        assert!(ended_first, "the agent's child ended before the turn answering it ran");
        host.shutdown().await;
    }));
}

/// R-SUB-7: a process let go with an agent running — the session moved to
/// another account — ends its child interrupted, saying why: its log's
/// turn completed and its last frame to the watchers.
#[test]
fn r_sub_7_a_background_agents_child_ends_interrupted_when_its_process_is_let_go() {
    let home = Home::new("bg-let-go");
    let dir = home.signed_in("cfg-work");
    let other = home.signed_in("cfg-other");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl"))), ("claude:other", home.instance(&other, None))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let a = r.unwrap().unwrap();
        let kid = kid_of(&lines, "a_bg_1");
        let (_, r) = run(&host, prompt(Some(&a.session_id), "go on over there", "claude:other/sonnet", PermissionMode::Default)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        let why = loop {
            match watch.recv().await.expect("the host is watched") {
                StreamLine::Log(LogEvent { session_id, body: LogBody::TurnCompleted { status, error, .. }, .. }) if session_id == kid => {
                    assert_eq!(status, TurnStatus::Interrupted);
                    break error.expect("why").message;
                }
                _ => {}
            }
        };
        assert!(why.contains("let go"), "{why}");
        let frame = watch.recv().await.unwrap();
        assert!(matches!(&frame, StreamLine::Live(LiveEvent::SubagentStatus { session_id, status: krowk_harness::protocol::ChildState::Interrupted, .. }) if *session_id == kid), "{frame:?}");
        host.shutdown().await;
    }));
}

/// R-SUB-7: a host shut down with an agent running ends its child before
/// it returns: the child's log has its turn completed, interrupted, the
/// moment `shutdown` is done.
#[test]
fn r_sub_7_a_host_shut_down_ends_its_background_agents_child_before_it_returns() {
    let home = Home::new("bg-shutdown");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl")))], trust::allow_all());
    let mut watch = host.watch();
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        r.unwrap().unwrap();
        let kid = kid_of(&lines, "a_bg_1");
        // Mid-way: its first call between turns has been written.
        loop {
            if let StreamLine::Log(LogEvent { session_id, body: LogBody::ItemCompleted { item: Item::ToolCall { .. }, .. }, .. }) = watch.recv().await.unwrap()
                && session_id == kid
            {
                break;
            }
        }
        host.shutdown().await;
        let log = home.events(&kid);
        assert!(matches!(log.last().map(|e| &e.body), Some(LogBody::TurnCompleted { status: TurnStatus::Interrupted, error: Some(e), .. }) if e.message.contains("shut down")), "{log:?}");
    }));
}

/// R-SUB-7: a prompt sent while a background agent's end is said but not
/// yet written — the writer between turns behind, as on a slow disk —
/// runs only once it is: the turn Claude Code began to answer the agent,
/// which the prompt runs first, finds its child ended by its first line.
#[test]
fn r_sub_7_a_prompt_while_a_background_agents_end_is_still_queued_finds_its_child_ended() {
    let home = Home::new("bg-queued");
    let dir = home.signed_in("cfg-work");
    let host = home.host(vec![("claude:work", home.instance(&dir, Some("background_agent.jsonl")))], trust::allow_all()).with_between_lag(std::time::Duration::from_millis(400));
    within(Box::pin(async {
        let (lines, r) = run(&host, prompt(None, "survey the repo in the background", "claude:work/sonnet", PermissionMode::Default)).await;
        let first = r.unwrap().unwrap();
        let kid = kid_of(&lines, "a_bg_1");
        // Claude Code has printed the agent's end and the turn it begins.
        while !home.fake_log().lines().any(|l| l.starts_with("out ") && l.contains("task-notification")) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let ended = |log: &[LogEvent]| matches!(log.last().map(|e| &e.body), Some(LogBody::TurnCompleted { .. }));
        assert!(!ended(&home.events(&kid)), "its end is still queued: the case this is about");
        let (tx, mut rx) = mpsc::channel(256);
        let going = host.execute(prompt(Some(&first.session_id), "anything else?", "claude:work/sonnet", PermissionMode::Default), tx);
        let (_, ended_first) = tokio::join!(going, async {
            rx.recv().await.expect("the turn sends a line");
            let at_first = ended(&home.events(&kid));
            while rx.recv().await.is_some() {}
            at_first
        });
        assert!(ended_first, "the agent's child ended before the turn ran");
        host.shutdown().await;
    }));
}
