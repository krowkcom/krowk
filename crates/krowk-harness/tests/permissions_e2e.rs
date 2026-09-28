//! Claude-compatible permissions, instructions, skills and hooks, end to
//! end through the host and the native loop against a stand-in Anthropic
//! API (R-PERM-1, R-PERM-2, R-COMPAT-1): a repository set up for Claude
//! Code — `.claude/settings.json`, `.claude/skills`, `AGENTS.md` — and what
//! krowk does with it, read from the requests the model was sent and the
//! files left behind. Fixtures only: no `claude` binary is run.

#[path = "common/mock.rs"]
mod mock;

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::permissions::Config;
use krowk_harness::protocol::{ApprovalDecision, Command, ContextRecord, LiveEvent, ModelRef, PermissionMode, RunResult, StreamLine};
use mock::Reply;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

struct Home {
    root: PathBuf,
    url: String,
}

impl Home {
    fn new(name: &str, url: &str) -> Home {
        let root = std::env::temp_dir().join(format!("krowk-perm-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("repo/.claude")).unwrap();
        Home { root: root.canonicalize().unwrap(), url: url.into() }
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

    fn write(&self, rel: &str, body: &str) {
        let p = self.repo().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn host(&self, permissions: Config) -> Host {
        Host::new(HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.repo(),
            registry: Registry::resolve(&InstancesConfig::default(), &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.krowk/credentials.json"),
            trust: krowk_harness::trust::allow_all(),
            publisher: None,
            permissions: Config { home: Some(self.root.join("home")), krowk_dir: Some(self.root.join("home/.krowk")), ..permissions },
            agents: krowk_harness::subagent::AgentsConfig::none(),
        })
    }

    fn context(&self, session: &str) -> Vec<ContextRecord> {
        let dir = log::sessions_dir(&self.env()).unwrap().join(session);
        std::fs::read_to_string(dir.join(log::CONTEXT_FILE)).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn prompt(text: &str, mode: PermissionMode) -> Command {
    Command::Prompt {
        session_id: None,
        text: text.into(),
        model: Some(ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() }),
        permission_mode: mode,
        toolset: None,
        effort: None,
        budget: None,
    }
}

/// A model that calls one tool, then answers once it has the result.
fn one_tool(name: &'static str, input: Value) -> impl Fn(&Value, usize) -> Reply + Send + 'static {
    move |body, _| {
        let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
        if last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result")) {
            Reply::sse(&mock::text_stream("Done."))
        } else {
            Reply::sse(&mock::tool_use("toolu_01Call", name, &input))
        }
    }
}

/// Runs a prompt with no client answering approvals, as `krowk -p` does.
fn run(host: &Host, cmd: Command) -> (Vec<StreamLine>, RunResult) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let r = host.execute(cmd, tx).await.unwrap().unwrap();
        let mut lines = Vec::new();
        while let Ok(l) = rx.try_recv() {
            lines.push(l);
        }
        host.shutdown().await;
        (lines, r)
    })
}

/// The text of the tool result the model was sent back, from the request
/// that carried it.
fn tool_result_sent(m: &mock::Mock) -> String {
    let seen = m.seen.lock().unwrap();
    let last = seen.last().unwrap().body["messages"].as_array().unwrap().last().unwrap().clone();
    let block = last["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_result").cloned().expect("a tool result was sent");
    match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        v => v.to_string(),
    }
}

#[test]
fn r_perm_1_a_repository_whose_settings_deny_bash_rm_blocks_it_in_every_mode() {
    let m = mock::serve(one_tool("bash", json!({"command": "rm -f keep.txt"})));
    let h = Home::new("deny-rm", &m.url);
    h.write(".claude/settings.json", &json!({"permissions": {"deny": ["Bash(rm:*)"]}}).to_string());
    h.write("keep.txt", "still here\n");
    // The person's own settings allow every command; the repository is not
    // even trusted. Its deny rule holds anyway, in every mode.
    let host = h.host(Config { user: Some(json!({"permissions": {"allow": ["Bash"]}})), ..Config::default() });
    for mode in [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan, PermissionMode::BypassPermissions] {
        let (_, r) = run(&host, prompt("clean up", mode));
        assert_eq!(r.result, "Done.", "{mode:?}");
        let sent = tool_result_sent(&m);
        assert!(sent.contains("denied by the rule `Bash(rm:*)`") && sent.contains(".claude/settings.json"), "{mode:?}: {sent}");
        assert!(h.repo().join("keep.txt").exists(), "{mode:?}: rm ran");
    }
}

#[test]
fn r_perm_1_unhinged_runs_what_a_deny_rule_would_have_stopped() {
    let m = mock::serve(one_tool("bash", json!({"command": "rm -f keep.txt"})));
    let h = Home::new("unhinged", &m.url);
    h.write(".claude/settings.json", &json!({"permissions": {"deny": ["Bash(rm:*)"]}}).to_string());
    h.write("keep.txt", "going\n");
    let host = h.host(Config::default());
    let (_, r) = run(&host, prompt("clean up", PermissionMode::Unhinged));
    assert_eq!(r.result, "Done.");
    assert!(!tool_result_sent(&m).contains("denied"), "{}", tool_result_sent(&m));
    assert!(!h.repo().join("keep.txt").exists(), "rm ran: unhinged holds nothing back");
}

#[test]
fn r_perm_1_a_hook_that_blocks_still_blocks_under_unhinged() {
    let m = mock::serve(one_tool("bash", json!({"command": "touch ran.txt"})));
    let h = Home::new("unhinged-hook", &m.url);
    let hooks = json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo 'not during the freeze' >&2; exit 2"}]}]}});
    let host = h.host(Config { user: Some(hooks), ..Config::default() });
    let (_, r) = run(&host, prompt("make a file", PermissionMode::Unhinged));
    assert_eq!(r.result, "Done.");
    assert!(tool_result_sent(&m).contains("not during the freeze"), "{}", tool_result_sent(&m));
    assert!(!h.repo().join("ran.txt").exists(), "a hook is the person's own program, not a rule: its block stands");
}

#[test]
fn r_compat_1_a_pretooluse_hook_exiting_2_blocks_the_tool_and_the_model_reads_why() {
    let m = mock::serve(one_tool("bash", json!({"command": "touch ran.txt"})));
    let h = Home::new("hook", &m.url);
    // The person's own hooks, in Claude Code's format: one that blocks every
    // command, and one that says where the session starts.
    let hooks = json!({"hooks": {
        "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "cat > \"$CLAUDE_PROJECT_DIR/hook-input.json\"; echo 'commands are off during the freeze' >&2; exit 2"}]}],
        "SessionStart": [{"hooks": [{"type": "command", "command": "echo 'the release branch is frozen'"}]}]
    }});
    // A repository's own hook, which nobody trusted: it must not run.
    h.write(".claude/settings.json", &json!({"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "touch repo-hook-ran"}]}]}}).to_string());
    let host = h.host(Config { user: Some(hooks), ..Config::default() });
    let (_, r) = run(&host, prompt("make a file", PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    let sent = tool_result_sent(&m);
    assert!(sent.contains("a PreToolUse hook blocked it: commands are off during the freeze"), "the model reads the hook's reason: {sent}");
    assert!(!h.repo().join("ran.txt").exists(), "the blocked command did not run, even under bypassPermissions");
    assert!(!h.repo().join("repo-hook-ran").exists(), "an untrusted repository's hook never runs");
    let input: Value = serde_json::from_str(&std::fs::read_to_string(h.repo().join("hook-input.json")).unwrap()).unwrap();
    assert_eq!((input["hook_event_name"].as_str(), input["tool_name"].as_str(), input["tool_input"]["command"].as_str()), (Some("PreToolUse"), Some("Bash"), Some("touch ran.txt")), "Claude Code's input shape: {input}");
    assert_eq!(input["permission_mode"], "bypassPermissions");
    // SessionStart's output is context the model read with the prompt.
    let first = m.seen.lock().unwrap()[0].body["messages"].to_string();
    assert!(first.contains("<hook event=\\\"SessionStart\\\">") && first.contains("the release branch is frozen"), "{first}");
}

#[test]
fn r_compat_1_a_skill_is_listed_by_its_description_and_its_body_enters_only_when_used() {
    let m = mock::serve(one_tool("skill", json!({"name": "release-notes"})));
    let h = Home::new("skill", &m.url);
    let body: String = (0..400).map(|i| format!("Step {i}: check the changelog entry against the diff.\n")).collect();
    h.write(".claude/skills/release-notes/SKILL.md", &format!("---\nname: release-notes\ndescription: Write the release notes for a tag\n---\n# Release notes\n{body}"));
    h.write("AGENTS.md", "Run the tests before you say you are done.");
    let host = h.host(Config::default());
    let (_, r) = run(&host, prompt("write the notes", PermissionMode::Default));
    assert_eq!(r.result, "Done.");
    let ctx = h.context(&r.session_id);
    let system = &ctx[0].system;
    assert!(system.contains("- release-notes: Write the release notes for a tag") && !system.contains("Step 0:"), "the description rides in the prompt, the body does not");
    assert!(system.contains("Run the tests before you say you are done.") && system.contains("AGENTS.md"), "the instructions do");
    assert!(ctx[0].tools.iter().any(|t| t.name == "skill"), "the skill tool is offered");
    // By token counts: the body is many times what listing it cost, and it
    // reached the model only in the call that used it.
    let body_tokens = (body.len() as u64).div_ceil(4);
    let baseline = (krowk_harness::native::system_prompt(&h.repo(), &krowk_harness::toolset::Toolset { preset: krowk_harness::toolset::by_name("claude").unwrap(), custom_tools: false }).len() as u64).div_ceil(4);
    let listed = ctx[0].system_tokens - baseline;
    println!("R-COMPAT-1 skill tokens: body {body_tokens}, system prompt {} ({baseline} krowk's own + {listed} instructions and skill list)", ctx[0].system_tokens);
    assert!(listed * 10 < body_tokens, "listing the skill cost {listed} tokens against a {body_tokens}-token body");
    let seen = m.seen.lock().unwrap();
    assert!(!seen[0].body.to_string().contains("Step 0:"), "the first call carries no body");
    drop(seen);
    let sent = tool_result_sent(&m);
    assert!(sent.contains("Step 399: check the changelog") && sent.contains(".claude/skills/release-notes"), "the call that used it carries the body");
}

#[test]
fn r_perm_2_an_asked_call_is_an_approval_request_a_client_answers_over_the_protocol() {
    let m = mock::serve(one_tool("bash", json!({"command": "touch approved.txt"})));
    let h = Home::new("approve", &m.url);
    let host = h.host(Config { approvals: true, ..Config::default() });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    for (decision, runs) in [(ApprovalDecision::Deny, false), (ApprovalDecision::AllowSession, true)] {
        let _ = std::fs::remove_file(h.repo().join("approved.txt"));
        let lines = rt.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
            let turn = host.execute(prompt("make the file", PermissionMode::Default), tx);
            // The client: whatever asks, it answers, as the TUI does (or a
            // phone, later) — by the request's id, with `approve`.
            let client = async {
                let mut lines = Vec::new();
                while let Some(l) = rx.recv().await {
                    if let StreamLine::Live(LiveEvent::ApprovalRequested(req)) = &l {
                        let (itx, _) = tokio::sync::mpsc::channel(1);
                        host.execute(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision }, itx).await.unwrap();
                    }
                    let done = matches!(l, StreamLine::Live(LiveEvent::Result(_)));
                    lines.push(l);
                    if done {
                        break;
                    }
                }
                lines
            };
            let (r, lines) = tokio::join!(turn, client);
            assert_eq!(r.unwrap().unwrap().result, "Done.");
            lines
        });
        let req = lines.iter().find_map(|l| if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = l { Some(r.clone()) } else { None }).expect("an approval.requested frame");
        assert_eq!((req.tool.as_str(), req.summary.as_str()), ("bash", "Bash `touch approved.txt`"));
        assert!(req.reason.contains("runs a command") && req.remember == ["Bash(touch approved.txt)"], "{req:?}");
        assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ApprovalResolved { decision: d, .. }) if *d == decision)), "every client hears it was answered");
        assert_eq!(h.repo().join("approved.txt").exists(), runs, "{decision:?}");
        if !runs {
            assert!(tool_result_sent(&m).contains("the person declined"));
        }
    }
    // A request nobody is waiting on is refused, not ignored.
    let (itx, _) = tokio::sync::mpsc::channel(1);
    let e = rt.block_on(host.execute(Command::Approve { session_id: "s".into(), request_id: "r".into(), decision: ApprovalDecision::Allow }, itx)).unwrap_err();
    assert_eq!(e.code, "no_approval_request");
}

#[test]
fn r_perm_2_headless_never_waits_on_an_ask_and_says_what_would_allow_it() {
    let m = mock::serve(one_tool("bash", json!({"command": "npm test"})));
    let h = Home::new("headless", &m.url);
    let host = h.host(Config::default());
    let t = std::time::Instant::now();
    let (lines, r) = run(&host, prompt("run the tests", PermissionMode::Default));
    assert!(t.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(r.result, "Done.");
    assert!(!lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ApprovalRequested(_)))), "nobody is sent a request nobody can answer");
    let sent = tool_result_sent(&m);
    assert!(sent.contains("nobody is here to give it") && sent.contains("`Bash(npm test)`") && sent.contains("--permission-mode bypassPermissions"), "{sent}");
}

#[test]
fn r_compat_1_a_skill_denied_by_rule_never_enters_the_context_and_hooks_see_skill_calls() {
    let m = mock::serve(one_tool("skill", json!({"name": "deploy"})));
    let h = Home::new("skill-deny", &m.url);
    h.write(".claude/skills/deploy/SKILL.md", "---\nname: deploy\ndescription: Ship it\n---\nSTEP ONE: push to production.");
    h.write(".claude/settings.json", &json!({"permissions": {"deny": ["Skill(deploy)"]}}).to_string());
    let hook = json!({"hooks": {"PreToolUse": [{"matcher": "Skill", "hooks": [{"type": "command", "command": "cat > \"$CLAUDE_PROJECT_DIR/skill-hook.json\""}]}]}});
    let host = h.host(Config { user: Some(hook), ..Config::default() });
    let (_, r) = run(&host, prompt("ship", PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    let sent = tool_result_sent(&m);
    assert!(sent.contains("denied by the rule `Skill(deploy)`"), "{sent}");
    let everything: String = m.seen.lock().unwrap().iter().map(|s| s.raw.clone()).collect();
    assert!(!everything.contains("STEP ONE"), "the body never reached the model");
    let input: Value = serde_json::from_str(&std::fs::read_to_string(h.repo().join("skill-hook.json")).unwrap()).unwrap();
    assert_eq!((input["tool_name"].as_str(), input["tool_input"]["skill"].as_str()), (Some("Skill"), Some("deploy")), "{input}");
}

#[test]
fn r_compat_1_a_hook_that_says_continue_false_stops_the_turn_with_its_reason() {
    let m = mock::serve(one_tool("bash", json!({"command": "ls"})));
    let h = Home::new("hook-stop", &m.url);
    let hooks = json!({"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "echo '{\"continue\": false, \"stopReason\": \"quota reached\", \"systemMessage\": \"see the dashboard\"}'"}]}]}});
    let host = h.host(Config { user: Some(hooks), ..Config::default() });
    let (lines, r) = run(&host, prompt("list", PermissionMode::BypassPermissions));
    let e = r.error.expect("stopped");
    assert_eq!(e.code, "hook_stopped");
    assert!(e.message.contains("quota reached"), "{}", e.message);
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::Notice { text, .. }) if text.contains("see the dashboard"))), "systemMessage is the person's notice");
    assert_eq!(m.seen.lock().unwrap().len(), 1, "no model call after the stop");
}
