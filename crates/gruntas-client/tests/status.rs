//! The status layer's contract (R-STA-1, D1, D8, D9): a session's Task is
//! A2A v1.0's shape with gruntas's fields under the extension URI, each
//! tagged with its provenance; a machine's board seals to the person's
//! devices within its budget, trimmed in D8's order; the counts carry the
//! machine's health and tallies and nothing else.

use gruntas_client::device_chain::{Action, Chain, Entry, Kind, SignedEntry, Subject};
use gruntas_client::e2e::{DeviceId, DeviceKey, SigningKey};
use gruntas_client::recovery::RecoveryKit;
use gruntas_client::status::{self, Board, Counts, Extension, Field, Health, Message, MessageMetadata, PlanEntry, PlanPriority, PlanStatus, Provenance, Report, Task, TaskMetadata, TaskState, TaskStatus};
use gruntas_client::user_key::{UserKey, UserKeys};
use serde_json::{json, Value};

const T: u64 = 1_790_000_000;
const PUBLISHED: &str = "2026-11-23T12:00:00Z";

fn task(id: &str, state: TaskState, at: &str) -> Task {
    Task {
        id: id.into(),
        status: TaskStatus { state, message: Some(Message::text(format!("{id}-status"), "building the board")), timestamp: at.into() },
        history: Vec::new(),
        metadata: TaskMetadata::default(),
    }
}

fn full_task() -> Task {
    let mut t = task("0123456789abcdef0123456789abcdef", TaskState::InputRequired, "2026-11-23T11:59:00Z");
    t.history = vec![Message {
        metadata: Some(MessageMetadata { report: Report { state: Some(TaskState::Working), phase: Some("build".into()), blocked_on: None, pr: Some("https://github.com/acme/app/pull/7".into()), next: Some("review".into()), timestamp: "2026-11-23T11:50:00Z".into() } }),
        ..Message::text("r1", "tests green, opening the PR")
    }];
    t.metadata.gruntas = Extension {
        state_source: Some(Provenance::Harness),
        health: Some(Field::harness(Health::Serving)),
        project: Some(Field::harness("github.com/acme/app".into())),
        machine: Some(Field::harness("00112233445566778899aabbccddeeff".into())),
        harness: Some(Field::harness("claude".into())),
        account: Some(Field::harness("anthropic".into())),
        parent_session_id: Some(Field::harness("fedcba9876543210fedcba9876543210".into())),
        branch: Some(Field::harness("feature/x".into())),
        head: Some(Field::harness("38aaabd8".into())),
        worktree: Some(Field::harness("/home/someone/app".into())),
        dirty: Some(Field::harness(true)),
        phase: Some(Field::agent("build".into())),
        plan: Some(Field::agent(vec![PlanEntry { content: "write tests".into(), priority: PlanPriority::High, status: PlanStatus::InProgress }])),
        pr: Some(Field::agent("https://github.com/acme/app/pull/7".into())),
        checks: Some(Field::harness(Vec::new())),
        cost: Some(Field::harness(1.25)),
        last_activity: Some(Field::harness("2026-11-23T11:59:30Z".into())),
    };
    t
}

/// R-STA-1: the JSON is A2A v1.0's Task in ProtoJSON (camelCase fields,
/// `TASK_STATE_*` and `ROLE_*` enum names), gruntas's fields under the
/// extension URI, each `{value, source}`.
#[test]
fn r_sta_1_a_task_is_an_a2a_task_with_the_gruntas_extension() {
    let v = serde_json::to_value(full_task()).unwrap();
    assert_eq!(v["id"], "0123456789abcdef0123456789abcdef");
    assert_eq!(v["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    assert_eq!(v["status"]["timestamp"], "2026-11-23T11:59:00Z");
    assert_eq!(v["status"]["message"], json!({ "messageId": "0123456789abcdef0123456789abcdef-status", "role": "ROLE_AGENT", "parts": [{ "text": "building the board" }] }));
    assert_eq!(v["history"][0]["metadata"][status::EXTENSION_URI]["state"], "TASK_STATE_WORKING");
    let ext = &v["metadata"][status::EXTENSION_URI];
    assert_eq!(v["metadata"].as_object().unwrap().len(), 1, "{v}");
    assert_eq!(ext["stateSource"], "harness");
    assert_eq!(ext["health"], json!({ "value": "SERVING", "source": "harness" }));
    assert_eq!(ext["project"], json!({ "value": "github.com/acme/app", "source": "harness" }));
    assert_eq!(ext["phase"], json!({ "value": "build", "source": "agent" }));
    assert_eq!(ext["plan"]["value"][0], json!({ "content": "write tests", "priority": "high", "status": "in_progress" }));
    assert_eq!(ext["checks"], json!({ "value": [], "source": "harness" }));
    let fields: Vec<&str> = ext.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(fields, ["stateSource", "health", "project", "machine", "harness", "account", "parentSessionId", "branch", "head", "worktree", "dirty", "phase", "plan", "pr", "checks", "cost", "lastActivity"]);
    // Every gruntas field but the state's own source carries a source.
    for (k, f) in ext.as_object().unwrap().iter().filter(|(k, _)| *k != "stateSource") {
        assert!(matches!(f["source"].as_str(), Some("agent" | "harness")), "{k}: {f}");
    }
    assert_eq!(serde_json::from_value::<Task>(v).unwrap(), full_task());
}

/// R-STA-1: a reader built before a state or field was added still reads
/// the Task: an unknown state is unspecified, an unknown field ignored,
/// and a Task with no gruntas fields at all reads with none.
#[test]
fn r_sta_1_a_task_from_a_newer_writer_still_reads() {
    let v = json!({
        "id": "s1",
        "status": { "state": "TASK_STATE_SOMETHING_NEW", "timestamp": PUBLISHED },
        "artifacts": [],
        "metadata": { status::EXTENSION_URI: { "health": { "value": "DRAINING", "source": "harness" }, "newField": 1 }, "https://example.com/other": {} }
    });
    let t: Task = serde_json::from_value(v).unwrap();
    assert_eq!(t.status.state, TaskState::Unspecified);
    assert_eq!(t.metadata.gruntas.health, Some(Field::harness(Health::Unknown)));
    let bare: Task = serde_json::from_value(json!({ "id": "s2", "status": { "state": "TASK_STATE_WORKING", "timestamp": PUBLISHED }, "metadata": { status::EXTENSION_URI: {} } })).unwrap();
    assert_eq!(bare.metadata.gruntas, Extension::default());
}

fn defs(name: &str) -> Value {
    let (_, text) = gruntas_harness_schema(name);
    serde_json::from_str(&text).unwrap()
}

/// The schema the harness checks in, generated from the same types.
fn gruntas_harness_schema(name: &str) -> (String, String) {
    let path = format!("{}/../gruntas-harness/schema/{name}", env!("CARGO_MANIFEST_DIR"));
    (path.clone(), std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

/// R-STA-1: the published JSON Schema names A2A's nine states and two
/// provenances, puts the extension under its URI, and requires what A2A
/// requires of a Task and what a board must have.
#[test]
fn r_sta_1_the_published_schema_is_the_task_and_board_contract() {
    let task = defs("status-task.schema.json");
    assert_eq!(task["required"], json!(["id", "status", "metadata"]));
    let mut states: Vec<String> = serde_json::from_value(task["$defs"]["TaskState"]["oneOf"].as_array().map_or_else(|| task["$defs"]["TaskState"]["enum"].clone(), |o| Value::Array(o.iter().map(|v| v["const"].clone()).collect()))).unwrap();
    states.sort();
    assert_eq!(states, ["TASK_STATE_AUTH_REQUIRED", "TASK_STATE_CANCELED", "TASK_STATE_COMPLETED", "TASK_STATE_FAILED", "TASK_STATE_INPUT_REQUIRED", "TASK_STATE_REJECTED", "TASK_STATE_SUBMITTED", "TASK_STATE_UNSPECIFIED", "TASK_STATE_WORKING"]);
    assert_eq!(task["$defs"]["TaskMetadata"]["required"], json!([status::EXTENSION_URI]));
    let provenance = &task["$defs"]["Provenance"];
    assert_eq!((&provenance["oneOf"][0]["enum"], &provenance["oneOf"][1]["const"]), (&json!(["agent", "harness"]), &json!("unknown")), "{provenance}");
    let ext = &task["$defs"]["Extension"]["properties"];
    for field in ["project", "machine", "harness", "account", "parentSessionId", "branch", "head", "worktree", "dirty", "phase", "plan", "pr", "checks", "cost", "lastActivity", "health"] {
        assert!(ext.get(field).is_some(), "the schema's extension has no {field}");
    }
    let board = defs("status-board.schema.json");
    assert_eq!(board["required"], json!(["device", "publishedAt"]));
    for field in ["sessions", "tasks"] {
        assert!(board["properties"].get(field).is_some(), "the board schema has no {field}");
    }
    let counts = defs("status-counts.schema.json");
    let mut fields: Vec<&String> = counts["properties"].as_object().unwrap().keys().collect();
    fields.sort();
    assert_eq!(fields, ["health", "health_counts", "states"]);
}

struct K {
    key: DeviceKey,
    signing: SigningKey,
    name: &'static str,
    kind: Kind,
}

impl K {
    fn device(n: u8, name: &'static str) -> K {
        K { key: DeviceKey::from_secret(&[n; 32]).unwrap(), signing: SigningKey::from_secret(&[n + 100; 32]).unwrap(), name, kind: Kind::Device }
    }

    fn id(&self) -> DeviceId {
        self.key.id()
    }

    fn subject(&self) -> Subject {
        Subject { kind: self.kind, name: self.name.into(), os: if self.kind == Kind::Device { "linux".into() } else { String::new() }, device: self.key.public(), signing: self.signing.public() }
    }
}

fn keys() -> UserKeys {
    UserKeys::new(UserKey::from_bytes(1, [1; 32]).unwrap(), []).unwrap()
}

fn sign(e: &Entry, by: &[&K]) -> SignedEntry {
    e.sign(&by.iter().map(|k| (k.key.id(), &k.signing)).collect::<Vec<_>>()).unwrap()
}

/// The laptop and the recovery kit, then the phone.
fn world() -> (K, K, K, Chain) {
    let (laptop, phone) = (K::device(1, "laptop"), K::device(2, "phone"));
    let d = RecoveryKit::from_bytes([1; 16]).device();
    let kit = K { key: d.key, signing: d.signing, name: "recovery kit", kind: Kind::Recovery };
    let key_id = UserKey::from_bytes(1, [1; 32]).unwrap().id();
    let g = sign(&Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects: vec![laptop.subject(), kit.subject()], generation: 1, key_id, time: T, signers: Vec::new() }, &[&laptop, &kit]);
    let p = sign(&Entry { seq: 1, prev: g.hash(), action: Action::Add, subjects: vec![phone.subject()], generation: 1, key_id, time: T + 1, signers: Vec::new() }, &[&laptop]);
    let chain = Chain::verify(&[g, p], None).unwrap();
    (laptop, phone, kit, chain)
}

fn board(device: &DeviceId, tasks: Vec<Task>) -> Board {
    Board { device: device.to_string(), published_at: PUBLISHED.into(), sessions: tasks.iter().map(|t| t.id.clone()).collect(), tasks }
}

/// R-STA-5: a board sealed on one device opens on another as it was, and
/// the sealed bytes name nothing it lists.
#[test]
fn r_sta_5_a_board_sealed_on_one_device_opens_on_another() {
    let (laptop, _phone, _kit, chain) = world();
    let b = board(&laptop.id(), vec![full_task()]);
    let sealed = status::seal_board(&b, &keys(), chain.generation(), &laptop.id()).unwrap();
    assert_eq!(sealed.len() % 1024, gruntas_client::e2e::PERSON_OVERHEAD % 1024, "padded to whole kilobytes");
    for secret in ["github.com/acme/app", "building the board", "feature/x", "/home/someone/app"] {
        assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} is in the sealed bytes");
    }
    assert_eq!(status::open_board(&sealed, &laptop.id(), &keys(), &chain).unwrap(), b);
}

/// R-STA-5: what does not open — a board moved to another device, one
/// from the recovery kit or off the list, one about another device, and
/// one with a control character a reader would print.
#[test]
fn r_sta_5_a_board_that_is_not_the_devices_own_does_not_open() {
    let (laptop, phone, kit, chain) = world();
    let sealed = status::seal_board(&board(&laptop.id(), vec![full_task()]), &keys(), 1, &laptop.id()).unwrap();
    assert!(status::open_board(&sealed, &phone.id(), &keys(), &chain).is_err(), "bound to the laptop");
    assert!(status::open_board(&sealed, &kit.id(), &keys(), &chain).unwrap_err().contains("recovery kit"));
    let stranger = K::device(9, "stranger");
    assert!(status::open_board(&sealed, &stranger.id(), &keys(), &chain).unwrap_err().contains("not on your device list"));
    assert!(status::seal_board(&board(&phone.id(), vec![]), &keys(), 1, &laptop.id()).unwrap_err().contains("not this one"));
    // A crafted board with a control character in it, sealed past
    // `seal_board`'s sanitising, does not open: the views print it.
    let mut t = full_task();
    t.status.message = Some(Message::text("m", "done\u{1b}[2J"));
    let mut json = serde_json::to_vec(&board(&laptop.id(), vec![t])).unwrap();
    json.resize(json.len().div_ceil(status::PAD) * status::PAD, b' ');
    let crafted = gruntas_client::e2e::seal_to_person(&keys(), 1, gruntas_client::e2e::Label::StatusBoard, &laptop.id().0, &json).unwrap();
    assert!(status::open_board(&crafted, &laptop.id(), &keys(), &chain).unwrap_err().contains("control character"));
    let mut many = full_task();
    many.history = vec![Message::text("r", "x"); status::MAX_HISTORY + 1];
    assert!(status::seal_board(&board(&laptop.id(), vec![many]), &keys(), 1, &laptop.id()).is_err());
}

/// R-STA-5: what a reader would refuse is made harmless on seal rather
/// than refusing the machine's whole board: a newline or escape in a
/// summary becomes a space, and a cost that is not a number is dropped.
#[test]
fn r_sta_5_a_newline_or_a_nan_cost_does_not_stop_the_board() {
    let (laptop, _phone, _kit, chain) = world();
    let mut t = full_task();
    t.status.message = Some(Message::text("m", "tests green\nopening the PR\u{1b}[2J 👨\u{200d}💻"));
    t.metadata.gruntas.cost = Some(Field::harness(f64::NAN));
    let sealed = status::seal_board(&board(&laptop.id(), vec![t]), &keys(), 1, &laptop.id()).unwrap();
    let opened = status::open_board(&sealed, &laptop.id(), &keys(), &chain).unwrap();
    assert_eq!(opened.tasks[0].status.message.as_ref().unwrap().summary(), "tests green opening the PR [2J 👨 💻");
    assert_eq!(opened.tasks[0].metadata.gruntas.cost, None);
    assert_eq!(opened.tasks[0].metadata.gruntas.project, full_task().metadata.gruntas.project);
}

/// The board's seal label, byte for byte: boards already sealed open only
/// under it.
#[test]
fn r_sta_5_the_board_label_is_pinned() {
    assert_eq!(gruntas_client::e2e::LABEL_STATUS_BOARD_V1, b"gruntas/status-board/v1");
    assert_eq!(gruntas_client::e2e::LABEL_STATUS_BOARD_V1_KEY, b"gruntas/status-board/v1-key");
}

/// R-STA-1: a plan or provenance value from a newer writer reads as
/// unknown rather than refusing the board.
#[test]
fn r_sta_1_unknown_plan_and_provenance_values_read_as_unknown() {
    let t: Task = serde_json::from_value(json!({
        "id": "s",
        "status": { "state": "TASK_STATE_WORKING", "timestamp": PUBLISHED },
        "metadata": { status::EXTENSION_URI: {
            "phase": { "value": "x", "source": "registry" },
            "plan": { "value": [{ "content": "c", "priority": "urgent", "status": "cancelled" }], "source": "agent" }
        } }
    })).unwrap();
    assert_eq!(t.metadata.gruntas.phase.unwrap().source, Provenance::Unknown);
    let plan = t.metadata.gruntas.plan.unwrap().value;
    assert_eq!((plan[0].priority, plan[0].status), (PlanPriority::Unknown, PlanStatus::Unknown));
}

/// A Task of about `bytes` bytes: its summary, and `reports` reports of
/// `bytes / 10` each in its history.
fn heavy(id: &str, state: TaskState, at: &str, reports: usize) -> Task {
    let mut t = task(id, state, at);
    t.status.message = Some(Message::text("m", "s".repeat(1500)));
    t.history = (0..reports).map(|i| Message { metadata: Some(MessageMetadata { report: Report { timestamp: at.into(), ..Report::default() } }), ..Message::text(format!("r{i}"), format!("{i}:{}", "h".repeat(1500))) }).collect();
    t
}

fn ids(b: &Board) -> Vec<&str> {
    b.tasks.iter().map(|t| t.id.as_str()).collect()
}

/// D8: a board within budget is left as it is.
#[test]
fn d8_a_board_within_budget_is_not_trimmed() {
    let (laptop, ..) = world();
    let mut b = board(&laptop.id(), vec![heavy("old-done", TaskState::Completed, "2026-11-23T09:00:00Z", 10), heavy("live", TaskState::Working, PUBLISHED, 10)]);
    let before = b.clone();
    b.fit().unwrap();
    assert_eq!(b, before);
}

/// D8, first: over budget, finished Tasks older than an hour go — and
/// nothing else when that is enough. A recent finished one, and old live
/// ones, stay; registered session ids all stay.
#[test]
fn d8_over_budget_old_finished_tasks_go_first() {
    let (laptop, ..) = world();
    let mut tasks: Vec<Task> = (0..16).map(|i| heavy(&format!("done-{i}"), [TaskState::Completed, TaskState::Failed, TaskState::Canceled, TaskState::Rejected][i % 4], "2026-11-23T10:30:00Z", 10)).collect();
    tasks.push(heavy("just-done", TaskState::Completed, "2026-11-23T11:30:00Z", 10));
    tasks.push(heavy("old-working", TaskState::Working, "2026-11-23T08:00:00Z", 10));
    tasks.push(heavy("waiting", TaskState::InputRequired, "2026-11-23T08:00:00Z", 10));
    let mut b = board(&laptop.id(), tasks);
    assert!(b.sealed_len() > status::MAX_SEALED, "{}", b.sealed_len());
    let sessions = b.sessions.clone();
    b.fit().unwrap();
    assert_eq!(ids(&b), ["just-done", "old-working", "waiting"]);
    assert!(b.tasks.iter().all(|t| t.history.len() == 10), "history untouched when dropping was enough");
    assert_eq!(b.sessions, sessions);
}

/// D8, second: still over, every Task keeps only its last 3 reports, the
/// newest; summaries untouched when that is enough.
#[test]
fn d8_then_history_beyond_the_last_three_reports() {
    let (laptop, ..) = world();
    let tasks: Vec<Task> = (0..20).map(|i| heavy(&format!("live-{i}"), TaskState::Working, PUBLISHED, 10)).collect();
    let mut b = board(&laptop.id(), tasks);
    assert!(b.sealed_len() > status::MAX_SEALED);
    b.fit().unwrap();
    assert_eq!(b.tasks.len(), 20);
    for t in &b.tasks {
        let kept: Vec<&str> = t.history.iter().map(|m| m.message_id.as_str()).collect();
        assert_eq!(kept, ["r7", "r8", "r9"]);
        assert_eq!(t.status.message.as_ref().unwrap().summary().len(), 1500);
    }
}

/// D8, last: still over, summaries are shortened — the status's and each
/// report's — to `SHORT_SUMMARY` characters with an ellipsis; and a board
/// over even then is refused rather than sealed.
#[test]
fn d8_then_summaries_are_shortened_and_beyond_that_it_is_refused() {
    let (laptop, ..) = world();
    let tasks: Vec<Task> = (0..60).map(|i| heavy(&format!("live-{i}"), TaskState::Working, PUBLISHED, 3)).collect();
    let mut b = board(&laptop.id(), tasks);
    assert!(b.sealed_len() > status::MAX_SEALED);
    b.fit().unwrap();
    assert_eq!(b.tasks.len(), 60);
    for t in &b.tasks {
        let s = t.status.message.as_ref().unwrap().summary();
        assert_eq!(s.chars().count(), status::SHORT_SUMMARY);
        assert!(s.ends_with('…'));
        assert!(t.history.iter().all(|m| m.summary().chars().count() == status::SHORT_SUMMARY));
        assert_eq!(t.history.len(), 3);
    }
    let mut huge = full_task();
    huge.metadata.gruntas.worktree = Some(Field::harness("w".repeat(300 * 1024)));
    let mut over = board(&laptop.id(), vec![huge]);
    assert!(over.fit().unwrap_err().contains("more than"));
    assert!(status::seal_board(&over, &keys(), 1, &laptop.id()).is_err());
}

/// D9: the counts tally a board's Tasks per A2A state and per health
/// (unknown when none was observed), the machine serving; the JSON has
/// those three fields and nothing from the board's content.
#[test]
fn d9_counts_carry_health_and_tallies_only() {
    let (laptop, ..) = world();
    let mut gone = task("c", TaskState::Canceled, PUBLISHED);
    gone.metadata.gruntas.health = Some(Field::harness(Health::NotServing));
    let mut live = full_task();
    live.status.state = TaskState::Working;
    let b = board(&laptop.id(), vec![live, task("b", TaskState::Working, PUBLISHED), gone]);
    let c = Counts::of(&b);
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v, json!({ "health": "SERVING", "states": { "TASK_STATE_WORKING": 2, "TASK_STATE_CANCELED": 1 }, "health_counts": { "SERVING": 1, "NOT_SERVING": 1, "UNKNOWN": 1 } }));
    let text = v.to_string();
    for secret in ["acme", "building", laptop.id().to_string().as_str()] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    assert_eq!(serde_json::from_value::<Counts>(v).unwrap(), c);
}

/// D9: a statistics id is random, its own form, and never the device id.
#[test]
fn d9_a_statistics_id_is_random_and_not_the_device_id() {
    let (a, b) = (status::statistics_id(), status::statistics_id());
    assert!(status::is_statistics_id(&a), "{a}");
    assert_ne!(a, b);
    let (laptop, ..) = world();
    assert_ne!(a, laptop.id().to_string());
    assert!(!status::is_statistics_id("ABCDEF0123456789abcdef0123456789"));
    assert!(!status::is_statistics_id("../0123456789abcdef0123456789abc"));
}
