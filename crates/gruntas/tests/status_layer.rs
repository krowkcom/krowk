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

/// SL03: `gruntas_report` (state, summary, phase, blocked on, PR, next)
/// updates the calling session's Task with agent provenance, appends to its
/// history and answers the current status; the 11th report drops the
/// oldest (`status::MAX_HISTORY`).
#[test]
#[ignore = "SL03: report status from agents"]
fn sl03_a_report_updates_the_task_and_its_history_keeps_ten() {
    todo!("SL03: start gruntas-mcp under a registered harness_pid (as KT04's test does), call gruntas_report 11 times, read the Task: state and summary from the last, stateSource agent, history of 10 starting at the 2nd")
}

/// SL03: `Stop` after tool calls with no report since yields a one-line
/// reminder to report; after a report, or with no tool calls, nothing.
#[test]
#[ignore = "SL03: report status from agents"]
fn sl03_stop_reminds_only_after_unreported_work() {
    todo!("SL03: pipe claude/PostToolUse.json then claude/Stop.json into `gruntas hook claude <event>` with a daemon up; expect the reminder; report via gruntas_report, Stop again, expect none")
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
        Person { root, api: format!("{}/v1", registry.url()), people: Default::default(), account: gruntas_client::e2e::AccountKey::generate(), homes: Vec::new(), _registry: registry }
    }

    /// Machine `name` added to the person's list: its index.
    fn machine(&mut self, name: &str) -> usize {
        let home = self.root.join(name);
        std::fs::create_dir_all(home.join(".gruntas")).unwrap();
        std::fs::write(home.join(".gruntas/config.json"), "{}").unwrap();
        let run = std::path::PathBuf::from(format!("/tmp/gruntas-sl05-{name}-{}-{}", self.homes.len(), std::process::id()));
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
            assert!(std::time::Instant::now() < deadline, "machine {reader} never read machine {i}'s board as wanted: {read:?}\nhost.log: {}", std::fs::read_to_string(self.homes[i].home.join(".gruntas/host.log")).unwrap_or_default());
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

    /// Its host daemon kept up, announcing it and publishing its board.
    fn serve(&mut self) {
        let c = self.command(&["host", "serve"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
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
        assert!(w[1].duration_since(w[0]) >= jiff::SignedDuration::from_millis(4900), "two publishes within 5 s: {seen:?}");
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
