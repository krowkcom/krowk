//! The Codex backend through the host, against a fake `codex` (a script
//! replaying recorded JSON-RPC, `fixtures/codex/fake-codex`), so no test
//! needs a real login: a tool-using turn that calls krowk's injected
//! session_info tool, one process serving the whole session, a resume from
//! a new host, approvals by krowk's modes, steering and an interrupt
//! mid-turn, a crash mid-turn, two instances with their own homes and
//! accounts, and the refusals — a looser sandbox, a missing login, a missing
//! binary, an untrusted repository. Every message either side sent is
//! checked against the schema pinned from the real `codex app-server`.

#![cfg(unix)]

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstanceKind, InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{Billing, Command, ContextRecord, Item, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine, TurnStatus, Usage, WireApi};
use krowk_harness::trust;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

const THREAD: &str = "01a0d8df-ba17-70c0-b9b6-9049bdb7a89c";

/// One test at a time in this binary: a process another test is spawning
/// holds a copy of every open descriptor between its fork and its exec — a
/// session log's lock included.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex").join(name)
}

struct Home {
    root: PathBuf,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Home {
    fn new(name: &str) -> Home {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("krowk-codex-backend-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n").unwrap();
        // The fake, installed as `codex`, executable whatever git kept.
        let bin = root.join("bin/codex");
        std::fs::copy(fixture("fake-codex"), &bin).unwrap();
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

    /// A signed-in Codex account: the fake's marker, never Codex's login file.
    fn signed_in(&self, dir: &str, account: &str) -> PathBuf {
        let d = self.root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("fake-login"), format!("{account}\n")).unwrap();
        d
    }

    /// An instance: the fake, a home, and the scenario in its environment —
    /// the instance's own env, as a router's settings would be.
    fn instance(&self, home: &Path, scenario: Option<&str>, extra: &[(&str, &str)]) -> InstanceKind {
        let mut env = BTreeMap::new();
        env.insert("FAKE_CODEX_LOG".to_string(), self.log_file().display().to_string());
        if let Some(s) = scenario {
            env.insert("FAKE_CODEX_SCENARIO".to_string(), fixture(s).display().to_string());
        }
        for (k, v) in extra {
            env.insert(k.to_string(), v.to_string());
        }
        InstanceKind::CodexAppServer { binary: Some(self.root.join("bin/codex").display().to_string()), codex_home: Some(home.display().to_string()), env, args: Vec::new(), api_key_env: None, effort: None }
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "PATH" => std::env::var("PATH").unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn host(&self, instances: Vec<(&str, InstanceKind)>, gate: trust::Gate) -> Host {
        let cfg = InstancesConfig { instances: instances.into_iter().map(|(n, k)| (n.to_string(), k)).collect(), ..Default::default() };
        Host::new(HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.root.join("repo"),
            registry: Registry::resolve(&cfg, &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.config/krowk/providers/credentials.json"),
            trust: gate,
            publisher: None,
            permissions: Default::default(),
            agents: krowk_harness::subagent::AgentsConfig::none(),
        })
    }

    fn events(&self, session_id: &str) -> Vec<LogEvent> {
        log::read_events(&log::sessions_dir(&self.env()).unwrap().join(session_id).join(log::EVENTS_FILE)).unwrap()
    }

    fn context(&self, session_id: &str) -> Vec<ContextRecord> {
        let raw = std::fs::read_to_string(log::sessions_dir(&self.env()).unwrap().join(session_id).join("context.jsonl")).unwrap();
        raw.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
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
    let (instance, model) = model.split_once('/').unwrap();
    Command::Prompt { session_id: session_id.map(String::from), text: text.into(), model: Some(ModelRef { instance: instance.into(), model: model.into() }), permission_mode: mode, toolset: None, effort: None, budget: None }
}

type Outcome = Result<Option<RunResult>, krowk_harness::engine::EngineError>;

/// Runs one command to its end, with every stream line it produced.
async fn run(host: &Host, cmd: Command) -> (Vec<StreamLine>, Outcome) {
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

/// Runs a prompt, calling `mid` each time a live delta arrives with the
/// host (to steer or interrupt it) and the deltas' count so far.
async fn run_with(host: &Host, cmd: Command, mut mid: impl AsyncFnMut(&Host, &str, usize)) -> (Vec<StreamLine>, Outcome) {
    let (tx, mut rx) = mpsc::channel(4096);
    let exec = host.execute(cmd, tx);
    tokio::pin!(exec);
    let mut lines = Vec::new();
    let mut deltas = 0;
    let mut session = String::new();
    let mut done = None;
    loop {
        tokio::select! {
            l = rx.recv() => match l {
                Some(l) => {
                    if let StreamLine::Log(e) = &l { session = e.session_id.clone(); }
                    let delta = matches!(&l, StreamLine::Live(LiveEvent::ItemDelta { .. }));
                    lines.push(l);
                    if delta {
                        deltas += 1;
                        mid(host, &session, deltas).await;
                    }
                }
                None => break,
            },
            r = &mut exec, if done.is_none() => done = Some(r),
        }
    }
    (lines, done.expect("the command ran"))
}

fn items(events: &[LogEvent]) -> Vec<Item> {
    events.iter().filter_map(|e| if let LogBody::ItemCompleted { item, .. } = &e.body { Some(item.clone()) } else { None }).collect()
}

fn backend_sessions(events: &[LogEvent]) -> Vec<(String, Option<String>, Option<Billing>)> {
    events
        .iter()
        .filter_map(|e| if let LogBody::BackendSession { vendor_session_id, transcript_path, billing, .. } = &e.body { Some((vendor_session_id.clone(), transcript_path.clone(), *billing)) } else { None })
        .collect()
}

/// The lines of the fake's log with a prefix, e.g. `in ` or `out `.
fn lines_of(log: &str, prefix: &str) -> Vec<String> {
    log.lines().filter_map(|l| l.strip_prefix(prefix)).map(String::from).collect()
}

/// The `codex app-server` processes that served turns. The readiness check
/// starts one too, before a session's first turn, that is asked only
/// `initialize` and `account/read`; a backend's goes on to `model/list`.
fn backend_processes(log: &str) -> usize {
    lines_of(log, "in ").iter().filter(|l| l.contains(r#""method":"model/list""#)).count()
}

#[test]
fn r_back_3_a_tool_using_turn_on_a_codex_instance_completes_and_is_logged() {
    let h = Home::new("tool-use");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    // A skill the person added to their own Codex after the account was set
    // up: linked into the account's home before the process starts.
    std::fs::create_dir_all(h.root.join("home/.codex/skills/later")).unwrap();
    let host = h.host(vec![("codex:team", h.instance(&home, Some("tool_use.jsonl"), &[]))], trust::allow_all());
    rt().block_on(async {
        let (lines, r) = run(&host, prompt(None, "what is here?", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        assert_eq!(std::fs::read_link(home.join("skills/later")).unwrap(), h.root.join("home/.codex/skills/later"), "a skill added later reaches the account");
        let r = r.unwrap().unwrap();
        assert_eq!((r.status, r.result.as_str(), r.num_model_calls), (TurnStatus::Completed, "There is one file, README.md.", 2), "{r:?}");
        // Codex's own subagent thread made one call: metered once, as the
        // session's spend, and none of its conversation (R-SUB-4).
        assert_eq!((r.usage.input_tokens, r.usage.cache_read_tokens, r.usage.output_tokens, r.usage.reasoning_tokens), (600, 2300, 39, 40));
        let subagent: Vec<Usage> = h.events(&r.session_id).iter().filter_map(|e| if let LogBody::SubagentResponse { usage, .. } = &e.body { Some(*usage) } else { None }).collect();
        assert_eq!(subagent, [Usage { input_tokens: 200, cache_read_tokens: 100, output_tokens: 7, ..Usage::default() }], "r_sub_4: Codex's subagent thread is metered");
        assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ItemDelta { .. }))), "the answer streamed");

        let events = h.events(&r.session_id);
        let started = events.iter().find_map(|e| if let LogBody::TurnStarted { provider, wire_api, .. } = &e.body { Some((provider.clone(), *wire_api)) } else { None }).unwrap();
        assert_eq!(started, ("openai".to_string(), WireApi::CodexAppServer));
        // The vendor's session is on the record: its thread, where Codex
        // keeps the transcript — in this account's home — and the billing.
        let sessions = backend_sessions(&events);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, THREAD);
        let transcript = sessions[0].1.clone().unwrap();
        assert!(transcript.starts_with(&home.display().to_string()) && transcript.ends_with(&format!("{THREAD}.jsonl")) && Path::new(&transcript).exists(), "{transcript}");
        assert_eq!(sessions[0].2, Some(Billing::Subscription));

        let got = items(&events);
        let kinds: Vec<&str> = got
            .iter()
            .map(|i| match i {
                Item::UserText { .. } => "user",
                Item::AssistantText { .. } => "assistant",
                Item::Reasoning { .. } => "reasoning",
                Item::ToolCall { .. } => "call",
                Item::ToolResult { .. } => "result",
            })
            .collect();
        assert_eq!(kinds, ["user", "reasoning", "call", "result", "call", "result", "assistant"], "{got:?}");
        assert!(matches!(&got[2], Item::ToolCall { name, input, call_id } if name == "shell" && input["command"] == "/bin/bash -lc ls" && call_id == "call-ls"));
        assert!(matches!(&got[3], Item::ToolResult { output, is_error: false, .. } if output == "README.md\n"));
        // krowk's own tool, injected as Codex's dynamic tools and answered by
        // the bridge: the result is what krowk said.
        assert!(matches!(&got[4], Item::ToolCall { name, .. } if name == "mcp__krowk__session_info"));
        let Item::ToolResult { output, is_error: false, .. } = &got[5] else { panic!("{:?}", got[5]) };
        assert!(output.contains(&format!("krowk session: {}", r.session_id)) && output.contains("instance: codex:team") && output.contains("backend: codex-app-server"), "{output}");
        let responses: Vec<usize> = events.iter().filter_map(|e| if let LogBody::ResponseCompleted { item_ids, .. } = &e.body { Some(item_ids.len()) } else { None }).collect();
        assert_eq!(responses, [3, 1], "each model call names the model output it made");

        let ctx = h.context(&r.session_id);
        assert_eq!((ctx[0].toolset.as_str(), ctx[0].wire_api, ctx[0].tools[0].name.as_str()), ("codex-app-server", WireApi::CodexAppServer, "mcp__krowk__session_info"));
        assert!(ctx[0].system.contains("Codex's own system prompt"));

        // What krowk told Codex: itself as the client, the thread in its
        // read-only sandbox with krowk reviewing, krowk's tools, the prompt.
        let log = h.fake_log();
        let sent = lines_of(&log, "in ");
        assert!(sent[0].contains(r#""method":"initialize""#) && sent[0].contains(r#""name":"krowk""#) && sent[0].contains(r#""experimentalApi":true"#), "{}", sent[0]);
        let start = sent.iter().find(|l| l.contains(r#""method":"thread/start""#)).unwrap();
        for want in [r#""sandbox":"read-only""#, r#""approvalPolicy":"on-request""#, r#""approvalsReviewer":"user""#, r#""dynamicTools":[{"type":"namespace","name":"krowk""#] {
            assert!(start.contains(want), "{want} in {start}");
        }
        let answer = sent.iter().find(|l| l.starts_with(r#"{"id":"srv-1""#)).unwrap();
        assert!(answer.contains(r#""success":true"#), "{answer}");

        // The next turn is the same process and the same thread.
        let (_, r2) = run(&host, prompt(Some(&r.session_id), "and now?", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        assert_eq!(r2.unwrap().unwrap().result, "Still one file.");
        let log = h.fake_log();
        assert_eq!(backend_processes(&log), 1, "one process served the session");
        assert!(!log.lines().any(|l| l.starts_with("resume ")), "an open thread is not resumed");
        assert_eq!(backend_sessions(&h.events(&r.session_id)).len(), 1, "an unchanged vendor session is not logged twice");
        host.shutdown().await;
    });
}

#[test]
fn r_back_5_a_new_host_resumes_the_codex_thread_the_log_names() {
    let h = Home::new("resume");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    let session = rt().block_on(async {
        let host = h.host(vec![("codex:team", h.instance(&home, Some("tool_use.jsonl"), &[]))], trust::allow_all());
        let (_, r) = run(&host, prompt(None, "what is here?", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        host.shutdown().await;
        r.unwrap().unwrap().session_id
    });
    // A new krowk: a new process, on the thread the log holds.
    rt().block_on(async {
        let host = h.host(vec![("codex:team", h.instance(&home, Some("tool_use.jsonl"), &[]))], trust::allow_all());
        let (_, r) = run(&host, Command::Prompt { session_id: Some(session.clone()), text: "and now?".into(), model: None, permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }).await;
        let r = r.unwrap().unwrap();
        assert_eq!((r.status, r.result.as_str(), r.model.instance.as_str()), (TurnStatus::Completed, "Still one file.", "codex:team"), "the session's model, on the same instance");
        host.shutdown().await;
    });
    let log = h.fake_log();
    assert_eq!(backend_processes(&log), 2);
    assert!(log.contains(&format!("resume {THREAD}")), "{log}");
    let resume = lines_of(&log, "in ").into_iter().find(|l| l.contains("thread/resume")).unwrap();
    assert!(resume.contains(r#""sandbox":"read-only""#) && resume.contains(r#""approvalsReviewer":"user""#), "a resumed thread is put back in krowk's mode: {resume}");
}

/// An MCP server the person's config names, as `config/read` reports one.
const MCP: &str = r#"{"tripwire":{"command":"/bin/sh","args":["-c","touch ran"],"enabled":true}}"#;

#[test]
fn r_back_3_codexs_approval_requests_are_answered_by_krowks_modes() {
    let h = Home::new("approvals");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    for (mode, patch) in [(PermissionMode::Default, "decline"), (PermissionMode::AcceptEdits, "accept")] {
        let _ = std::fs::remove_file(home.join("fake-turns"));
        let _ = std::fs::remove_file(h.log_file());
        let host = h.host(vec![("codex:team", h.instance(&home, Some("approvals.jsonl"), &[("FAKE_CODEX_MCP", MCP)]))], trust::allow_all());
        rt().block_on(async {
            let (_, r) = run(&host, prompt(None, "tidy up", "codex:team/gpt-5.5", mode)).await;
            let r = r.unwrap().unwrap();
            assert_eq!(r.status, TurnStatus::Completed);
            let answers: Vec<String> = lines_of(&h.fake_log(), "in ").into_iter().filter(|l| l.starts_with(r#"{"id":"srv-"#)).collect();
            assert_eq!(answers.len(), 4, "{answers:?}");
            // The person's MCP servers are off on the thread outside bypass.
            let start = lines_of(&h.fake_log(), "in ").into_iter().find(|l| l.contains("thread/start")).unwrap();
            assert!(start.contains(r#""config":{"mcp_servers":{"tripwire":{"enabled":false}}}"#), "{start}");
            assert!(answers[0].contains(r#""decision":"decline""#), "a command beyond the sandbox needs bypassPermissions: {}", answers[0]);
            assert!(answers[1].contains(&format!(r#""decision":"{patch}""#)), "{mode:?}: {}", answers[1]);
            assert!(answers[2].contains(r#""decision":"decline""#), "never .codex, whatever the mode: {}", answers[2]);
            assert!(answers[3].contains(r#""decision":"decline""#), "a move into .git is judged by where it lands: {}", answers[3]);
            // The log keeps krowk's reason where Codex told the model only
            // that it was declined.
            let results: Vec<(String, bool)> = items(&h.events(&r.session_id)).into_iter().filter_map(|i| if let Item::ToolResult { output, is_error, .. } = i { Some((output, is_error)) } else { None }).collect();
            assert!(results[0].1 && results[0].0.contains("bypassPermissions"), "{:?}", results[0]);
            assert!(results[2].1 && results[2].0.contains("inside a .codex directory"), "{:?}", results[2]);
            assert!(results[3].1 && results[3].0.contains("inside a .git directory"), "{:?}", results[3]);
            host.shutdown().await;
        });
    }
    // bypassPermissions is Codex's full access, asked about nothing.
    let _ = std::fs::remove_file(h.log_file());
    let host = h.host(vec![("codex:team", h.instance(&home, None, &[("FAKE_CODEX_MCP", MCP)]))], trust::allow_all());
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "go", "codex:team/gpt-5.5", PermissionMode::BypassPermissions)).await;
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        host.shutdown().await;
    });
    let start = lines_of(&h.fake_log(), "in ").into_iter().find(|l| l.contains("thread/start")).unwrap();
    assert!(start.contains(r#""sandbox":"danger-full-access""#) && start.contains(r#""approvalPolicy":"never""#), "{start}");
    assert!(!start.contains(r#""config""#) && !h.fake_log().contains("config-read"), "bypassPermissions leaves Codex's MCP servers on: {start}");
}

#[test]
fn r_back_3_steering_reaches_the_running_codex_turn_and_none_is_lost() {
    let h = Home::new("steer");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    let host = h.host(vec![("codex:team", h.instance(&home, Some("steer.jsonl"), &[]))], trust::allow_all());
    rt().block_on(async {
        // The first delta: steer the running turn, which Codex takes. The
        // second, the turn's last answer: steer again — Codex refuses, its
        // turn is ending — so krowk gives it to another Codex turn.
        let (_, r) = run_with(&host, prompt(None, "count to three", "codex:team/gpt-5.5", PermissionMode::Default), async |host: &Host, session: &str, n: usize| {
            let text = match n {
                1 => "count in French",
                2 => "and in German",
                _ => return,
            };
            let (tx, _rx) = mpsc::channel(8);
            host.execute(Command::Steer { session_id: session.into(), text: text.into() }, tx).await.unwrap();
        })
        .await;
        let r = r.unwrap().unwrap();
        assert_eq!((r.status, r.result.as_str()), (TurnStatus::Completed, "And in German: eins, zwei, drei."));
        assert!(r.unread_steers.is_empty(), "a completed turn has read all its steering");
        let users: Vec<String> = items(&h.events(&r.session_id)).into_iter().filter_map(|i| if let Item::UserText { text } = i { Some(text) } else { None }).collect();
        assert_eq!(users, ["count to three", "count in French", "and in German"], "each steer is logged where it landed, once");
        let log = h.fake_log();
        assert!(lines_of(&log, "steer ").iter().any(|l| l.contains(r#""expectedTurnId":"01a0d8df-e17f-7ed2-b3ff-6bfc42900001""#) && l.contains("count in French")), "{log}");
        assert!(log.contains("refused ") && log.contains("and in German"));
        let second = lines_of(&log, "in ").into_iter().filter(|l| l.contains(r#""method":"turn/start""#)).nth(1).unwrap();
        assert!(second.contains("and in German"), "the refused steer is the next Codex turn's input: {second}");
        host.shutdown().await;
    });
}

#[test]
fn r_back_3_an_interrupt_mid_turn_keeps_what_arrived_and_the_session_goes_on() {
    let h = Home::new("interrupt");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    let host = h.host(vec![("codex:team", h.instance(&home, Some("interrupt.jsonl"), &[]))], trust::allow_all());
    rt().block_on(async {
        let (_, r) = run_with(&host, prompt(None, "count for a long time", "codex:team/gpt-5.5", PermissionMode::Default), async |host: &Host, session: &str, n: usize| {
            if n == 1 {
                let (tx, _rx) = mpsc::channel(8);
                host.execute(Command::Interrupt { session_id: session.into() }, tx).await.unwrap();
            }
        })
        .await;
        let r = r.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Interrupted);
        assert!(h.fake_log().lines().any(|l| l == "interrupt"), "turn/interrupt reached Codex");
        let got = items(&h.events(&r.session_id));
        assert!(got.iter().any(|i| matches!(i, Item::AssistantText { text } if text == "1\n2\n3\n")), "what arrived is kept: {got:?}");
        // The process lives on and serves the next turn.
        let (_, r2) = run(&host, prompt(Some(&r.session_id), "go on", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        assert_eq!(r2.unwrap().unwrap().result, "Picking up where we left off.");
        assert_eq!(backend_processes(&h.fake_log()), 1);
        host.shutdown().await;
    });
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn r_back_3_a_codex_that_crashes_mid_turn_fails_the_turn_and_the_next_one_resumes() {
    let h = Home::new("crash");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    let host = h.host(vec![("codex:team", h.instance(&home, Some("crash.jsonl"), &[]))], trust::allow_all());
    rt().block_on(async {
        let (_, r) = run(&host, prompt(None, "work", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        let r = r.unwrap().unwrap();
        let e = r.error.clone().unwrap();
        assert_eq!((r.status, e.code.as_str()), (TurnStatus::Failed, "backend_exited"), "{e:?}");
        assert!(e.message.contains("--resume"), "{}", e.message);
        let got = items(&h.events(&r.session_id));
        assert!(got.iter().any(|i| matches!(i, Item::AssistantText { text } if text == "Halfway through")), "what it said before it died is kept: {got:?}");
        // What it started went with it: the whole group is stopped.
        let pid: i32 = h.fake_log().lines().find_map(|l| l.strip_prefix("grandchild ")).unwrap().parse().unwrap();
        let gone = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            !alive(pid)
        });
        assert!(gone, "the command Codex started ({pid}) outlived it");
        // The next turn is a new process on the same thread.
        let (_, r2) = run(&host, prompt(Some(&r.session_id), "again", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        assert_eq!(r2.unwrap().unwrap().result, "Back again.");
        let log = h.fake_log();
        assert_eq!(backend_processes(&log), 2);
        assert!(log.contains(&format!("resume {THREAD}")));
        host.shutdown().await;
    });
}

#[test]
fn r_inst_1_two_codex_instances_with_their_own_homes_run_sessions_under_their_own_accounts() {
    let h = Home::new("instances");
    let team = h.signed_in("codex-team", "chatgpt team@example.com");
    let personal = h.signed_in("codex-personal", "apiKey");
    let host = h.host(
        vec![("codex:team", h.instance(&team, None, &[("FAKE_CODEX_THREAD", "01a0d8df-0000-7000-8000-00000000000a")])), ("codex:personal", h.instance(&personal, None, &[("FAKE_CODEX_THREAD", "01a0d8df-0000-7000-8000-00000000000b")]))],
        trust::allow_all(),
    );
    rt().block_on(async {
        let mut seen = Vec::new();
        for (instance, home, thread, billing) in [("codex:team", &team, "0a", Billing::Subscription), ("codex:personal", &personal, "0b", Billing::ApiKey)] {
            let (_, r) = run(&host, prompt(None, "hi", &format!("{instance}/gpt-5.5"), PermissionMode::Default)).await;
            let r = r.unwrap().unwrap();
            assert_eq!((r.status, r.model.instance.as_str()), (TurnStatus::Completed, instance));
            let s = backend_sessions(&h.events(&r.session_id));
            assert!(s[0].0.ends_with(thread), "{s:?}");
            assert!(s[0].1.as_deref().unwrap().starts_with(&home.display().to_string()), "the transcript is in the account's own home: {s:?}");
            assert_eq!(s[0].2, Some(billing), "each account's own billing (R-INST-3)");
            seen.push(r.session_id);
        }
        assert_ne!(seen[0], seen[1]);
        host.shutdown().await;
    });
    // Each process ran with its own CODEX_HOME, and Codex answered with that
    // account.
    let log = h.fake_log();
    for home in [&team, &personal] {
        assert!(log.contains(&format!("home {}", home.display())), "{log}");
    }
    let accounts: Vec<String> = lines_of(&log, "out ").into_iter().filter(|l| l.contains(r#""account":"#)).collect();
    // The readiness check and then the backend ask each account.
    assert!(accounts[0].contains("team@example.com") && accounts.last().unwrap().contains(r#""type":"apiKey""#), "{accounts:?}");
}

#[test]
fn r_back_3_a_looser_sandbox_a_missing_login_or_binary_and_an_untrusted_repository_are_refused() {
    let h = Home::new("refusals");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    rt().block_on(async {
        // A config (or a managed requirement) that makes Codex open the
        // thread looser than asked: stopped before a turn runs.
        for (k, v) in [("FAKE_CODEX_SANDBOX", "dangerFullAccess"), ("FAKE_CODEX_SANDBOX", "workspaceWrite"), ("FAKE_CODEX_REVIEWER", "auto_review")] {
            let host = h.host(vec![("codex:team", h.instance(&home, None, &[(k, v)]))], trust::allow_all());
            let (_, r) = run(&host, prompt(None, "hi", "codex:team/gpt-5.5", PermissionMode::AcceptEdits)).await;
            let r = r.unwrap().unwrap();
            assert_eq!(r.error.as_ref().map(|e| e.code.as_str()), Some("backend_permission_mode"), "{k}={v}: {r:?}");
            assert!(!lines_of(&h.fake_log(), "in ").iter().any(|l| l.contains("turn/start")), "no turn ran");
            let _ = std::fs::remove_file(h.log_file());
        }
        // No login: Codex's account/read says so, before any thread.
        let empty = h.root.join("codex-empty");
        std::fs::create_dir_all(&empty).unwrap();
        let host = h.host(vec![("codex:empty", h.instance(&empty, None, &[]))], trust::allow_all());
        // Refused by the readiness check, before a session exists.
        let (_, r) = run(&host, prompt(None, "hi", "codex:empty/gpt-5.5", PermissionMode::Default)).await;
        let e = r.unwrap_err();
        assert_eq!((e.code.as_str(), e.status), ("not_authenticated", 401));
        assert!(e.message.contains("krowk providers add codex --name empty"), "{}", e.message);
        assert!(!h.fake_log().contains("thread/start"));
        // A login Codex finds stale mid-turn, as a real Codex reported it.
        let host = h.host(vec![("codex:team", h.instance(&home, Some("unauthorized.jsonl"), &[]))], trust::allow_all());
        let (_, r) = run(&host, prompt(None, "hi", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        let e = r.unwrap().unwrap().error.unwrap();
        assert_eq!(e.code, "not_authenticated", "{e:?}");
        host.shutdown().await;
        // No binary: named before a session exists.
        let mut gone = h.instance(&home, None, &[]);
        if let InstanceKind::CodexAppServer { binary, .. } = &mut gone {
            *binary = Some(h.root.join("bin/no-such-codex").display().to_string());
        }
        let host = h.host(vec![("codex:gone", gone)], trust::allow_all());
        let (lines, r) = run(&host, prompt(None, "hi", "codex:gone/gpt-5.5", PermissionMode::Default)).await;
        assert_eq!(r.unwrap_err().code, "backend_not_found");
        assert!(lines.is_empty(), "no session was created");
        // An untrusted repository: the gate refuses, and nothing is spawned.
        let _ = std::fs::remove_file(h.log_file());
        let refuse: trust::Gate = Arc::new(|root: &Path| Err(trust::untrusted(root, "Pass --trust.")));
        let host = h.host(vec![("codex:team", h.instance(&home, None, &[]))], refuse);
        let (lines, r) = run(&host, prompt(None, "hi", "codex:team/gpt-5.5", PermissionMode::Default)).await;
        assert_eq!(r.unwrap_err().code, "untrusted_directory");
        assert!(lines.is_empty() && h.fake_log().is_empty(), "nothing ran");
    });
}

/// The pinned schema, with `definitions` at its root, as a validator for
/// one of its definitions.
fn validator(bundle: &Value, name: &str) -> jsonschema::Validator {
    let schema = serde_json::json!({"$schema": "http://json-schema.org/draft-07/schema#", "$ref": format!("#/definitions/{name}"), "definitions": bundle["definitions"]});
    jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The response type each request is answered with, by method.
fn response_type(method: &str) -> Option<&'static str> {
    Some(match method {
        "initialize" => "InitializeResponse",
        "account/read" => "v2/GetAccountResponse",
        "model/list" => "v2/ModelListResponse",
        "thread/start" => "v2/ThreadStartResponse",
        "thread/resume" => "v2/ThreadResumeResponse",
        "config/read" => "v2/ConfigReadResponse",
        "turn/start" => "v2/TurnStartResponse",
        "turn/steer" => "v2/TurnSteerResponse",
        "turn/interrupt" => "v2/TurnInterruptResponse",
        "item/commandExecution/requestApproval" => "CommandExecutionRequestApprovalResponse",
        "item/fileChange/requestApproval" => "FileChangeRequestApprovalResponse",
        "item/tool/call" => "DynamicToolCallResponse",
        _ => return None,
    })
}

#[test]
fn r_back_3_every_message_krowk_sends_and_codex_answers_matches_the_pinned_schema() {
    let bundle: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/codex/codex_app_server_protocol.schemas.json")).unwrap()).unwrap();
    let h = Home::new("schema");
    let home = h.signed_in("codex-team", "chatgpt team@example.com");
    rt().block_on(async {
        for (scenario, mode) in [("tool_use.jsonl", PermissionMode::Default), ("approvals.jsonl", PermissionMode::AcceptEdits), ("interrupt.jsonl", PermissionMode::Default)] {
            let _ = std::fs::remove_file(home.join("fake-turns"));
            let host = h.host(vec![("codex:team", h.instance(&home, Some(scenario), &[("FAKE_CODEX_MCP", MCP)]))], trust::allow_all());
            let (_, r) = run_with(&host, prompt(None, "hi", "codex:team/gpt-5.5", mode), async |host: &Host, session: &str, n: usize| {
                if scenario == "interrupt.jsonl" && n == 1 {
                    let (tx, _rx) = mpsc::channel(8);
                    host.execute(Command::Interrupt { session_id: session.into() }, tx).await.unwrap();
                }
            })
            .await;
            let session = r.unwrap().unwrap().session_id;
            // The scenarios with a second turn: a resumed thread's messages too.
            if scenario != "approvals.jsonl" {
                let (_, _) = run(&host, prompt(Some(&session), "again", "codex:team/gpt-5.5", mode)).await;
            }
            host.shutdown().await;
        }
    });
    let log = h.fake_log();
    let (requests, notifications, server_requests, answers) =
        (validator(&bundle, "ClientRequest"), validator(&bundle, "ClientNotification"), validator(&bundle, "ServerRequest"), validator(&bundle, "ServerNotification"));
    // Who asked what, by id, so each answer is checked against its type.
    let mut asked: BTreeMap<String, String> = BTreeMap::new();
    let mut checked = 0;
    for (dir, line) in log.lines().filter_map(|l| l.strip_prefix("in ").map(|x| ("in", x)).or_else(|| l.strip_prefix("out ").map(|x| ("out", x)))) {
        let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{dir} {line}: {e}"));
        let id = v.get("id").map(|i| i.to_string());
        let method = v.get("method").and_then(Value::as_str).map(String::from);
        let (validator, what) = match (dir, &method, &id) {
            ("in", Some(m), Some(i)) => {
                asked.insert(i.clone(), m.clone());
                (&requests, format!("krowk's {m}"))
            }
            ("in", Some(m), None) => (&notifications, format!("krowk's {m}")),
            ("out", Some(m), Some(i)) => {
                asked.insert(i.clone(), m.clone());
                (&server_requests, format!("codex's {m}"))
            }
            ("out", Some(m), None) => (&answers, format!("codex's {m}")),
            (_, None, Some(i)) => {
                let Some(m) = asked.get(i) else { panic!("{dir}: an answer to nothing: {line}") };
                if v.get("error").is_some() {
                    continue;
                }
                let Some(ty) = response_type(m) else { continue };
                let one = validator(&bundle, ty);
                let errors: Vec<String> = one.iter_errors(&v["result"]).map(|e| format!("{e} at {}", e.instance_path())).collect();
                assert!(errors.is_empty(), "the answer to {m} is not a {ty}: {errors:?}\n{line}");
                checked += 1;
                continue;
            }
            _ => panic!("{dir}: {line}"),
        };
        let errors: Vec<String> = validator.iter_errors(&v).map(|e| format!("{e} at {}", e.instance_path())).collect();
        assert!(errors.is_empty(), "{what} does not match the pinned schema: {errors:?}\n{line}");
        checked += 1;
    }
    assert!(checked > 60, "only {checked} messages were checked");
}
