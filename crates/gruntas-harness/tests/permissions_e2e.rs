//! Asking for permission through the host (R-PERM-2), for a session
//! running in Claude Code — the fake `claude` (`common/fake.rs`): a call
//! Claude Code asks gruntas about (`can_use_tool`) that gruntas's rules say to
//! ask a person about is an approval request a client answers over the
//! protocol, and with nobody to ask it is refused at once, saying what
//! would allow it. Claude Code's own permissions, hooks, skills and
//! instructions are Claude Code's, and tested with it.
#![cfg(unix)]

#[path = "common/fake.rs"]
mod fake;

use gruntas_harness::host::{Host, HostConfig};
use gruntas_harness::instances::{InstancesConfig, Registry};
use gruntas_harness::log;
use gruntas_harness::permissions::Config;
use gruntas_harness::protocol::{ApprovalDecision, Command, Item, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine};
use std::path::PathBuf;
use std::sync::Arc;

struct Home {
    root: PathBuf,
    instances: InstancesConfig,
}

impl Home {
    /// A home whose one instance, `claude`, is the fake playing `scenario`.
    fn new(name: &str, scenario: &str) -> Home {
        let root = std::env::temp_dir().join(format!("krowk-perm-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        let root = root.canonicalize().unwrap();
        let instances = InstancesConfig { instances: [("claude".to_string(), fake::claude(&root, "claude", Some(scenario)))].into_iter().collect(), ..Default::default() };
        Home { root, instances }
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

    fn host(&self, permissions: Config) -> Host {
        Host::new(HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.repo(),
            registry: Registry::resolve(&self.instances, &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.gruntas/credentials.json"),
            trust: gruntas_harness::trust::allow_all(),
            publisher: None,
            permissions: Config { home: Some(self.root.join("home")), krowk_dir: Some(self.root.join("home/.gruntas")), ..permissions },
            agents: gruntas_harness::agents::AgentsConfig::none(),
            session: Default::default(),
        })
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn prompt(text: &str, mode: PermissionMode) -> Command {
    Command::Prompt {
        rollover: None,
        session_id: None,
        text: text.into(), images: Vec::new(),
        model: Some(ModelRef { instance: "claude".into(), model: "sonnet".into() }),
        permission_mode: mode,
        toolset: None,
        effort: None,
        budget: None,
    }
}

/// A model that runs one bash command — Claude Code asking gruntas first —
/// then answers. The fake makes `file`, as the command would, only if it
/// was allowed.
fn one_bash(command: &str, file: &str) -> String {
    format!("{}\n@touch {file}\n{}", fake::asks_bash("toolu_01Call", command), fake::answer("Done."))
}

/// Runs a prompt with no client answering approvals, as `gruntas -p` does.
fn run(host: &Host, cmd: Command) -> (Vec<StreamLine>, RunResult) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let collect = async {
            let mut lines = Vec::new();
            while let Some(l) = rx.recv().await {
                lines.push(l);
            }
            lines
        };
        let (r, lines) = tokio::join!(host.execute(cmd, tx), collect);
        host.shutdown().await;
        (lines, r.unwrap().unwrap())
    })
}

/// The text of the tool result the model read back.
fn tool_result_sent(lines: &[StreamLine]) -> String {
    lines.iter().find_map(|l| match l {
        StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, .. }, .. }, .. }) => Some(output.clone()),
        _ => None,
    }).expect("a tool result was sent")
}

#[test]
fn r_perm_2_an_asked_call_is_an_approval_request_a_client_answers_over_the_protocol() {
    let h = Home::new("approve", &one_bash("touch approved.txt", "approved.txt"));
    let host = h.host(Config { approvals: true, ..Config::default() });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    for (decision, runs) in [(ApprovalDecision::Deny, false), (ApprovalDecision::AllowSession, true)] {
        let _ = std::fs::remove_file(h.repo().join("approved.txt"));
        let lines = rt.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
            let turn = host.execute(prompt("make the file", PermissionMode::Default), tx);
            // The client: whatever asks, it answers, as an attached client does — by the request's id, with `approve`.
            let client = async {
                let mut lines = Vec::new();
                while let Some(l) = rx.recv().await {
                    if let StreamLine::Live(LiveEvent::ApprovalRequested(req)) = &l {
                        let (itx, _) = tokio::sync::mpsc::channel(1);
                        host.execute(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision, answers: Vec::new() }, itx).await.unwrap();
                    }
                    lines.push(l);
                }
                lines
            };
            let (r, lines) = tokio::join!(turn, client);
            assert_eq!(r.unwrap().unwrap().result, "Done.");
            lines
        });
        let req = lines.iter().find_map(|l| if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = l { Some(r.clone()) } else { None }).expect("an approval.requested frame");
        assert_eq!((req.tool.as_str(), req.summary.as_str()), ("Bash", "Bash `touch approved.txt`"));
        assert!(req.reason.contains("runs a command") && req.remember == ["Bash(touch approved.txt)"], "{req:?}");
        assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ApprovalResolved { decision: d, .. }) if *d == decision)), "every client hears it was answered");
        assert_eq!(h.repo().join("approved.txt").exists(), runs, "{decision:?}");
        // Claude Code was answered as the person said, and the model reads a denial's reason.
        let answered = fake::log(&h.root, "claude");
        let behavior = if runs { r#""behavior":"allow""# } else { r#""behavior":"deny""# };
        assert!(answered.lines().rfind(|l| l.starts_with("answer ")).is_some_and(|l| l.contains(behavior)), "{answered}");
        if !runs {
            assert!(tool_result_sent(&lines).contains("the person declined"), "{}", tool_result_sent(&lines));
        }
    }
    // A request nobody is waiting on is refused, not ignored.
    let (itx, _) = tokio::sync::mpsc::channel(1);
    let e = rt.block_on(host.execute(Command::Approve { session_id: "s".into(), request_id: "r".into(), decision: ApprovalDecision::Allow, answers: Vec::new() }, itx)).unwrap_err();
    assert_eq!(e.code, "no_approval_request");
    rt.block_on(host.shutdown());
}

#[test]
fn r_perm_2_headless_never_waits_on_an_ask_and_says_what_would_allow_it() {
    let h = Home::new("headless", &one_bash("npm test", "tested"));
    let host = h.host(Config::default());
    let t = std::time::Instant::now();
    let (lines, r) = run(&host, prompt("run the tests", PermissionMode::Default));
    assert!(t.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(r.result, "Done.");
    assert!(!lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ApprovalRequested(_)))), "nobody is sent a request nobody can answer");
    let sent = tool_result_sent(&lines);
    assert!(sent.contains("nobody is here to give it") && sent.contains("`Bash(npm test)`") && sent.contains("--permission-mode bypassPermissions"), "{sent}");
    assert!(!h.repo().join("tested").exists(), "the command was refused");
}
