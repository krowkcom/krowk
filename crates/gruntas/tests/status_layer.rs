//! The status layer's acceptance, slice by slice (the "Status layer" train).
//! SL01 landed the contract (`gruntas_client::status`, the registry calls
//! and the dev registry's stand-ins, tested in gruntas-client's and
//! gruntas-api's `status` tests); each later ticket un-ignores its own tests
//! here, fills in what is `todo!`, and goes green on top of them.

#![cfg(all(feature = "harness", unix))]

use gruntas_client::status::{Board, Field, Health, Message, Task, TaskMetadata, TaskState, TaskStatus};

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

/// SL04: `PermissionRequest` makes the session input-required (harness
/// provenance); `PostToolUse`, `PermissionDenied` or the next activity
/// clears it to the agent's last state; `SessionEnd` with no terminal
/// report makes it completed.
#[test]
#[ignore = "SL04: observe states and git context from the harness"]
fn sl04_hook_events_set_and_clear_observed_states() {
    todo!("SL04: register via SessionStart, report working, pipe PermissionRequest (expect TASK_STATE_INPUT_REQUIRED, stateSource harness), PostToolUse (back to working, agent), SessionEnd (TASK_STATE_COMPLETED, harness)")
}

/// SL04: a harness process gone without `SessionEnd` makes its session
/// canceled and NOT_SERVING; branch, HEAD, worktree and dirty come from the
/// daemon's own git run, and the hooks stay within their 2 s limit.
#[test]
#[ignore = "SL04: observe states and git context from the harness"]
fn sl04_a_vanished_harness_is_canceled_and_not_serving_and_git_context_is_the_daemons() {
    todo!("SL04: register a session whose harness_pid is a child the test kills; expect TASK_STATE_CANCELED with health NOT_SERVING; in a git repo expect branch/head/worktree/dirty with harness provenance")
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

/// SL06: `gruntas status` is the tree by project, machine and parent
/// session, each with state, health, quiet for and summary; `gruntas status
/// github.com/acme/site` only that project; `--json` the same tree.
#[test]
#[ignore = "SL06: show status and ping machines and sessions"]
fn sl06_status_shows_the_tree_across_machines() {
    let boards = two_machines();
    todo!(
        "SL06: serve these {} boards (sealed, on the dev registry) and run `gruntas status`: github.com/acme/app > laptop > p1 working > c1 input-required, server > s1 canceled NOT_SERVING; quiet for from each board's publishedAt; `gruntas status github.com/acme/site` shows o1 only; `--json` the same",
        boards.len()
    )
}

/// SL06: `gruntas ping <machine>` answers "answered in N s"; `gruntas ping
/// <session>` finds its machine from the boards and adds health and last
/// activity; an UNKNOWN machine says so without waiting.
#[test]
#[ignore = "SL06: show status and ping machines and sessions"]
fn sl06_ping_answers_for_a_machine_and_a_session_and_not_for_an_unknown_one() {
    todo!("SL06: ping host command against a live daemon on the dev registry; then let the presence lapse and expect UNKNOWN at once")
}
