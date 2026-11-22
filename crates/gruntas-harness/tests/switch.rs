//! Switching a session's model, instance or engine, through the host,
//! against the fake `claude` and `codex` binaries. No key and no login is
//! real.
//!
//! - Claude Code and Codex round-trip (R-SWITCH-5): a turn on one, a turn
//!   on the other, back on the first — the second reads the first's turn
//!   as a seeded handoff (R-SWITCH-2), and the first is caught up on the
//!   second's on its way back.
//! - A switch that cannot run is refused with its fix, and the session stays
//!   where it was (R-SWITCH-4).
//! - Two accounts of one vendor: the transcript is copied and resumed whole,
//!   or a summary when it cannot be (R-INST-4).
//! - Limits: per instance (R-INST-6), an offer by default (R-INST-7), and
//!   `rollover = "auto"` moving on, telling every client and logging it
//!   (R-INST-8).

#![cfg(unix)]


use gruntas_harness::host::{Host, HostConfig};
use gruntas_harness::instances::{Asked, InstanceKind, InstancesConfig, Registry, Rollover};
use gruntas_harness::log;
use gruntas_harness::protocol::{
    BudgetLimits, Command, HandoffKind, Item, LimitState, LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine, SwitchReason, TurnStatus,
};
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

/// The engines a pair is made of.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Eng {
    Claude,
    Codex,
}

impl Eng {
    fn model(self) -> ModelRef {
        let (i, m) = match self {
            Eng::Claude => ("claude:work", "haiku"),
            Eng::Codex => ("codex:team", "gpt-5.5"),
        };
        ModelRef { instance: i.into(), model: m.into() }
    }
}

struct World {
    root: PathBuf,
    instances: Vec<(String, InstanceKind)>,
    rollover: Option<Rollover>,
    order: Vec<String>,
    trust: Option<gruntas_harness::trust::Gate>,
    /// The repository counts as trusted for the repository's own settings
    /// and for where a router asks a vendor.
    trusted: bool,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl World {
    /// The fake Claude Code and Codex accounts, `claude:work` and
    /// `codex:team`, and nothing else.
    fn new(name: &str) -> World {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        World::backends(name, guard)
    }

    fn backends(name: &str, guard: std::sync::MutexGuard<'static, ()>) -> World {
        let root = std::env::temp_dir().join(format!("krowk-switch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        for (dir, bin) in [("claude", "fake-claude"), ("codex", "fake-codex")] {
            let to = root.join("bin").join(dir);
            // Linked, not copied: a copy is a file open for writing that a test
            // forking beside it can inherit, and running it then fails with
            // "Text file busy" (ETXTBSY) — read as a vendor that could not be checked.
            std::os::unix::fs::symlink(fixture(dir, bin), &to).unwrap();
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let mut w = World { root, instances: Vec::new(), rollover: None, order: Vec::new(), trust: None, trusted: false, _serial: guard };
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

    /// The account a Claude Code instance's fake reports at startup.
    fn account(&mut self, name: &str, email: &str) {
        for (n, kind) in &mut self.instances {
            if let (true, InstanceKind::ClaudeCode { env, .. }) = (n == name, kind) {
                env.insert("FAKE_CLAUDE_ACCOUNT".into(), email.into());
            }
        }
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
            credentials: self.root.join("home/.krowk/credentials.json"),
            trust: self.trust.clone().unwrap_or_else(gruntas_harness::trust::allow_all),
            publisher: None,
            permissions: gruntas_harness::permissions::Config { trusted: self.trusted.then(|| Arc::new(|_: &Path| true) as gruntas_harness::permissions::settings::Trusted), ..Default::default() },
            agents: gruntas_harness::agents::AgentsConfig::none(),
            session: Default::default(),
        })
    }

    fn events(&self, session: &str) -> Vec<LogEvent> {
        log::read_events(&log::sessions_dir(&self.env()).unwrap().join(session).join(log::EVENTS_FILE)).unwrap()
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

    /// What `e` was sent for its latest turn: its last prompt.
    fn seen_by(&self, e: Eng) -> String {
        match e {
            Eng::Claude => self.claude_prompts("claude-work").last().cloned().unwrap_or_default(),
            Eng::Codex => self.codex_prompts("codex-team").last().cloned().unwrap_or_default(),
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
    Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model, permission_mode: PermissionMode::Default, toolset: None, effort: None, rollover: None, budget: None }
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

#[test]
fn r_switch_5_claude_code_and_codex_round_trip() {
    round_trip(Eng::Claude, Eng::Codex);
}

/// A turn on `a`, one on `b`, and back on `a` (R-SWITCH-5): each reads the
/// turns the other ran.
fn round_trip(a: Eng, b: Eng) {
    let w = World::new(&format!("pair-{a:?}-{b:?}"));
    let host = w.host();
    let rt = rt();
    let ask = [format!("call a tool: step one on {a:?}"), format!("call a tool: step two on {b:?}"), format!("step three, back on {a:?}")];
    let (r1, _) = rt.block_on(run(&host, prompt(None, &ask[0], Some(a.model()))));
    assert_eq!(r1.status, TurnStatus::Completed, "{a:?}: {:?}", r1.error);
    let id = r1.session_id.clone();
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), &ask[1], Some(b.model()))));
    assert_eq!(r2.status, TurnStatus::Completed, "{a:?} → {b:?}: {:?}", r2.error);
    let said = answers(&w.events(&id));
    let seen = w.seen_by(b);
    assert!(seen.contains(&ask[0]) && seen.contains(&said[0]), "{b:?} reads {a:?}'s turn — its prompt and its answer {:?}:\n{seen}", said[0]);
    assert!(seen.starts_with("<handoff from krowk>\n") && seen.ends_with(&ask[1]), "into a backend, a handoff then the prompt (R-SWITCH-2): {seen}");
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), &ask[2], Some(a.model()))));
    assert_eq!(r3.status, TurnStatus::Completed, "{b:?} → {a:?}: {:?}", r3.error);
    let said = answers(&w.events(&id));
    let seen = w.seen_by(a);
    assert!(seen.contains(&ask[1]) && seen.contains(&said[1]), "{a:?} reads {b:?}'s turn on the way back — {:?}:\n{seen}", said[1]);
    // Its own thread, caught up on the turn it did not run — not told its
    // own turn again.
    assert!(seen.starts_with("<handoff from krowk>\nWhile you were not running this conversation") && !seen.contains(&ask[0]), "{seen}");
    assert!(handoffs(&w.events(&id)).iter().any(|h| h.0 == HandoffKind::CatchUp));
    assert_eq!(said.len(), 3);
    drop(host);
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
    // A Claude Code account on a key from a variable that is not set.
    w.claude("claude:nokey", "claude-nokey", None, false);
    if let Some((_, InstanceKind::ClaudeCode { api_key_env, .. })) = w.instances.iter_mut().find(|(n, _)| n == "claude:nokey") {
        *api_key_env = Some("UNSET_KEY".into());
    }
    w.claude("claude:nologin", "claude-nologin", None, false);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Claude.model()))));
    let id = r1.session_id.clone();
    let nokey = ModelRef { instance: "claude:nokey".into(), model: "haiku".into() };
    // Asked for as a switch: refused before anything is logged.
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: nokey.clone() }, tx)).unwrap_err();
    assert_eq!(e.code, "not_authenticated");
    assert!(e.message.contains("set UNSET_KEY") && e.message.ends_with("the session stays on claude:work/haiku"), "{}", e.message);
    // Named on a prompt: the same.
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(prompt(Some(&id), "second", Some(nokey)), tx)).unwrap_err();
    assert!(e.message.ends_with("the session stays on claude:work/haiku"), "{}", e.message);
    // A backend instance that is signed out — `claude auth status` says so —
    // is refused by the readiness check the same way, before a Claude
    // process is started for a turn, with the fix and the session kept.
    let nologin = ModelRef { instance: "claude:nologin".into(), model: "haiku".into() };
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: nologin.clone() }, tx)).unwrap_err();
    assert_eq!(e.code, "not_authenticated");
    assert!(e.message.contains("sign in with `krowk connect anthropic --method subscription --name nologin`, which runs Claude's own login") && e.message.ends_with("the session stays on claude:work/haiku"), "{}", e.message);
    let (tx, _rx) = mpsc::channel(16);
    let e = rt.block_on(host.execute(prompt(Some(&id), "third", Some(nologin.clone())), tx)).unwrap_err();
    assert!(e.message.contains("krowk connect anthropic --method subscription --name nologin") && e.message.ends_with("the session stays on claude:work/haiku"), "{}", e.message);
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
    assert!(err.message.contains("krowk connect anthropic --method subscription --name stale") && err.message.ends_with("the session continues on claude:work/haiku"), "{}", err.message);
    let back = w.events(&id).into_iter().find_map(|e| match e.body {
        LogBody::ModelSwitched { from, to, reason, .. } => Some((from, to, reason)),
        _ => None,
    });
    assert_eq!(back, Some((Some(stale), Eng::Claude.model(), SwitchReason::SwitchFailed)));
    // The session continues where it was.
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "fourth", None)));
    assert_eq!((r3.status, r3.model), (TurnStatus::Completed, Eng::Claude.model()));
    // A good switch is logged, and the next prompt runs there.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: Eng::Codex.model() }, tx)).unwrap();
    let (r4, _) = rt.block_on(run(&host, prompt(Some(&id), "fifth", None)));
    assert_eq!((r4.status, r4.model), (TurnStatus::Completed, Eng::Codex.model()));
    // Without a session it is only a check.
    let (tx, _rx) = mpsc::channel(16);
    assert!(rt.block_on(host.execute(Command::SwitchModel { session_id: None, model: Eng::Codex.model() }, tx)).is_ok());
}

#[test]
fn r_switch_4_a_switch_does_not_escape_the_budget_or_loosen_the_mode() {
    let w = World::new("budget");
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "spend some tokens", Some(Eng::Claude.model()))));
    let id = r1.session_id.clone();
    let spent = r1.usage.output_tokens;
    assert!(spent > 0);
    // On another vendor the tokens already spent still count.
    let mut cmd = prompt(Some(&id), "and more elsewhere", Some(Eng::Codex.model()));
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
    let rt = rt();
    let mut w = World::new("offer-claude");
    w.claude("claude:work", "claude-work", Some("limited.jsonl"), true);
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    let host = w.host();
    let (r1, lines) = rt.block_on(run(&host, prompt(None, "do the thing", Some(Eng::Claude.model()))));
    assert_eq!(r1.status, TurnStatus::Failed);
    let offer = r1.switch_offer.clone().expect("an offer, not a silent move");
    assert_eq!((offer.from.instance.as_str(), offer.to.instance.as_str(), offer.to.model.as_str()), ("claude:work", "claude:personal", "haiku"));
    let err = r1.error.unwrap();
    // Detected with when it lifts.
    assert_eq!((err.code.as_str(), err.resets_at_ms), ("rate_limited", Some(1_790_431_200_000)));
    assert!(err.message.contains(&format!("continue on claude:personal/haiku with `krowk -p --resume {} --model claude:personal/haiku`", r1.session_id)), "{}", err.message);
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::Limits { instance, limit, .. }) if instance == "claude:work" && limit.status == LimitState::Limited)));
    assert!(w.events(&r1.session_id).iter().all(|e| !matches!(e.body, LogBody::ModelSwitched { .. })), "offered, never taken by itself");
    // The yes: switch, and the prompt again.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(r1.session_id.clone()), model: offer.to.clone() }, tx)).unwrap();
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "do the thing", None)));
    assert_eq!((r2.status, r2.model.instance.as_str()), (TurnStatus::Completed, "claude:personal"), "{:?}", r2.error);
}

/// A prompt that sets the session's `rollover`.
fn rolled(mut cmd: Command, to: Rollover) -> Command {
    if let Command::Prompt { rollover, .. } = &mut cmd {
        *rollover = Some(to);
    }
    cmd
}

/// R-ROLL-5, R-ROLL-6, D11: `off` fails a limit with no offer and no hint
/// to continue elsewhere, whether the session, a trusted repository or the
/// global config says it; an untrusted repository's value is ignored; a
/// resumed session keeps the value it recorded until a new one replaces it.
#[test]
fn r_roll_5_rollover_is_resolved_per_session_repo_and_global_and_off_offers_nothing() {
    let rt = rt();
    let hint = |m: &str| m.contains("continue on") || m.contains("--model");
    let ask = |w: &World, asked: Option<Rollover>| {
        let host = w.host();
        let cmd = prompt(None, "do the thing", Some(Eng::Claude.model()));
        let (r, _) = rt.block_on(run(&host, asked.map_or(cmd.clone(), |a| rolled(cmd, a))));
        (host, r)
    };

    // Limited on every turn: one process plays the resumed session's turns.
    let mut w = World::new("roll-off");
    let limited = std::fs::read_to_string(fixture("claude", "limited.jsonl")).unwrap();
    let always = w.root.join("always_limited.jsonl");
    std::fs::write(&always, [limited.trim_end(); 4].join("\n---\n")).unwrap();
    w.claude("claude:work", "claude-work", None, true);
    if let Some((_, InstanceKind::ClaudeCode { env, .. })) = w.instances.iter_mut().find(|(n, _)| n == "claude:work") {
        env.insert("FAKE_CLAUDE_SCENARIO".into(), always.display().to_string());
    }
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    // Offered by default, with the hint.
    let (_, r) = ask(&w, None);
    assert!(r.switch_offer.is_some() && hint(&r.error.unwrap().message));
    // The session's off: nothing offered, nothing hinted, and logged.
    let (host, r) = ask(&w, Some(Rollover::Off));
    let (err, id) = (r.error.clone().unwrap(), r.session_id.clone());
    assert!(r.switch_offer.is_none() && !hint(&err.message) && err.code == "rate_limited", "{err:?}");
    assert!(w.events(&id).iter().any(|e| matches!(e.body, LogBody::TurnStarted { rollover: Some(Rollover::Off), .. })));
    // Resumed with no value: the recorded one stands. With one: it replaces it.
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "again", None)));
    assert!(r2.switch_offer.is_none() && !hint(&r2.error.unwrap().message));
    let (r3, _) = rt.block_on(run(&host, rolled(prompt(Some(&id), "again", None), Rollover::Offer)));
    assert!(r3.switch_offer.is_some(), "{:?}", r3.error);
    let (r4, _) = rt.block_on(run(&host, prompt(Some(&id), "again", None)));
    assert!(r4.switch_offer.is_some(), "the new value is the one recorded");

    // The global config's off, and a repository's own: believed only when trusted.
    w.rollover = Some(Rollover::Off);
    assert!(ask(&w, None).1.switch_offer.is_none(), "global");
    std::fs::create_dir_all(w.root.join("repo/.krowk")).unwrap();
    std::fs::write(w.root.join("repo/.krowk/config.json"), r#"{"rollover": "offer"}"#).unwrap();
    assert!(ask(&w, None).1.switch_offer.is_none(), "an untrusted repository's value is ignored");
    w.trusted = true;
    assert!(ask(&w, None).1.switch_offer.is_some(), "a trusted repository overrides the global one");
    assert!(ask(&w, Some(Rollover::Off)).1.switch_offer.is_none(), "the session overrides both");
}

#[test]
fn r_roll_2_every_limits_report_is_saved_under_the_login_of_the_instance_that_ran() {
    let mut w = World::new("usage");
    w.claude("claude:work", "claude-work", Some("limited.jsonl"), true);
    w.claude("claude:same", "claude-same", Some("switch.jsonl"), true);
    w.claude("claude:other", "claude-other", Some("switch.jsonl"), true);
    w.account("claude:work", "a@example.com");
    w.account("claude:same", "a@example.com");
    w.account("claude:other", "b@example.com");
    let host = w.host();
    let rt = rt();
    let on = |instance: &str| Some(ModelRef { instance: instance.into(), model: "sonnet".into() });
    let (r, _) = rt.block_on(run(&host, prompt(None, "go", on("claude:work"))));
    assert_eq!(r.error.map(|e| e.code), Some("rate_limited".into()));
    // Read as of a moment before the recorded reset (1790431200).
    let before_reset = 1_790_431_000_000;
    let reading = |login: &str| gruntas_harness::usage::Usage::in_home(&w.root.join("home/.krowk")).read().readings(login, before_reset).into_iter().map(|r| (r.window, r.status, r.used_percent)).collect::<Vec<_>>();
    // The limit the turn hit is under the login of the fake account.
    assert_eq!(reading("a@example.com"), [("five_hour".to_string(), LimitState::Limited, Some(100.0))]);
    assert!(reading("b@example.com").is_empty());
    // A second instance on that login shares its record, the latest report
    // winning; a third on another login has a record of its own.
    rt.block_on(run(&host, prompt(None, "go", on("claude:same"))));
    rt.block_on(run(&host, prompt(None, "go", on("claude:other"))));
    let data = gruntas_harness::usage::Usage::in_home(&w.root.join("home/.krowk")).read();
    assert_eq!((data.login_of("claude:work"), data.login_of("claude:same"), data.login_of("claude:other")), ("a@example.com", "a@example.com", "b@example.com"));
    assert_eq!(reading("a@example.com"), [("five_hour".to_string(), LimitState::Warning, Some(82.0))]);
    assert_eq!(reading("b@example.com"), [("five_hour".to_string(), LimitState::Warning, Some(82.0))]);
    // The models Claude Code listed at startup are kept per instance.
    assert_eq!(data.instance("claude:other").unwrap().models, ["sonnet", "haiku"]);
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
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::Notice { text, .. }) if text.contains("claude:work/haiku is limited until") && text.contains("continuing on claude:personal/haiku") && !text.contains("another model"))), "the notice names both, whole: {lines:?}");
    let events = w.events(&r.session_id);
    let switched = events.iter().find_map(|e| match &e.body {
        LogBody::ModelSwitched { from, to, reason, detail, .. } => Some((from.clone(), to.clone(), *reason, detail.clone())),
        _ => None,
    });
    let (from, to, reason, detail) = switched.expect("the rollover is on the record");
    assert_eq!((from.map(|m| m.instance), to.instance.as_str(), reason), (Some("claude:work".into()), "claude:personal", SwitchReason::RateLimited));
    assert!(detail.unwrap().contains("rollover is auto"));
    let now = gruntas_store::now_ms();
    assert!(gruntas_harness::usage::Usage::in_home(&w.root.join("home/.krowk")).read().moved_recently("claude:personal", now), "a hard-limit auto move marks its target too");
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

/// The limits each engine's instance reports, from a turn's lines.
fn limits_of(lines: &[StreamLine]) -> Vec<(String, LimitState, Option<f64>)> {
    lines.iter().filter_map(|l| match l {
        StreamLine::Live(LiveEvent::Limits { instance, limit, .. }) => Some((instance.clone(), limit.status, limit.used_percent.map(f64::round))),
        _ => None,
    }).collect()
}

#[test]
fn r_inst_6_each_instance_reports_how_near_its_limit_it_is() {
    let w = World::new("limits");
    let host = w.host();
    let rt = rt();
    let (r, lines) = rt.block_on(run(&host, prompt(None, "on claude code", Some(Eng::Claude.model()))));
    assert_eq!(limits_of(&lines), [("claude:work".to_string(), LimitState::Warning, Some(82.0))]);
    let (_, lines) = rt.block_on(run(&host, prompt(Some(&r.session_id), "on codex", Some(Eng::Codex.model()))));
    assert_eq!(limits_of(&lines), [("codex:team".to_string(), LimitState::Allowed, Some(35.0))]);
}

#[test]
fn r_roll_1_claude_code_reports_every_window_it_lists_below_the_limit() {
    let mut w = World::new("windows");
    w.claude("claude:work", "claude-work", Some("windows.jsonl"), true);
    w.claude("claude:full", "claude-full", Some("windows_rejected.jsonl"), true);
    let host = w.host();
    let rt = rt();
    type Seen = (Option<String>, LimitState, Option<f64>, Option<i64>);
    let limits = |lines: &[StreamLine]| -> Vec<Seen> {
        lines.iter().filter_map(|l| match l {
            StreamLine::Live(LiveEvent::Limits { limit, .. }) => Some((limit.window.clone(), limit.status, limit.used_percent.map(f64::round), limit.resets_at_ms)),
            _ => None,
        }).collect()
    };
    let (_, lines) = rt.block_on(run(&host, prompt(None, "hello", Some(ModelRef { instance: "claude:work".into(), model: "haiku".into() }))));
    // Fullest last; a bare `allowed` event adds a report with no utilisation, as before.
    assert_eq!(limits(&lines), [
        (Some("seven_day".into()), LimitState::Allowed, Some(57.0), Some(1_791_846_000_000)),
        (Some("five_hour".into()), LimitState::Allowed, Some(69.0), Some(1_791_628_200_000)),
        (None, LimitState::Allowed, None, None),
    ]);
    // A rejected turn names the rejected window, not the fullest-looking one.
    let (r, lines) = rt.block_on(run(&host, prompt(None, "hello", Some(ModelRef { instance: "claude:full".into(), model: "haiku".into() }))));
    assert_eq!(limits(&lines).iter().map(|l| (l.0.clone(), l.1, l.2)).collect::<Vec<_>>(), [
        (Some("five_hour".into()), LimitState::Warning, Some(90.0)),
        (Some("seven_day".into()), LimitState::Limited, Some(100.0)),
    ]);
    let e = r.error.unwrap();
    assert_eq!((e.code.as_str(), e.resets_at_ms), ("rate_limited", Some(1_791_846_000_000)));
}

#[test]
fn r_switch_4_a_switch_the_picker_made_goes_back_when_its_first_turn_cannot_run() {
    let mut w = World::new("picker-back");
    // Signed in as far as `claude auth status` says, stale once it runs.
    w.claude("claude:nologin", "claude-nologin", Some("stale_login.jsonl"), true);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Claude.model()))));
    let id = r1.session_id.clone();
    let nologin = ModelRef { instance: "claude:nologin".into(), model: "haiku".into() };
    // The picker, an offer's yes: switchModel, then a prompt naming no model.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: nologin.clone() }, tx)).unwrap();
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&id), "second", None)));
    assert_eq!((r2.status, r2.model.clone()), (TurnStatus::Failed, nologin.clone()));
    assert!(r2.error.unwrap().message.ends_with("the session continues on claude:work/haiku"));
    let back: Vec<(SwitchReason, ModelRef)> = w.events(&id).into_iter().filter_map(|e| match e.body {
        LogBody::ModelSwitched { reason, to, .. } => Some((reason, to)),
        _ => None,
    }).collect();
    assert_eq!(back, [(SwitchReason::Requested, nologin), (SwitchReason::SwitchFailed, Eng::Claude.model())]);
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "third", None)));
    assert_eq!((r3.status, r3.model), (TurnStatus::Completed, Eng::Claude.model()));
}

#[test]
fn r_roll_8_auto_without_an_order_moves_to_the_most_saved_headroom_on_the_same_model() {
    let mut w = World::new("auto-ranked");
    for (name, scenario) in [("claude:work", "limited.jsonl"), ("claude:twin", "switch.jsonl"), ("claude:low", "switch.jsonl"), ("claude:most", "switch.jsonl"), ("claude:nolist", "switch.jsonl"), ("claude:unseen", "switch.jsonl")] {
        w.claude(name, &name.replace(':', "-"), Some(scenario), true);
    }
    w.rollover = Some(Rollover::Auto);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let usage = gruntas_harness::usage::Usage::in_home(&w.root.join("home/.krowk"));
    let models = vec!["haiku".to_string()];
    let seed = |instance: &str, login: &str, used: f64, models: &[String]| {
        usage.record_instance(instance, login, models, now).unwrap();
        let limit = gruntas_harness::protocol::LimitStatus { status: LimitState::Allowed, window: Some("five_hour".into()), used_percent: Some(used), resets_at_ms: Some(now + 3_600_000) };
        usage.record_limit(login, &limit, now).unwrap();
    };
    // The twin shares the limited instance's login, with the most room of all: never a candidate.
    seed("claude:work", "w", 100.0, &models);
    seed("claude:twin", "w", 0.0, &models);
    seed("claude:low", "low", 80.0, &models);
    seed("claude:most", "most", 30.0, &models);
    // No room to speak of, and a model list without the model: skipped either way.
    seed("claude:nolist", "nolist", 0.0, &["opus".to_string()]);
    let host = w.host();
    let (r, _) = rt().block_on(run(&host, prompt(None, "keep going", Some(ModelRef { instance: "claude:work".into(), model: "haiku".into() }))));
    assert_eq!((r.status, r.model.clone()), (TurnStatus::Completed, ModelRef { instance: "claude:most".into(), model: "haiku".into() }), "{:?}", r.error);
    let detail = w.events(&r.session_id).into_iter().find_map(|e| match e.body {
        LogBody::ModelSwitched { detail, .. } => detail,
        _ => None,
    });
    assert!(detail.as_deref().is_some_and(|d| d.contains("continuing on claude:most/haiku (70% left as of ") && d.contains(", resets ")), "{detail:?}");
}

/// R-ROLL-11, D9, D19: a turn that leaves an `auto` session's login with too
/// little for two more such turns moves the session, as logged, to a
/// candidate with room, which is marked as moved to; `offer` and a
/// candidate with too little more room leave it where it is.
#[test]
fn r_roll_11_a_turn_that_leaves_too_little_moves_an_auto_session_between_turns() {
    let mut w = World::new("early");
    for (name, scenario) in [("claude:work", "low.jsonl"), ("claude:best", "switch.jsonl")] {
        w.claude(name, &name.replace(':', "-"), Some(scenario), true);
    }
    w.rollover = Some(Rollover::Auto);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let usage = gruntas_harness::usage::Usage::in_home(&w.root.join("home/.krowk"));
    let models = vec!["haiku".to_string()];
    // Seen now, each time: the report seen latest wins, and a turn's own is later than an earlier seed.
    let seed = |instance: &str, used: f64| {
        let seen = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        usage.record_instance(instance, instance, &models, seen).unwrap();
        let limit = gruntas_harness::protocol::LimitStatus { status: LimitState::Allowed, window: Some("five_hour".into()), used_percent: Some(used), resets_at_ms: Some(now + 3_600_000) };
        usage.record_limit(instance, &limit, seen).unwrap();
    };
    let work = ModelRef { instance: "claude:work".into(), model: "haiku".into() };
    let run_work = |host: &Host| rt().block_on(run(host, prompt(None, "keep going", Some(work.clone()))));
    let moves = |w: &World, id: &str| -> Vec<(ModelRef, SwitchReason, Option<String>)> {
        w.events(id).into_iter().filter_map(|e| match e.body {
            LogBody::ModelSwitched { to, reason, detail, .. } => Some((to, reason, detail)),
            _ => None,
        }).collect()
    };
    // Five hours at 90%, the turn takes it to 96%: 4 left is less than 12.
    seed("claude:work", 90.0);
    seed("claude:best", 59.0);
    let host = w.host();
    let (r, lines) = run_work(&host);
    assert_eq!((r.status, r.model.instance.as_str()), (TurnStatus::Completed, "claude:work"));
    let moved = moves(&w, &r.session_id);
    assert_eq!(moved.len(), 1, "{moved:?}");
    let detail = moved[0].2.clone().unwrap();
    assert_eq!((moved[0].0.instance.as_str(), moved[0].1), ("claude:best", SwitchReason::RateLimited));
    assert!(detail.starts_with("claude:work/haiku is 96% used; rollover is auto, continuing on claude:best/haiku (41% left as of "), "{detail}");
    assert!(lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::Notice { text, .. }) if text.contains("is 96% used"))));
    assert!(usage.read().moved_recently("claude:best", now + 1_000), "the target carries the mark");
    // The next prompt runs there, the transcript resumed.
    let (r2, _) = rt().block_on(run(&host, prompt(Some(&r.session_id), "and more", None)));
    assert_eq!((r2.status, r2.model.instance.as_str()), (TurnStatus::Completed, "claude:best"));
    assert!(w.fake_log("claude-best").contains("resume-lines"));
    // Offer: nothing moves by itself; the successful result offers it, marked early.
    drop(host);
    seed("claude:work", 90.0);
    w.rollover = Some(Rollover::Offer);
    let (r, _) = run_work(&w.host());
    assert!(moves(&w, &r.session_id).is_empty());
    assert_eq!(r.status, TurnStatus::Completed);
    let offer = r.switch_offer.expect("the early offer");
    assert!(offer.early && offer.to.instance == "claude:best" && offer.resets_at_ms.is_some(), "{offer:?}");
    assert!(offer.room.is_some_and(|room| room.contains("% left as of ")));
    // An older host's offer has no `early`: a limit's.
    let old: gruntas_harness::protocol::SwitchOffer = serde_json::from_value(json!({"from": {"instance": "a", "model": "m"}, "to": {"instance": "b", "model": "m"}})).unwrap();
    assert!(!old.early);
    // Auto, but the only candidate has under 10 to spare: stays.
    seed("claude:work", 90.0);
    seed("claude:best", 95.0);
    w.rollover = Some(Rollover::Auto);
    let (r, _) = run_work(&w.host());
    assert!(moves(&w, &r.session_id).is_empty() && r.switch_offer.is_none());
}

/// Saves `instance`'s usage as seen at `at`: five hours at `used`, its
/// binding window resetting an hour on.
fn seen_at(w: &World, instance: &str, used: f64, at: i64) {
    let usage = gruntas_harness::usage::Usage::in_home(&w.root.join("home/.krowk"));
    usage.record_instance(instance, instance, &["haiku".to_string()], at).unwrap();
    let limit = gruntas_harness::protocol::LimitStatus { status: LimitState::Allowed, window: Some("five_hour".into()), used_percent: Some(used), resets_at_ms: Some(at + 3_600_000) };
    usage.record_limit(instance, &limit, at).unwrap();
}

/// R-ROLL-13, D14: an early offer that is not taken is declined by the
/// session's next prompt on the same account, so the turn after it, still
/// low, offers nothing.
#[test]
fn r_roll_13_a_declined_early_offer_is_not_made_again_in_the_session() {
    let rt = rt();
    let mut w = World::new("declined");
    w.claude("claude:work", "claude-work", Some("plain_turns.jsonl"), true);
    w.claude("claude:best", "claude-best", None, true);
    w.rollover = Some(Rollover::Offer);
    let now = gruntas_store::now_ms();
    seen_at(&w, "claude:work", 99.0, now);
    seen_at(&w, "claude:best", 59.0, now);
    let work = ModelRef { instance: "claude:work".into(), model: "haiku".into() };
    let host = w.host();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "keep going", Some(work))));
    let offer = r1.switch_offer.expect("the early offer");
    assert!(offer.early && offer.to.instance == "claude:best", "{offer:?}");
    // Not taken: the next prompt runs on the same account, and ends still low.
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "and more", None)));
    assert_eq!((r2.status, r2.model.instance.as_str()), (TurnStatus::Completed, "claude:work"), "{:?}", r2.error);
    assert!(r2.switch_offer.is_none(), "{:?}", r2.switch_offer);
    assert!(w.events(&r2.session_id).iter().all(|e| !matches!(e.body, LogBody::ModelSwitched { .. })));
}

/// R-ROLL-13, D14: a declined offer comes again once its binding window has
/// reset, by the clock the host reads.
#[test]
fn r_roll_13_the_early_offer_comes_again_once_the_binding_window_resets() {
    let rt = rt();
    let mut w = World::new("declined-reset");
    w.claude("claude:work", "claude-work", Some("plain_turns.jsonl"), true);
    w.claude("claude:best", "claude-best", None, true);
    w.rollover = Some(Rollover::Offer);
    let base = gruntas_store::now_ms();
    let at = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(base));
    let clock = at.clone();
    let host = w.host().with_clock(move || clock.load(std::sync::atomic::Ordering::SeqCst));
    seen_at(&w, "claude:work", 99.0, base);
    seen_at(&w, "claude:best", 59.0, base);
    let work = ModelRef { instance: "claude:work".into(), model: "haiku".into() };
    let (r1, _) = rt.block_on(run(&host, prompt(None, "keep going", Some(work))));
    assert!(r1.switch_offer.is_some_and(|o| o.early));
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "and more", None)));
    assert!(r2.switch_offer.is_none());
    // The window resets an hour on; the account is low again, freshly read.
    let reset = base + 3_600_000;
    at.store(reset, std::sync::atomic::Ordering::SeqCst);
    seen_at(&w, "claude:work", 99.0, reset);
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "once more", None)));
    assert!(r3.switch_offer.as_ref().is_some_and(|o| o.early && o.to.instance == "claude:best"), "{:?}", r3.switch_offer);
}

/// R-ROLL-13: a hard-limit offer still comes after an early offer was
/// declined in the session, and is not marked early.
#[test]
fn r_roll_13_a_hard_limit_still_offers_after_a_declined_early_offer() {
    let rt = rt();
    let mut w = World::new("declined-limit");
    w.claude("claude:work", "claude-work", Some("declined.jsonl"), true);
    w.claude("claude:best", "claude-best", None, true);
    w.rollover = Some(Rollover::Offer);
    let now = gruntas_store::now_ms();
    seen_at(&w, "claude:work", 99.0, now);
    seen_at(&w, "claude:best", 59.0, now);
    let work = ModelRef { instance: "claude:work".into(), model: "haiku".into() };
    let host = w.host();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "keep going", Some(work))));
    assert!(r1.switch_offer.is_some_and(|o| o.early));
    let (r2, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "and more", None)));
    assert!(r2.switch_offer.is_none());
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&r1.session_id), "and more", None)));
    assert_eq!(r3.status, TurnStatus::Failed);
    let offer = r3.switch_offer.expect("the hard-limit offer");
    assert!(!offer.early && offer.to.instance == "claude:best", "{offer:?}");
}

#[test]
fn r_inst_8_a_rollover_is_logged_only_once_its_turn_can_start() {
    let mut w = World::new("rollover-preflight");
    w.claude("claude:work", "claude-work", Some("limited.jsonl"), true);
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    w.claude("claude:third", "claude-third", Some("switch.jsonl"), true);
    w.rollover = Some(Rollover::Auto);
    w.order = vec!["claude:work".into(), "claude:personal".into(), "claude:third".into()];
    // The repository is trusted for the limited turn and the candidate's
    // check, and refused when the candidate's own turn asks: a check that
    // passed is not a turn that starts.
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    w.trust = Some(Arc::new(move |root: &Path| {
        if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 { Err(gruntas_harness::trust::untrusted(root, "refused for the test")) } else { Ok(()) }
    }));
    let host = w.host();
    let rt = rt();
    let (r, _) = rt.block_on(run(&host, prompt(None, "keep going", Some(Eng::Claude.model()))));
    // claude:personal could not start, so the session went on to the next, on the same model.
    let third = ModelRef { instance: "claude:third".into(), model: Eng::Claude.model().model };
    assert_eq!((r.status, r.model.clone()), (TurnStatus::Completed, third.clone()), "{:?}", r.error);
    let switched: Vec<(ModelRef, SwitchReason, String)> = w.events(&r.session_id).into_iter().filter_map(|e| match e.body {
        LogBody::ModelSwitched { to, reason, detail, .. } => Some((to, reason, detail.unwrap_or_default())),
        _ => None,
    }).collect();
    assert_eq!(switched.len(), 1, "one move on the record, to where the turn ran: {switched:?}");
    assert_eq!((switched[0].0.clone(), switched[0].1), (third, SwitchReason::RateLimited));
    assert!(switched[0].2.contains("claude:work/haiku is limited") && switched[0].2.contains("continuing on claude:third/haiku"), "{}", switched[0].2);
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
    w.claude("claude:personal", "claude-personal", Some("switch.jsonl"), true);
    let personal = ModelRef { instance: "claude:personal".into(), model: "haiku".into() };
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Codex.model()))));
    let id = r1.session_id.clone();
    let (r2, switched) = rt.block_on(async {
        let turn = run(&host, prompt(Some(&id), "slowly", Some(Eng::Claude.model())));
        let switch = async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let (tx, _rx) = mpsc::channel(16);
            host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: personal.clone() }, tx).await
        };
        tokio::join!(turn, switch)
    });
    assert!(switched.is_ok(), "{switched:?}");
    assert_eq!(r2.0.status, TurnStatus::Completed);
    // Right after the turn, from the same host: no lock in the way.
    let (tx, _rx) = mpsc::channel(16);
    rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model: Eng::Codex.model() }, tx)).unwrap();
    let to: Vec<ModelRef> = w.events(&id).into_iter().filter_map(|e| match e.body {
        LogBody::ModelSwitched { to, .. } => Some(to),
        _ => None,
    }).collect();
    assert_eq!(to, [personal, Eng::Codex.model()]);
    let (r3, _) = rt.block_on(run(&host, prompt(Some(&id), "next", None)));
    assert_eq!(r3.model, Eng::Codex.model());
}

#[test]
fn readiness_a_vendors_signed_in_is_believed_for_a_minute_and_a_signed_out_asked_every_time() {
    let mut w = World::new("readiness-cache");
    w.claude("claude:in", "claude-in", None, true);
    let out_dir = w.claude("claude:out", "claude-out", None, false);
    let host = w.host();
    let rt = rt();
    let (r1, _) = rt.block_on(run(&host, prompt(None, "first", Some(Eng::Codex.model()))));
    let id = r1.session_id.clone();
    let asked = |dir: &str| w.fake_log(dir).lines().filter(|l| *l == "argv auth status --json").count();
    let switch_to = |model: ModelRef| {
        let (tx, _rx) = mpsc::channel(16);
        rt.block_on(host.execute(Command::SwitchModel { session_id: Some(id.clone()), model }, tx))
    };
    let switch = |instance: &str| switch_to(ModelRef { instance: instance.into(), model: "haiku".into() });
    // Signed in: asked once, then believed — a long-lived host (an interactive client)
    // does not spawn Claude Code's status for every switch.
    switch("claude:in").unwrap();
    switch_to(Eng::Codex.model()).unwrap();
    switch("claude:in").unwrap();
    assert_eq!(asked("claude-in"), 1);
    // Signed out: asked every time, so signing in elsewhere and trying again
    // works at once.
    assert_eq!(switch("claude:out").unwrap_err().code, "not_authenticated");
    std::fs::write(out_dir.join("fake-login"), "").unwrap();
    switch("claude:out").unwrap();
    assert_eq!(asked("claude-out"), 2);
}

/// The cwd of every `claude auth status` a fake Claude Code logged.
fn status_cwds(log: &str) -> Vec<PathBuf> {
    let lines: Vec<&str> = log.lines().collect();
    lines.iter().enumerate().filter(|(_, l)| **l == "argv auth status --json").filter_map(|(i, _)| lines[i..].iter().find_map(|l| l.strip_prefix("cwd ")).map(PathBuf::from)).collect()
}

#[test]
fn a_bare_model_goes_to_the_one_ready_instance() {
    let mut w = World::new("routing");
    let rt = rt();
    // Only one instance connected: no model at all runs on its default,
    // asked of the vendor in krowk's own directory for routing and in the
    // repository, once trusted, for the turn.
    w.instances.retain(|(n, _)| n == "claude:work");
    let host = w.host();
    let (r, _) = rt.block_on(run(&host, prompt(None, "hello", None)));
    assert_eq!((r.status, r.model.to_string()), (TurnStatus::Completed, "claude:work/claude-opus-5-5".to_string()), "{:?}", r.error);
    let data = log::sessions_dir(&w.env()).unwrap().parent().unwrap().to_path_buf();
    assert_eq!(status_cwds(&w.fake_log("claude-work")), [data.join("readiness"), w.root.join("repo")], "{}", w.fake_log("claude-work"));
    // Its repository trusted already, the router asks there, and the
    // turn's own check is the cache's: one check in all.
    w.trusted = true;
    let _ = std::fs::remove_file(w.root.join("claude-work.log"));
    let host = w.host();
    let (r, _) = rt.block_on(run(&host, prompt(None, "hello again", None)));
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    assert!(status_cwds(&w.fake_log("claude-work")).iter().all(|d| *d == w.root.join("repo")), "{}", w.fake_log("claude-work"));
}

/// A bare model stays on the session's account; with no session and two
/// accounts that could run it — one whose check could not answer among
/// them — it is refused, each named, never guessed.
#[test]
fn a_bare_model_stays_on_the_sessions_account_and_two_accounts_are_refused_as_ambiguous() {
    let mut w = World::new("routing-accounts");
    let rt = rt();
    w.claude("claude:personal", "claude-personal", None, true);
    let host = w.host();
    let bare = |m: &str| Asked::Bare(m.into());
    let repo = w.root.join("repo");
    let on_personal = ModelRef { instance: "claude:personal".into(), model: "sonnet".into() };
    assert_eq!(rt.block_on(host.route_model(Some(&bare("haiku")), Some(&on_personal), &repo)).unwrap(), ModelRef { instance: "claude:personal".into(), model: "haiku".into() });
    let e = rt.block_on(host.route_model(Some(&bare("haiku")), None, &repo)).unwrap_err();
    assert_eq!(e.code, "ambiguous_model");
    assert!(e.message.contains("claude:work, Claude subscription: --model claude:work/haiku") && e.message.contains("claude:personal, Claude subscription: --model claude:personal/haiku"), "{}", e.message);
    // A subscription whose check could not answer is a candidate too.
    if let Some((_, InstanceKind::ClaudeCode { binary, .. })) = w.instances.iter_mut().find(|(n, _)| n == "claude:personal") {
        let broken = w.root.join("bin/claude-broken");
        std::fs::write(&broken, "#!/bin/sh\necho 'not json'\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
        *binary = Some(broken.display().to_string());
    }
    let host = w.host();
    let e = rt.block_on(host.route_model(Some(&bare("haiku")), None, &repo)).unwrap_err();
    assert!(e.code == "ambiguous_model" && e.message.contains("claude:personal, Claude subscription: --model claude:personal/haiku (could not be checked"), "{}", e.message);
}

