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

/// SL05: two devices on the dev registry each read the other's board
/// (listed through presences, fetched and opened per device); a burst of
/// changes publishes at most once per 5 s; an over-budget board is trimmed
/// in D8's order (`Board::fit`); "quiet for" is computed from the board's
/// publish time and its receipt, never across clocks.
#[test]
#[ignore = "SL05: publish and read sealed status boards"]
fn sl05_two_devices_read_each_others_boards_and_publishes_coalesce() {
    todo!("SL05: two paired homes on gruntas-devregistry with daemons up, sessions registered on each; read the other's board via `status_board`; 20 changes in 1 s show at most one new `updated_at` on /_status_layer")
}

/// SL05: each publish sends the counts under the device's random
/// statistics id, and the registry holds nothing else of the board.
#[test]
#[ignore = "SL05: publish and read sealed status boards"]
fn sl05_the_counts_carry_only_the_allowed_fields() {
    todo!("SL05: after a publish, /_status_layer's statistics row has exactly id, health, states, health_counts, updated_at, and its id is not the device id")
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
