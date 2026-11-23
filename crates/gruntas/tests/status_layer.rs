//! The status layer's acceptance, slice by slice (the "Status layer" train).
//! SL01 landed the contract (`gruntas_client::status`, the registry calls
//! and the dev registry's stand-ins, tested in gruntas-client's and
//! gruntas-api's `status` tests); each later ticket un-ignores its own tests
//! here, fills in what is `todo!`, and goes green on top of them.

#![cfg(all(feature = "harness", unix))]

use gruntas_client::status::{Board, Field, Health, Message, Provenance, Task, TaskMetadata, TaskState, TaskStatus};

/// A Task as a daemon would publish it, for the views' fixtures.
fn task(id: &str, state: TaskState, project: &str, parent: Option<&str>, summary: &str, last_activity: &str) -> Task {
    let mut t = Task { id: id.into(), status: TaskStatus { state, message: Some(Message::text(format!("{id}-m"), summary)), timestamp: last_activity.into() }, history: Vec::new(), metadata: TaskMetadata::default() };
    let g = &mut t.metadata.gruntas;
    g.health = Some(Field::harness(if state == TaskState::Canceled { Health::NotServing } else { Health::Serving }));
    g.project = Some(Field::harness(project.into()));
    g.parent_session_id = parent.map(|p| Field::harness(p.into()));
    g.last_activity = Some(Field::harness(last_activity.into()));
    t
}

/// Two machines' boards: the laptop runs a parent and its child on one
/// project and a session on another; the server one session, canceled.
fn two_machines() -> Vec<Board> {
    let laptop = Board {
        device: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        published_at: "2026-11-23T12:00:00Z".into(),
        sessions: vec!["p1".into(), "c1".into(), "o1".into()],
        tasks: vec![
            task("p1", TaskState::Working, "github.com/acme/app", None, "splitting the train", "2026-11-23T11:59:00Z"),
            task("c1", TaskState::InputRequired, "github.com/acme/app", Some("p1"), "waiting on a permission", "2026-11-23T11:50:00Z"),
            task("o1", TaskState::Completed, "github.com/acme/site", None, "shipped", "2026-11-23T11:00:00Z"),
        ],
    };
    let server = Board {
        device: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        published_at: "2026-11-23T11:59:30Z".into(),
        sessions: vec!["s1".into()],
        tasks: vec![task("s1", TaskState::Canceled, "github.com/acme/app", None, "fixing CI", "2026-11-23T11:30:00Z")],
    };
    vec![laptop, server]
}

// ---- SL03: report status from agents ----

/// A test's own home and runtime directory, under /tmp so the socket path
/// fits `sun_path`, with a daemon in this process (as KT03's), and
/// `gruntas hook` and `gruntas-mcp` run as a harness runs them: this test
/// process is the harness.
struct Machine(std::path::PathBuf);

impl Machine {
    fn up(name: &str) -> Machine {
        use gruntas_harness::daemon::server;
        let root = std::path::PathBuf::from("/tmp").join(format!("gruntas-sl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let m = Machine(root);
        let socket = m.socket();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let daemon = std::thread::spawn(move || {
            let factory: server::Factory = Box::new(|_, _| Err(gruntas_harness::engine::EngineError::new("no_sessions", "this test runs none")));
            server::run(server::Options { socket, idle: None, gruntas_version: "test".into(), ..Default::default() }, factory)
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(m.socket()).is_err() {
            assert!(!daemon.is_finished(), "the daemon did not start: {:?}", daemon.join());
            assert!(std::time::Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        m
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.0.join("home").display().to_string(),
            "XDG_RUNTIME_DIR" => self.0.join("run").display().to_string(),
            _ => String::new(),
        }
    }

    fn socket(&self) -> std::path::PathBuf {
        gruntas_harness::daemon::socket(&self.env()).unwrap()
    }

    fn command(&self, exe: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(exe);
        cmd.env_clear()
            .env("HOME", self.0.join("home"))
            .env("XDG_RUNTIME_DIR", self.0.join("run"))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .current_dir(&self.0)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    }

    /// `gruntas hook claude <event>` with `payload`: what it printed.
    fn hook(&self, event: &str, payload: &serde_json::Value) -> String {
        use std::io::Write;
        let mut child = self.command(env!("CARGO_BIN_EXE_gruntas")).args(["hook", "claude", event]).spawn().unwrap();
        child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{event}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// `gruntas-mcp` called with each of `calls` (a tool and its arguments)
    /// after its initialize: each call's result.
    fn mcp(&self, calls: &[(&str, serde_json::Value)]) -> Vec<serde_json::Value> {
        use std::io::Write;
        let mut lines = vec![r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_string()];
        for (n, (name, args)) in calls.iter().enumerate() {
            lines.push(serde_json::json!({ "jsonrpc": "2.0", "id": n + 1, "method": "tools/call", "params": { "name": name, "arguments": args } }).to_string());
        }
        let mut child = self.command(env!("CARGO_BIN_EXE_gruntas-mcp")).spawn().unwrap();
        child.stdin.take().unwrap().write_all(format!("{}\n", lines.join("\n")).as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let answers: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(answers.len(), calls.len() + 1, "{answers:?} {}", String::from_utf8_lossy(&out.stderr));
        answers[1..].iter().map(|a| a["result"].clone()).collect()
    }

    /// Waits until the daemon has written `id`'s registration, which the
    /// MCP server reads.
    fn registered(&self, id: &str) {
        let dir = gruntas_harness::daemon::dir(&self.env()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while gruntas_harness::daemon::registry::Registry::load(&dir).get(id).is_none() {
            assert!(std::time::Instant::now() < deadline, "{id} never registered");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(event: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/hooks/claude").join(format!("{event}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// SL03: `gruntas_report` (state, summary, phase, blocked on, PR, next)
/// updates the calling session's Task with agent provenance, appends to its
/// history and answers the current status; the 11th report drops the
/// oldest (`status::MAX_HISTORY`).
#[test]
fn sl03_a_report_updates_the_task_and_its_history_keeps_ten() {
    use gruntas_harness::protocol::{Activity, PluginHarness, Registration};
    let m = Machine::up("sl03-report");
    let reg = Registration { session_id: "mine".into(), harness: PluginHarness::Claude, harness_pid: Some(std::process::id()), config_home: None, cwd: Some("/r".into()), repository: None, transcript_path: None, attendance: None };
    gruntas_harness::hook::send(&m.socket(), "/r", "test", Activity::Register(reg)).unwrap();
    m.registered("mine");
    let mut calls = vec![("gruntas_report", serde_json::json!({ "state": "working", "summary": "report 1", "phase": "build", "blocked_on": "nothing", "pr": "https://github.com/o/r/pull/1", "next": "tests" }))];
    for n in 2..=10 {
        calls.push(("gruntas_report", serde_json::json!({ "summary": format!("report {n}") })));
    }
    calls.push(("gruntas_report", serde_json::json!({ "state": "input-required", "summary": "report 11", "blocked_on": "your review" })));
    calls.push(("gruntas_report", serde_json::json!({ "state": "stalled", "summary": "x" })));
    calls.push(("gruntas_report", serde_json::json!({ "state": "working" })));
    calls.push(("gruntas_report", serde_json::json!({ "summary": "x", "pr": "javascript:alert(1)" })));
    let results = m.mcp(&calls);
    for r in &results[..11] {
        assert_eq!(r["isError"], false, "{r}");
    }
    let first: Task = serde_json::from_value(results[0]["structuredContent"]["task"].clone()).unwrap();
    assert_eq!((first.id.as_str(), first.status.state, first.history.len()), ("mine", TaskState::Working, 1));
    assert_eq!(first.metadata.gruntas.phase, Some(Field::agent("build".to_string())));
    let report = &first.history[0].metadata.as_ref().unwrap().report;
    assert_eq!((report.blocked_on.as_deref(), report.pr.as_deref(), report.next.as_deref()), (Some("nothing"), Some("https://github.com/o/r/pull/1"), Some("tests")));

    let last = &results[10];
    let task: Task = serde_json::from_value(last["structuredContent"]["task"].clone()).unwrap();
    assert_eq!(task.status.state, TaskState::InputRequired, "the last report's state");
    assert_eq!(task.status.message.as_ref().unwrap().summary(), "report 11");
    assert_eq!(task.metadata.gruntas.state_source, Some(gruntas_client::status::Provenance::Agent));
    assert_eq!(task.metadata.gruntas.phase, Some(Field::agent("build".to_string())), "a field a report leaves out stays");
    assert_eq!(task.history.len(), gruntas_client::status::MAX_HISTORY);
    let summaries: Vec<String> = task.history.iter().map(Message::summary).collect();
    assert_eq!(summaries.first().map(String::as_str), Some("report 2"), "the 11th dropped the oldest: {summaries:?}");
    assert_eq!(summaries.last().map(String::as_str), Some("report 11"));
    let text = last["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("input-required — report 11"), "{text}");
    // Neither a state A2A does not have, a missing summary nor a link that
    // is not a web one is taken.
    for r in &results[11..] {
        assert_eq!(r["isError"], true, "{r}");
    }

    // More sessions in this one process: which is reporting cannot be
    // told, so none is guessed.
    for id in ["x1", "x2"] {
        let reg = Registration { session_id: id.into(), harness: PluginHarness::Codex, harness_pid: Some(std::process::id()), config_home: None, cwd: None, repository: None, transcript_path: None, attendance: None };
        gruntas_harness::hook::send(&m.socket(), "/r", "test", Activity::Register(reg)).unwrap();
    }
    m.registered("x2");
    let r = &m.mcp(&[("gruntas_report", serde_json::json!({ "summary": "which?" }))])[0];
    assert_eq!((&r["isError"], &r["structuredContent"]["error"]), (&serde_json::json!(true), &serde_json::json!("ambiguous_session")), "{r}");
}

/// SL03: `Stop` after tool calls with no report since yields a one-line
/// reminder to report; after a report, or with no tool calls, nothing.
#[test]
fn sl03_stop_reminds_only_after_unreported_work() {
    let m = Machine::up("sl03-stop");
    let session = fixture("Stop")["session_id"].as_str().unwrap().to_string();
    assert_eq!(m.hook("SessionStart", &fixture("SessionStart")), "");
    m.registered(&session);
    assert_eq!(m.hook("Stop", &fixture("Stop")), "", "no tool calls yet");
    assert_eq!(m.hook("PostToolUse", &fixture("PostToolUse")), "");
    let reminder = m.hook("Stop", &fixture("Stop"));
    let answer: serde_json::Value = serde_json::from_str(reminder.trim()).unwrap_or_else(|e| panic!("{e}: {reminder:?}"));
    assert_eq!(answer["decision"], "block", "{answer}");
    let reason = answer["reason"].as_str().unwrap();
    assert!(reason.contains("gruntas_report") && !reason.contains('\n'), "one line: {reason}");
    assert_eq!(m.hook("Stop", &fixture("Stop")), "", "reminded once, not every turn");

    // Work, then a report: nothing to remind of, the report's own call
    // included.
    let mut failed = fixture("PostToolUse");
    failed["hook_event_name"] = "PostToolUseFailure".into();
    assert_eq!(m.hook("PostToolUseFailure", &failed), "");
    let reported = m.mcp(&[("gruntas_report", serde_json::json!({ "state": "working", "summary": "halfway" }))]);
    assert_eq!(reported[0]["isError"], false, "{}", reported[0]);
    assert_eq!(reported[0]["structuredContent"]["task"]["id"], session.as_str());
    let mut own = fixture("PostToolUse");
    own["tool_name"] = "mcp__plugin_gruntas_gruntas__gruntas_report".into();
    assert_eq!(m.hook("PostToolUse", &own), "");
    assert_eq!(m.hook("Stop", &fixture("Stop")), "", "after a report, none");

    // A harness already carrying on from a Stop answer is never held twice.
    assert_eq!(m.hook("PostToolUse", &fixture("PostToolUse")), "");
    let mut active = fixture("Stop");
    active["stop_hook_active"] = true.into();
    assert_eq!(m.hook("Stop", &active), "");
}

// ---- SL04: observe states and git context from the harness ----

/// The most a hook may take, as the harness sees it.
const HOOK_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// A test's own home and runtime directory, under /tmp so the daemon's
/// socket path fits `sun_path`; the daemon a hook started is stopped as it
/// goes.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = std::path::PathBuf::from("/tmp").join(format!("gruntas-sl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Scratch(root)
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.0.join("home").display().to_string(),
            "XDG_RUNTIME_DIR" => self.0.join("run").display().to_string(),
            _ => String::new(),
        }
    }

    fn command(&self, program: &std::path::Path) -> std::process::Command {
        let mut c = std::process::Command::new(program);
        c.env_clear().env("HOME", self.0.join("home")).env("XDG_RUNTIME_DIR", self.0.join("run")).env("PATH", std::env::var("PATH").unwrap_or_default());
        c
    }

    /// Runs `gruntas hook claude <event>` with `payload` on stdin, as the
    /// harness does: exit 0, nothing on stdout, inside the limit.
    fn hook(&self, event: &str, payload: &serde_json::Value) {
        use std::io::Write;
        use std::process::Stdio;
        let at = std::time::Instant::now();
        let mut child = self.command(std::path::Path::new(env!("CARGO_BIN_EXE_gruntas"))).args(["hook", "claude", event]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let took = at.elapsed();
        assert!(out.status.success() && out.stdout.is_empty() && took < HOOK_LIMIT, "{event}: {out:?} in {took:?}");
    }

    /// The session's Task once `want` holds of it, as the daemon writes it.
    fn task_once(&self, id: &str, what: &str, want: impl Fn(&Task) -> bool) -> Task {
        let dir = gruntas_harness::daemon::dir(&self.env()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let t = gruntas_harness::daemon::tasks::Tasks::load(&dir).get(id).cloned();
            if let Some(t) = t.as_ref().filter(|t| want(t)) {
                return t.clone();
            }
            assert!(std::time::Instant::now() < deadline, "never {what}: {t:#?}\n{}", std::fs::read_to_string(dir.join("hook.log")).unwrap_or_default());
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        use std::process::Stdio;
        if gruntas_harness::daemon::socket(&self.env()).is_ok_and(|s| s.exists()) {
            let _ = self.command(std::path::Path::new(env!("CARGO_BIN_EXE_gruntas"))).args(["host", "stop", "--force"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn hook_payload(event: &str, session: &str, cwd: &std::path::Path) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/hooks/claude").join(format!("{event}.json"));
    let mut p: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    p["session_id"] = session.into();
    p["cwd"] = cwd.display().to_string().into();
    p
}

fn state_of(t: &Task) -> (TaskState, Option<Provenance>) {
    (t.status.state, t.metadata.gruntas.state_source)
}

/// SL04: `PermissionRequest` makes the session input-required (harness
/// provenance); `PostToolUse`, `PermissionDenied` or the next activity
/// clears it to the agent's last state; `SessionEnd` with no terminal
/// report makes it completed. Before any report the agent's last state is
/// working as the harness sees it; after one, what it reported.
#[test]
fn sl04_hook_events_set_and_clear_observed_states() {
    let s = Scratch::new("sl04-states");
    let id = "5b0e6a52-0000-4000-8000-000000000001";
    let cwd = s.0.clone();
    s.hook("SessionStart", &hook_payload("SessionStart", id, &cwd));
    let t = s.task_once(id, "registered", |t| t.metadata.gruntas.health.is_some());
    assert_eq!(state_of(&t), (TaskState::Working, Some(Provenance::Harness)));
    assert_eq!(t.metadata.gruntas.health, Some(Field::harness(Health::Serving)));
    assert_eq!(t.metadata.gruntas.harness, Some(Field::harness("claude".into())));
    // Cleared with nothing reported: working, as the harness sees it.
    s.hook("PermissionRequest", &hook_payload("PermissionRequest", id, &cwd));
    s.task_once(id, "input-required", |t| t.status.state == TaskState::InputRequired);
    s.hook("PostToolUse", &hook_payload("PostToolUse", id, &cwd));
    let t = s.task_once(id, "cleared", |t| t.status.state != TaskState::InputRequired);
    assert_eq!(state_of(&t), (TaskState::Working, Some(Provenance::Harness)));
    // The agent reports working, as gruntas_report sends it (SL03).
    let socket = gruntas_harness::daemon::socket(&s.env()).unwrap();
    let report = gruntas_harness::protocol::Activity::Report { session_id: id.into(), summary: "on it".into(), report: gruntas_client::status::Report { state: Some(TaskState::Working), ..Default::default() } };
    gruntas_harness::hook::send(&socket, &cwd.display().to_string(), "test", report).unwrap();
    s.task_once(id, "reported", |t| t.metadata.gruntas.state_source == Some(Provenance::Agent));
    for clearing in ["PostToolUse", "PermissionDenied", "PreToolUse"] {
        s.hook("PermissionRequest", &hook_payload("PermissionRequest", id, &cwd));
        let t = s.task_once(id, "input-required", |t| t.status.state == TaskState::InputRequired);
        assert_eq!(state_of(&t), (TaskState::InputRequired, Some(Provenance::Harness)));
        assert!(t.metadata.gruntas.last_activity.as_ref().is_some_and(|a| a.source == Provenance::Harness));
        s.hook(clearing, &hook_payload(clearing, id, &cwd));
        let t = s.task_once(id, &format!("cleared by {clearing}"), |t| t.status.state != TaskState::InputRequired);
        assert_eq!(state_of(&t), (TaskState::Working, Some(Provenance::Agent)), "back to the agent's last state after {clearing}");
    }
    s.hook("SessionEnd", &hook_payload("SessionEnd", id, &cwd));
    let t = s.task_once(id, "completed", |t| t.status.state == TaskState::Completed);
    assert_eq!(state_of(&t), (TaskState::Completed, Some(Provenance::Harness)));
    assert_eq!(t.metadata.gruntas.health, Some(Field::harness(Health::NotServing)));
}

/// SL04: a harness process gone without `SessionEnd` makes its session
/// canceled and NOT_SERVING; branch, HEAD, worktree and dirty come from the
/// daemon's own git run, and the hooks stay within their 2 s limit.
#[test]
fn sl04_a_vanished_harness_is_canceled_and_not_serving_and_git_context_is_the_daemons() {
    use std::process::{Command, Stdio};
    let s = Scratch::new("sl04-vanished");
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let o = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgSign=false"]).args(args).current_dir(&repo).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "sl04-branch"]);
    git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    std::fs::write(repo.join("a"), "a").unwrap();
    git(&["add", "a"]);
    git(&["commit", "-q", "-m", "a"]);
    let head = git(&["rev-parse", "HEAD"]);
    std::fs::write(repo.join("b"), "untracked").unwrap();

    // The harness: a process that is not a shell (perl, on every unix CI
    // runs on), so the hook it runs names it, and that outlives the hook
    // until killed. It times the hook as a harness would, to its exit.
    let id = "5b0e6a52-0000-4000-8000-000000000002";
    let payload = s.0.join("start.json");
    std::fs::write(&payload, hook_payload("SessionStart", id, &repo).to_string()).unwrap();
    let took_file = s.0.join("took");
    let script = r#"open(STDIN, "<", $ARGV[1]) or die; my $t = time; system($ARGV[0], "hook", "claude", "SessionStart"); open(my $f, ">", "$ARGV[2].tmp") or die; printf $f "%d", (time - $t) * 1000; close $f; rename("$ARGV[2].tmp", $ARGV[2]); sleep 60"#;
    let mut child = s.command(std::path::Path::new("perl")).args(["-MTime::HiRes=time", "-e", script, env!("CARGO_BIN_EXE_gruntas")]).arg(&payload).arg(&took_file).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();

    let t = s.task_once(id, "with its git context", |t| t.metadata.gruntas.head.is_some());
    let g = &t.metadata.gruntas;
    assert_eq!(g.branch, Some(Field::harness("sl04-branch".into())));
    assert_eq!(g.head, Some(Field::harness(head)));
    assert_eq!(g.worktree, Some(Field::harness(repo.canonicalize().unwrap().display().to_string())));
    assert_eq!(g.dirty, Some(Field::harness(true)));
    assert_eq!(g.project, Some(Field::harness("github.com/acme/app".into())));
    assert_eq!(g.health, Some(Field::harness(Health::Serving)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let took: u64 = loop {
        match std::fs::read_to_string(&took_file) {
            Ok(t) => break t.trim().parse().unwrap(),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => panic!("the harness never timed its hook: {e}"),
        }
    };
    assert!(took < HOOK_LIMIT.as_millis() as u64, "SessionStart took {took} ms");

    // A clean tree reads clean at the next read after activity.
    std::fs::remove_file(repo.join("b")).unwrap();
    s.hook("PostToolUse", &hook_payload("PostToolUse", id, &repo));
    s.task_once(id, "clean", |t| t.metadata.gruntas.dirty == Some(Field::harness(false)));

    child.kill().unwrap();
    child.wait().unwrap();
    let t = s.task_once(id, "canceled", |t| t.status.state == TaskState::Canceled);
    assert_eq!(state_of(&t), (TaskState::Canceled, Some(Provenance::Harness)));
    assert_eq!(t.metadata.gruntas.health, Some(Field::harness(Health::NotServing)));
}

// ---- SL05: publish and read sealed status boards ----

#[path = "common/device_list.rs"]
mod device_list;

const TOKEN: &str = "krowk_sk_status_layer_00000000000000000";

/// One person's devices on the stand-in registry: each a home of its own,
/// on the person's device list with its own key, as `sync init` and `sync
/// join` leave a machine.
struct Person {
    /// In every path it makes: the tests run side by side in one process.
    name: String,
    root: std::path::PathBuf,
    api: String,
    people: device_list::People,
    account: gruntas_client::e2e::AccountKey,
    homes: Vec<Home>,
    _registry: gruntas_devregistry::Running,
}

/// A machine: its home, its runtime directory (short, for the socket), its
/// environment, and its `gruntas host serve` once started.
struct Home {
    home: std::path::PathBuf,
    run: std::path::PathBuf,
    env: Vec<(String, String)>,
    serve: Option<std::process::Child>,
}

impl Person {
    fn new(name: &str) -> Person {
        let root = std::env::temp_dir().join(format!("gruntas-sl05-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let registry = gruntas_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        Person { name: name.into(), root, api: format!("{}/v1", registry.url()), people: Default::default(), account: gruntas_client::e2e::AccountKey::generate(), homes: Vec::new(), _registry: registry }
    }

    /// Machine `name` added to the person's list: its index.
    fn machine(&mut self, name: &str) -> usize {
        let home = self.root.join(name);
        std::fs::create_dir_all(home.join(".gruntas")).unwrap();
        std::fs::write(home.join(".gruntas/config.json"), "{}").unwrap();
        let run = std::path::PathBuf::from(format!("/tmp/gruntas-sl05-{}-{name}-{}", self.name, std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ks = gruntas_client::keystore::Keystore::new(&home.join(".gruntas"));
        ks.recover(gruntas_client::e2e::AccountKey::from_bytes(*self.account.as_bytes())).unwrap();
        self.people.enlist(&ks, name);
        self.people.publish(&self.api, TOKEN);
        let env = vec![
            ("HOME".into(), home.display().to_string()),
            ("XDG_RUNTIME_DIR".into(), run.display().to_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("GRUNTAS_NO_UPDATE_CHECK".into(), "1".into()),
            ("GRUNTAS_HOST_IDLE".into(), "0".into()),
            ("GRUNTAS_API_URL".into(), self.api.clone()),
            ("GRUNTAS_TOKEN".into(), device_list::key(TOKEN, name)),
            ("GRUNTAS_RELAY_URL".into(), "ws://127.0.0.1:9".into()),
            ("GRUNTAS_TAILSCALE_SOCKET".into(), run.join("no-tailscaled.sock").display().to_string()),
        ];
        self.homes.push(Home { home, run, env, serve: None });
        self.homes.len() - 1
    }

    /// The registry as machine `i`.
    fn client(&self, i: usize) -> gruntas_api::Client {
        self.people.client(&self.api, TOKEN, i)
    }

    fn device(&self, i: usize) -> String {
        self.people.chain().devices()[i].id().to_string()
    }

    /// Every board machine `i` reads, by device.
    fn read(&self, i: usize) -> Vec<gruntas_harness::sync::boards::Read> {
        let keys = gruntas_client::user_key::UserKeys::new(self.people.user().clone(), []).unwrap();
        gruntas_harness::sync::boards::read(&self.client(i), &keys, self.people.chain()).unwrap()
    }

    /// What the stand-in holds of boards and counts.
    fn held(&self) -> serde_json::Value {
        let url = format!("{}/_status_layer", self.api.trim_end_matches("/v1"));
        serde_json::from_str(&ureq::get(&url).call().unwrap().body_mut().read_to_string().unwrap()).unwrap()
    }

    /// Machine `i`'s board as machine `reader` opens it, once `want` holds.
    fn board_of(&self, reader: usize, i: usize, want: impl Fn(&Board) -> bool) -> Board {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let read = self.read(reader);
            if let Some(Ok(b)) = read.iter().find(|r| r.device == self.device(i)).map(|r| r.board.clone())
                && want(&b)
            {
                return b;
            }
            assert!(std::time::Instant::now() < deadline, "machine {reader} never read machine {i}'s board as wanted: {read:?}\n{}", self.homes[i].logs());
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

impl Home {
    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args).env_clear().envs(self.env.iter().map(|(k, v)| (k, v))).current_dir(&self.home);
        c
    }

    /// What its daemon wrote, for a failure's message.
    fn logs(&self) -> String {
        let mut out = String::new();
        for f in walk(&self.run).into_iter().chain([self.home.join(".gruntas/host.log")]) {
            if f.extension().is_some_and(|e| e == "log" || e == "json") {
                out += &format!("{}:\n{}\n", f.display(), std::fs::read_to_string(&f).unwrap_or_default());
            }
        }
        out
    }

    /// Its host daemon kept up, announcing it and publishing its board.
    fn serve(&mut self) {
        let log = std::fs::File::create(self.run.join("serve.log")).unwrap();
        let c = self.command(&["host", "serve"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log).spawn().unwrap();
        self.serve = Some(c);
    }

    /// A Claude Code session registered through the plugin's hook, this
    /// test its harness process.
    fn register(&self, session: &str) {
        let mut payload: serde_json::Value = serde_json::from_slice(&std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/hooks/claude/SessionStart.json")).unwrap()).unwrap();
        payload["session_id"] = session.into();
        payload["cwd"] = self.home.display().to_string().into();
        let mut child = self.command(&["hook", "claude", "SessionStart"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["host", "stop", "--force"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        if let Some(mut c) = self.serve.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.run);
    }
}

impl Drop for Person {
    fn drop(&mut self) {
        self.homes.clear();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.flatten().flat_map(|e| if e.path().is_dir() { walk(&e.path()) } else { vec![e.path()] }).collect()
}

/// When the stand-in last took machine `device`'s board, if it has one.
fn board_updated_at(held: &serde_json::Value, device: &str) -> Option<String> {
    held["status_boards"].as_array()?.iter().find(|b| b["device"] == device).and_then(|b| b["updated_at"].as_str()).map(String::from)
}

/// SL05: two devices on the dev registry each read the other's board
/// (listed through presences, fetched and opened per device); a burst of
/// changes publishes at most once per 5 s; "quiet for" is computed from
/// the board's publish time and its receipt, never across clocks.
#[test]
fn sl05_two_devices_read_each_others_boards_and_publishes_coalesce() {
    let mut p = Person::new("two");
    let (a, b) = (p.machine("a"), p.machine("b"));
    p.homes[a].serve();
    p.homes[b].serve();
    // Both daemons up and publishing before any session registers, so no
    // hook starts a daemon of its own.
    p.board_of(b, a, |_| true);
    p.board_of(a, b, |_| true);
    p.homes[a].register("a-session-0");
    p.homes[b].register("b-session-0");
    let on_a = p.board_of(b, a, |x| x.sessions == ["a-session-0"]);
    let on_b = p.board_of(a, b, |x| x.sessions == ["b-session-0"]);
    for (board, device) in [(&on_a, p.device(a)), (&on_b, p.device(b))] {
        let t = &board.tasks[0];
        assert_eq!((t.status.state, board.device.as_str()), (TaskState::Working, device.as_str()));
        assert_eq!(t.metadata.gruntas.machine.as_ref().map(|m| m.value.as_str()), Some(device.as_str()));
        let quiet = gruntas_client::status::quiet_for(board, t, jiff::Timestamp::now(), jiff::Timestamp::now()).expect("a last activity");
        assert!(quiet < jiff::SignedDuration::from_secs(60), "{quiet:?}");
    }

    // A burst: twenty sessions registered as fast as the hook goes, the
    // stand-in's record watched meanwhile and for 12 s after.
    let device = p.device(a);
    let mut seen: Vec<String> = board_updated_at(&p.held(), &device).into_iter().collect();
    let watch = |seen: &mut Vec<String>| {
        if let Some(at) = board_updated_at(&p.held(), &device)
            && seen.last() != Some(&at)
        {
            seen.push(at);
        }
    };
    for i in 1..=20 {
        p.homes[a].register(&format!("a-session-{i}"));
        watch(&mut seen);
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(12);
    while std::time::Instant::now() < until {
        watch(&mut seen);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let times: Vec<jiff::Timestamp> = seen.iter().map(|s| s.parse().unwrap()).collect();
    assert!(times.len() >= 2, "the burst was published: {seen:?}");
    for w in times.windows(2) {
        // The registry's arrival times: slack for a put held up in flight.
        assert!(w[1].duration_since(w[0]) >= jiff::SignedDuration::from_secs(4), "two publishes within 5 s: {seen:?}");
    }
    let after = p.board_of(b, a, |x| x.sessions.len() == 21);
    assert_eq!(after.tasks.len(), 21, "every session of the burst is on the board");
}

/// SL05: each publish sends the counts under the device's random
/// statistics id, and the registry holds nothing else of the board.
#[test]
fn sl05_the_counts_carry_only_the_allowed_fields() {
    let mut p = Person::new("counts");
    let a = p.machine("a");
    p.machine("b");
    p.homes[a].serve();
    p.board_of(1, a, |_| true);
    p.homes[a].register("counted-1");
    p.homes[a].register("counted-2");
    p.board_of(1, a, |x| x.sessions.len() == 2);
    let id = std::fs::read_to_string(p.homes[a].home.join(".gruntas").join(gruntas_harness::sync::boards::STATISTICS_ID_FILE)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let row = loop {
        let held = p.held();
        let row = held["statistics"].as_array().and_then(|s| s.iter().find(|r| r["id"] == id.as_str()).cloned());
        if let Some(r) = row.filter(|r| r["states"]["TASK_STATE_WORKING"] == 2) {
            break r;
        }
        assert!(std::time::Instant::now() < deadline, "no counts of two working sessions under {id}: {held}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let mut keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["health", "health_counts", "id", "states", "updated_at"], "{row}");
    assert_eq!((row["health"].as_str(), &row["health_counts"]), (Some("SERVING"), &serde_json::json!({"SERVING": 2})), "{row}");
    assert_eq!(row["states"], serde_json::json!({"TASK_STATE_WORKING": 2}), "{row}");
    assert!(gruntas_client::status::is_statistics_id(&id) && id != p.device(a), "a random id, not the device's");
    let held = p.held();
    let board = held["status_boards"].as_array().unwrap().iter().find(|b| b["device"] == p.device(a).as_str()).unwrap();
    let mut keys: Vec<&str> = board.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["device", "sealed", "updated_at"], "only the sealed bytes of the board");
    let sealed = String::from_utf8_lossy(&gruntas_client::e2e::unhex(board["sealed"].as_str().unwrap()).unwrap()).into_owned();
    assert!(!sealed.contains("counted-1"), "the board is sealed");
}

/// SL05 (D8): a board over budget, published through the same path the
/// daemon takes, is trimmed in order (old finished Tasks, then history
/// beyond three reports, then summaries) and still opens on another device.
#[test]
fn sl05_an_over_budget_board_is_trimmed_in_order_and_read_elsewhere() {
    use gruntas_client::status::{MessageMetadata, Report, MAX_HISTORY, MAX_SEALED, SHORT_SUMMARY, TRIMMED_HISTORY};
    let mut p = Person::new("budget");
    let (a, b) = (p.machine("a"), p.machine("b"));
    let device = gruntas_client::e2e::DeviceId::parse(&p.device(a)).unwrap();
    let keys = gruntas_client::user_key::UserKeys::new(p.people.user().clone(), []).unwrap();
    let announcer = gruntas_harness::sync::announce::Announcer { api: std::sync::Arc::new(p.client(a)), keys, chain: p.people.chain().clone(), device };
    p.client(a).put_presence(b"a", 30).unwrap();
    let long = "x".repeat(2000);
    let report = |i: usize| {
        let mut m = Message::text(format!("r{i}"), long.clone());
        m.metadata = Some(MessageMetadata { report: Report { timestamp: "2026-11-23T11:59:00Z".into(), ..Default::default() } });
        m
    };
    let mut tasks: Vec<Task> = (0..30).map(|i| task(&format!("old-{i}"), TaskState::Completed, "github.com/acme/app", None, &long, "2026-11-23T09:00:00Z")).collect();
    tasks.extend((0..40).map(|i| {
        let mut t = task(&format!("live-{i}"), TaskState::Working, "github.com/acme/app", None, &long, "2026-11-23T11:59:00Z");
        t.history = (0..MAX_HISTORY).map(report).collect();
        t
    }));
    let board = Board { device: p.device(a), published_at: "2026-11-23T12:00:00Z".into(), sessions: tasks.iter().map(|t| t.id.clone()).collect(), tasks };
    assert!(board.sealed_len() > MAX_SEALED);
    gruntas_harness::sync::boards::publish(&board, &announcer, &gruntas_client::status::statistics_id()).unwrap();
    let read = p.board_of(b, a, |_| true);
    assert!(read.tasks.iter().all(|t| t.id.starts_with("live-")) && read.tasks.len() == 40, "old finished Tasks went first");
    assert!(read.tasks.iter().all(|t| t.history.len() == TRIMMED_HISTORY), "then history beyond three reports");
    assert!(read.tasks.iter().all(|t| t.status.message.as_ref().unwrap().summary().chars().count() == SHORT_SUMMARY), "then summaries");
    assert_eq!(read.sessions.len(), 70, "every session still listed");
}

// ---- SL06: show status and ping machines and sessions ----

/// Machine `i`'s presence put for `ttl` seconds and `board`, about it,
/// published as its daemon would (`boards::publish`).
fn put_board(p: &Person, i: usize, mut board: Board, ttl: u64) {
    board.device = p.device(i);
    let device = gruntas_client::e2e::DeviceId::parse(&board.device).unwrap();
    let keys = gruntas_client::user_key::UserKeys::new(p.people.user().clone(), []).unwrap();
    let announcer = gruntas_harness::sync::announce::Announcer { api: std::sync::Arc::new(p.client(i)), keys, chain: p.people.chain().clone(), device };
    p.client(i).put_presence(b"presence", ttl).unwrap();
    gruntas_harness::sync::boards::publish(&board, &announcer, &gruntas_client::status::statistics_id()).unwrap();
}

/// `gruntas <args>` on machine `i`: its exit code, stdout and stderr.
fn run(p: &Person, i: usize, args: &[&str]) -> (i32, String, String) {
    let o = p.homes[i].command(args).stdin(std::process::Stdio::null()).output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

/// SL06, SL08: `gruntas status` (D10's tree; `gruntas agents`, its name
/// for a release, still answers the same) is the tree by project, machine
/// and parent session, each with state, health, quiet for and summary;
/// `gruntas status github.com/acme/site` only that project; `--json` the
/// same tree.
#[test]
fn sl06_status_shows_the_tree_across_machines() {
    let mut p = Person::new("tree");
    let (laptop, server, desk) = (p.machine("laptop"), p.machine("server"), p.machine("desk"));
    let mut boards = two_machines().into_iter();
    put_board(&p, laptop, boards.next().unwrap(), 60);
    put_board(&p, server, boards.next().unwrap(), 60);

    let (code, out, err) = run(&p, desk, &["status", "--format", "human"]);
    assert_eq!(code, 0, "{out}{err}");
    let want = "\
github.com/acme/app
  laptop  SERVING
    p1  working  SERVING  quiet 1m  splitting the train
      c1  input-required  SERVING  quiet 10m  waiting on a permission
  server  SERVING
    s1  canceled  NOT_SERVING  quiet 29m  fixing CI
github.com/acme/site
  laptop  SERVING
    o1  completed  SERVING  quiet 1h  shipped
";
    assert_eq!(out, want, "{err}");
    let (code, alias, _) = run(&p, desk, &["agents", "--format", "human"]);
    assert_eq!((code, alias.as_str()), (0, want), "`agents`, the tree's name for a release, still answers");

    let (code, out, _) = run(&p, desk, &["status", "github.com/acme/site", "--format", "human"]);
    assert_eq!((code, out.as_str()), (0, "github.com/acme/site\n  laptop  SERVING\n    o1  completed  SERVING  quiet 1h  shipped\n"));
    let (_, out, _) = run(&p, desk, &["status", "https://github.com/acme/site.git", "--format", "human"]);
    assert!(out.starts_with("github.com/acme/site\n"), "a project named as a remote is normalised: {out}");

    let (code, out, err) = run(&p, desk, &["status", "--json"]);
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let projects = &v["data"]["projects"];
    assert_eq!(projects.as_array().unwrap().len(), 2, "{v}");
    let app = &projects[0];
    assert_eq!(app["project"], "github.com/acme/app");
    let (on_laptop, on_server) = (&app["machines"][0], &app["machines"][1]);
    assert_eq!((on_laptop["name"].as_str(), on_laptop["device"].as_str(), on_laptop["health"].as_str()), (Some("laptop"), Some(p.device(laptop).as_str()), Some("SERVING")));
    assert_eq!(on_laptop["publishedAt"], "2026-11-23T12:00:00Z");
    let p1 = &on_laptop["sessions"][0];
    assert_eq!((p1["id"].as_str(), p1["state"].as_str(), p1["health"].as_str(), p1["summary"].as_str()), (Some("p1"), Some("TASK_STATE_WORKING"), Some("SERVING"), Some("splitting the train")));
    assert!((60..65).contains(&p1["quietForSeconds"].as_i64().unwrap()), "from the board's publish time and its receipt: {p1}");
    assert_eq!(p1["lastActivity"], "2026-11-23T11:59:00Z");
    let c1 = &p1["children"][0];
    assert_eq!((c1["id"].as_str(), c1["state"].as_str(), c1["children"].as_array().map(Vec::len)), (Some("c1"), Some("TASK_STATE_INPUT_REQUIRED"), Some(0)));
    let s1 = &on_server["sessions"][0];
    assert_eq!((on_server["name"].as_str(), s1["id"].as_str(), s1["state"].as_str(), s1["health"].as_str()), (Some("server"), Some("s1"), Some("TASK_STATE_CANCELED"), Some("NOT_SERVING")));
    assert_eq!(projects[1]["machines"][0]["sessions"][0]["id"], "o1");
    assert_eq!(v["data"]["unopened"], serde_json::json!([]));
}

/// SL06: `gruntas ping <machine>` answers "answered in N s"; `gruntas ping
/// <session>` finds its machine from the boards and adds health and last
/// activity; an UNKNOWN machine says so without waiting.
#[test]
fn sl06_ping_answers_for_a_machine_and_a_session_and_not_for_an_unknown_one() {
    let mut p = Person::new("ping");
    let (a, b, c) = (p.machine("a"), p.machine("b"), p.machine("gone"));
    // Machine `gone` announced itself for 10 s, the least, with one session
    // on its board, and nothing keeps it up: its presence lapses meanwhile.
    let lapses = std::time::Instant::now() + std::time::Duration::from_secs(11);
    let mut board = two_machines().pop().unwrap();
    board.published_at = jiff::Timestamp::now().to_string();
    put_board(&p, c, board, 10);

    p.homes[a].serve();
    p.board_of(b, a, |_| true);
    p.homes[a].register("a-session-0");
    p.board_of(b, a, |x| x.sessions == ["a-session-0"]);

    let (code, out, err) = run(&p, b, &["ping", "a", "--format", "human"]);
    assert_eq!(code, 0, "{out}{err}");
    let secs: f64 = out.strip_prefix("a: answered in ").and_then(|r| r.strip_suffix(" s\n")).unwrap_or_else(|| panic!("{out}")).parse().unwrap();
    assert!(secs < 15.0, "within a presence refresh: {out}");

    let (code, out, err) = run(&p, b, &["ping", "a-session-0", "--json"]);
    assert_eq!(code, 0, "{out}{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let d = &v["data"];
    assert_eq!((d["machine"].as_str(), d["device"].as_str(), d["health"].as_str()), (Some("a"), Some(p.device(a).as_str()), Some("SERVING")), "{v}");
    assert!(d["answeredInSeconds"].as_f64().is_some_and(|s| s < 15.0), "{v}");
    let s = &d["session"];
    assert_eq!((s["id"].as_str(), s["health"].as_str()), (Some("a-session-0"), Some("SERVING")), "{v}");
    assert!(s["lastActivity"].as_str().is_some_and(|t| t.parse::<jiff::Timestamp>().is_ok()) && s["quietForSeconds"].as_i64().is_some_and(|q| (0..120).contains(&q)), "{v}");
    let (code, out, _) = run(&p, b, &["ping", "a-session-0", "--format", "human"]);
    assert!(code == 0 && out.starts_with("a-session-0 on a: answered in ") && out.contains("  SERVING  last activity "), "{out}");

    // An UNKNOWN machine, by name or by a session on its board: said at
    // once, nothing sent.
    while std::time::Instant::now() < lapses {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    for target in ["gone", "s1"] {
        let at = std::time::Instant::now();
        let (code, out, err) = run(&p, b, &["ping", target, "--format", "human"]);
        assert!(at.elapsed() < std::time::Duration::from_secs(5), "without waiting");
        assert_eq!(code, 6, "{out}{err}");
        assert!(err.contains("Gone is UNKNOWN: its presence lapsed (last seen ") && err.contains("so it was not pinged"), "{err}");
    }
    let (_, out, _) = run(&p, b, &["status", "--format", "human"]);
    assert!(out.contains("  gone  UNKNOWN\n    s1  canceled  NOT_SERVING"), "the tree says so too: {out}");
    let held: serde_json::Value = serde_json::from_str(&ureq::get(format!("{}/_host_commands", p.api.trim_end_matches("/v1"))).call().unwrap().body_mut().read_to_string().unwrap()).unwrap();
    assert!(!held.to_string().contains(&p.device(c)), "nothing was sent to the lapsed machine: {held}");

    let (code, _, err) = run(&p, b, &["ping", "nothing-by-this-name"]);
    assert_eq!(code, 2, "{err}");
}
