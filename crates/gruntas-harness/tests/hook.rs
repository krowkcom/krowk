//! KT01: `gruntas hook`'s normalisation, on the payloads Claude Code and
//! Codex send (`fixtures/hooks/`), and the activity frame it sends, against
//! a daemon and against one too old to know it.

#![cfg(unix)]

#[path = "common/scratch.rs"]
mod scratch;

use gruntas_harness::hook::{self, Context, HookEvent, Session};
use gruntas_harness::protocol::{Activity, Attendance, PluginHarness, Registration, UsageWindow};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

fn fixture(harness: &str, event: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks").join(harness).join(format!("{event}.json"));
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

fn normalised(h: PluginHarness, event: &str) -> HookEvent {
    let word = if h == PluginHarness::Claude { "claude" } else { "codex" };
    hook::normalise(h, event, &fixture(word, event)).unwrap().unwrap_or_else(|| panic!("{word} {event} normalised to nothing"))
}

const CLAUDE: &str = "2965e294-a0d4-4f50-9a48-0c9b4d0aca11";
const CODEX: &str = "019a8f3e-6c1d-7b42-9e0a-3f5d2c7b8a91";

fn claude_session() -> Session {
    Session { id: CLAUDE.into(), cwd: Some("/home/user/repo".into()), transcript_path: Some(format!("/home/user/.claude/projects/-home-user-repo/{CLAUDE}.jsonl")) }
}

fn codex_session(transcript: bool) -> Session {
    let t = format!("/home/user/.codex/sessions/2026/11/23/rollout-2026-11-23T07-40-00-{CODEX}.jsonl");
    Session { id: CODEX.into(), cwd: Some("/home/user/repo".into()), transcript_path: transcript.then_some(t) }
}

#[test]
fn kt01_claude_codes_payloads_normalise_to_one_event_each() {
    let c = PluginHarness::Claude;
    assert_eq!(normalised(c, "SessionStart"), HookEvent::SessionStart { session: claude_session(), source: Some("startup".into()) });
    assert_eq!(normalised(c, "SessionEnd"), HookEvent::SessionEnd { session: claude_session() });
    assert_eq!(normalised(c, "PreToolUse"), HookEvent::ToolStart { session: claude_session(), tool_call_id: "toolu_01HTh5vFPUMgCcLz8rj6s46g".into(), tool_name: "Bash".into(), input: serde_json::json!({"command": "echo hi", "description": "Print hi via echo"}) });
    assert_eq!(normalised(c, "PostToolUse"), HookEvent::ToolEnd { session: claude_session(), tool_call_id: "toolu_01HTh5vFPUMgCcLz8rj6s46g".into(), tool_name: "Bash".into(), input: serde_json::json!({"command": "echo hi", "description": "Print hi via echo"}) });
    assert_eq!(normalised(c, "Stop"), HookEvent::Stop { session: claude_session(), active: false });
    assert_eq!(
        normalised(c, "usage"),
        HookEvent::Usage { config_home: "/home/user/.claude".into(), windows: vec![UsageWindow { kind: "fiveHour".into(), used_percent: 37.5, resets_at_ms: Some(1_795_430_000_000) }, UsageWindow { kind: "sevenDay".into(), used_percent: 12.0, resets_at_ms: Some(1_795_900_000_000) }] }
    );
}

#[test]
fn kt01_codexs_payloads_normalise_to_the_same_events() {
    let x = PluginHarness::Codex;
    assert_eq!(normalised(x, "SessionStart"), HookEvent::SessionStart { session: codex_session(true), source: Some("startup".into()) });
    // Codex may send a null transcript path.
    assert_eq!(normalised(x, "SessionEnd"), HookEvent::SessionEnd { session: codex_session(false) });
    assert_eq!(normalised(x, "PreToolUse"), HookEvent::ToolStart { session: codex_session(true), tool_call_id: "call_Xq3mB7rT2kLp9vNw".into(), tool_name: "Bash".into(), input: serde_json::json!({"command": "cargo test"}) });
    assert_eq!(normalised(x, "PostToolUse"), HookEvent::ToolEnd { session: codex_session(true), tool_call_id: "call_Xq3mB7rT2kLp9vNw".into(), tool_name: "Bash".into(), input: serde_json::json!({"command": "cargo test"}) });
    assert_eq!(normalised(x, "Stop"), HookEvent::Stop { session: codex_session(true), active: false });
}

#[test]
fn kt01_a_payload_missing_its_session_or_tool_call_is_an_error_not_a_guess() {
    assert!(hook::normalise(PluginHarness::Claude, "SessionStart", &serde_json::json!({"cwd": "/r"})).is_err());
    assert!(hook::normalise(PluginHarness::Codex, "PreToolUse", &serde_json::json!({"session_id": "s", "cwd": "/r"})).is_err());
}

#[test]
fn kt01_session_start_registers_and_session_end_deregisters() {
    let ctx = Context { harness_pid: Some(7), config_home: Some("/home/user/.claude".into()), repository: Some("git@github.com:o/r.git".into()), attendance: Some(Attendance::Attended), heavy: gruntas_harness::builds::BuildsConfig::default().heavy_list() };
    let c = PluginHarness::Claude;
    assert_eq!(
        hook::to_activity(c, &normalised(c, "SessionStart"), &ctx),
        Some(Activity::Register(Registration { session_id: CLAUDE.into(), harness: c, harness_pid: Some(7), config_home: Some("/home/user/.claude".into()), cwd: Some("/home/user/repo".into()), repository: Some("git@github.com:o/r.git".into()), transcript_path: claude_session().transcript_path, attendance: Some(Attendance::Attended) }))
    );
    assert_eq!(hook::to_activity(PluginHarness::Codex, &normalised(PluginHarness::Codex, "SessionEnd"), &ctx), Some(Activity::Deregister { session_id: CODEX.into() }));
    // A tool call off the heavy list (`echo hi`) tells the daemon nothing
    // as it starts; its end is work the agent has not reported (SL03).
    assert_eq!(hook::to_activity(c, &normalised(c, "PreToolUse"), &ctx), None);
    assert_eq!(hook::to_activity(c, &normalised(c, "PostToolUse"), &ctx), Some(Activity::ToolEnd { session_id: CLAUDE.into() }));
}

/// SL03: the report tool's own call is the report, not work to report.
#[test]
fn sl03_the_report_tools_own_call_is_not_unreported_work() {
    let c = PluginHarness::Claude;
    for name in ["mcp__plugin_gruntas_gruntas__gruntas_report", "gruntas_report", "gruntas.gruntas_report", "gruntas/gruntas_report"] {
        let ev = HookEvent::ToolEnd { session: claude_session(), tool_call_id: "t".into(), tool_name: name.into(), input: serde_json::json!({ "summary": "s" }) };
        assert_eq!(hook::to_activity(c, &ev, &Context::default()), None, "{name}");
    }
    for name in ["mcp__other__not_gruntas_report_x", "mcp__other__my_gruntas_report"] {
        let ev = HookEvent::ToolEnd { session: claude_session(), tool_call_id: "t".into(), tool_name: name.into(), input: serde_json::json!({}) };
        assert_eq!(hook::to_activity(c, &ev, &Context::default()), Some(Activity::ToolEnd { session_id: CLAUDE.into() }), "{name}");
    }
    let mut active: serde_json::Value = serde_json::from_slice(&std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks/claude/Stop.json")).unwrap()).unwrap();
    active["stop_hook_active"] = true.into();
    assert_eq!(hook::normalise(c, "Stop", &active).unwrap(), Some(HookEvent::Stop { session: claude_session(), active: true }));
}

/// KT03: a heavy command's start and end, and the turn's end, as the
/// daemon is told them; the list is config.json's when it sets one.
#[test]
fn kt03_heavy_tool_calls_and_the_turns_end_are_told() {
    let x = PluginHarness::Codex;
    let ctx = Context { heavy: gruntas_harness::builds::BuildsConfig::default().heavy_list(), ..Context::default() };
    let call = "call_Xq3mB7rT2kLp9vNw".to_string();
    assert_eq!(hook::to_activity(x, &normalised(x, "PreToolUse"), &ctx), Some(Activity::HeavyStart { session_id: CODEX.into(), tool_call_id: call.clone() }));
    assert_eq!(hook::to_activity(x, &normalised(x, "PostToolUse"), &ctx), Some(Activity::HeavyEnd { session_id: CODEX.into(), tool_call_id: call }));
    assert_eq!(hook::to_activity(x, &normalised(x, "Stop"), &ctx), Some(Activity::TurnEnd { session_id: CODEX.into() }));
    // `cargo test` is not on a list that names only `make`.
    let make_only = Context { heavy: vec!["make".into()], ..Context::default() };
    assert_eq!(hook::to_activity(x, &normalised(x, "PreToolUse"), &make_only), None);
    // A tool without a command is never heavy.
    let read = HookEvent::ToolStart { session: codex_session(true), tool_call_id: "t".into(), tool_name: "Read".into(), input: serde_json::json!({"file_path": "cargo test"}) };
    assert_eq!(hook::to_activity(x, &read, &ctx), None);

    let root = scratch::root("hook-heavy-list");
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".gruntas")).unwrap();
    let env = |k: &str| if k == "HOME" { home.display().to_string() } else { String::new() };
    assert!(hook::heavy_list(&env).contains(&"cargo test".to_string()), "the default with no config");
    std::fs::write(home.join(".gruntas/config.json"), r#"{"builds": {"heavy": ["just ci"]}}"#).unwrap();
    assert_eq!(hook::heavy_list(&env), ["just ci"]);
    std::fs::write(home.join(".gruntas/config.json"), r#"{"builds": {"heavy": "make"}}"#).unwrap();
    assert!(hook::heavy_list(&env).contains(&"make".to_string()), "a list that is not one is the default");
    let _ = std::fs::remove_dir_all(&root);
}

/// A daemon too old to know `activity`: it welcomes, skips the line it
/// cannot read, and closes once the client has.
fn old_daemon(socket: &Path) -> std::thread::JoinHandle<()> {
    let l = std::os::unix::net::UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        let mut w = s.try_clone().unwrap();
        w.write_all(b"{\"type\":\"welcome\",\"protocolVersion\":3,\"krowkVersion\":\"old\",\"pid\":1,\"epoch\":1}\n").unwrap();
        for _ in BufReader::new(s).lines() {}
    })
}

#[test]
fn kt01_a_daemon_that_does_not_know_the_frame_is_given_up_on_at_once() {
    let root = scratch::root("hook-old");
    let socket = root.join("s.sock");
    let t = old_daemon(&socket);
    let at = Instant::now();
    let e = hook::send(&socket, "/r", "t", Activity::Deregister { session_id: "s".into() }).unwrap_err();
    assert!(at.elapsed() < Duration::from_millis(500), "{:?}", at.elapsed());
    assert!(e.contains("closed without answering"), "{e}");
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn kt01_no_daemon_is_an_error_at_once() {
    let root = scratch::root("hook-none");
    let at = Instant::now();
    assert!(hook::send(&root.join("s.sock"), "/r", "t", Activity::Deregister { session_id: "s".into() }).is_err());
    assert!(at.elapsed() < Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&root);
}
