//! KT01: the registration and activity frame pair round-trips, and its wire
//! names are the camelCase the rest of the socket speaks.

use gruntas_client::protocol::{Activity, Attendance, ClientFrame, ErrorInfo, PluginHarness, Registration, ServerFrame, UsageWindow};
use serde_json::json;

fn round_trip_client(f: ClientFrame) -> serde_json::Value {
    let v = serde_json::to_value(&f).unwrap();
    assert_eq!(serde_json::from_value::<ClientFrame>(v.clone()).unwrap(), f);
    v
}

#[test]
fn kt01_every_activity_round_trips_inside_its_frame() {
    let full = Registration {
        session_id: "s1".into(),
        harness: PluginHarness::Claude,
        harness_pid: Some(42),
        config_home: Some("/h/.claude".into()),
        cwd: Some("/r".into()),
        repository: Some("git@github.com:o/r.git".into()),
        transcript_path: Some("/h/.claude/projects/r/s1.jsonl".into()),
        attendance: Some(Attendance::Unattended),
    };
    let v = round_trip_client(ClientFrame::Activity { id: 1, activity: Activity::Register(full) });
    assert_eq!(
        v,
        json!({"type": "activity", "id": 1, "activity": {"type": "register", "sessionId": "s1", "harness": "claude", "harnessPid": 42, "configHome": "/h/.claude", "cwd": "/r", "repository": "git@github.com:o/r.git", "transcriptPath": "/h/.claude/projects/r/s1.jsonl", "attendance": "unattended"}})
    );
    for activity in [
        Activity::Deregister { session_id: "s1".into() },
        Activity::HeavyStart { session_id: "s1".into(), tool_call_id: "t1".into() },
        Activity::HeavyEnd { session_id: "s1".into(), tool_call_id: "t1".into() },
        Activity::Usage { config_home: "/h/.claude".into(), windows: vec![UsageWindow { kind: "fiveHour".into(), used_percent: 37.5, resets_at_ms: Some(1_800_000_000_000) }, UsageWindow { kind: "sevenDay".into(), used_percent: 2.0, resets_at_ms: None }] },
    ] {
        round_trip_client(ClientFrame::Activity { id: 2, activity });
    }
    let v = round_trip_client(ClientFrame::Activity { id: 3, activity: Activity::HeavyStart { session_id: "s".into(), tool_call_id: "t".into() } });
    assert_eq!(v["activity"], json!({"type": "heavyStart", "sessionId": "s", "toolCallId": "t"}));
}

#[test]
fn kt01_optional_registration_fields_may_be_left_out() {
    let f: ClientFrame = serde_json::from_value(json!({"type": "activity", "id": 9, "activity": {"type": "register", "sessionId": "s", "harness": "codex"}})).unwrap();
    let ClientFrame::Activity { activity: Activity::Register(r), .. } = f else { panic!("not a register: {f:?}") };
    assert_eq!((r.harness, r.harness_pid, r.cwd, r.attendance), (PluginHarness::Codex, None, None, None));
    // Absent fields stay absent on the way out.
    assert_eq!(serde_json::to_value(Activity::Register(Registration { session_id: "s".into(), harness: PluginHarness::Codex, harness_pid: None, config_home: None, cwd: None, repository: None, transcript_path: None, attendance: None })).unwrap(), json!({"type": "register", "sessionId": "s", "harness": "codex"}));
}

#[test]
fn kt01_the_answer_round_trips_with_and_without_an_error() {
    for f in [ServerFrame::Activity { id: 1, error: None }, ServerFrame::Activity { id: 2, error: Some(ErrorInfo { code: "local_only".into(), message: "m".into(), http_status: None, resets_at_ms: None }) }] {
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "activity");
        assert_eq!(serde_json::from_value::<ServerFrame>(v).unwrap(), f);
    }
    assert_eq!(serde_json::to_value(ServerFrame::Activity { id: 1, error: None }).unwrap(), json!({"type": "activity", "id": 1}));
}
