//! R-PROTO-2, R-PROTO-1: the protocol reaches Swift as krowk-client's own
//! types (src/protocol.rs re-declares them with `uniffi::remote`), so a
//! line the daemon writes decodes into the very value it was made from,
//! through the JSON edge and through UniFFI's buffer alike, and a command
//! encodes into the daemon's wire form.

use krowk_client::protocol::{
    ApprovalDecision, ApprovalRequest, BackendAgent, Billing, BudgetLimits, ChildState, ChildTool, Command, Delta,
    Effort, ErrorInfo, HandoffKind, ImageInput, ImageRef, Item, ItemKind, KeptWorktree, LimitState, LimitStatus,
    LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, ProviderBlob, Question, QuestionAnswer, QuestionOption,
    RanBy, RunResult, StreamLine, SwitchOffer, SwitchReason, Todo, TodoStatus, TurnStatus, Usage, Waiting, WireApi,
};
use krowk_ffi::{KrowkError, UniFfiTag, decode_log_event, decode_stream_line, encode_command};
use serde_json::json;
use uniffi::{Lift, Lower};

const SESSION: &str = "0192f0a0-0000-7000-8000-000000000001";
const TURN: &str = "0192f0a0-0000-7000-8000-000000000002";

fn model() -> ModelRef {
    ModelRef { instance: "anthropic".into(), model: "claude-opus-5-5".into() }
}

fn usage() -> Usage {
    Usage { input_tokens: 10, output_tokens: 20, cache_read_tokens: 3, cache_write_tokens: 4, reasoning_tokens: 5 }
}

fn error() -> ErrorInfo {
    ErrorInfo { code: "rate_limited".into(), message: "slow down".into(), http_status: Some(429), resets_at_ms: Some(1_700_000_000_000) }
}

/// Which `LogBody` variant this is; exhaustive, so a new variant fails to
/// compile here until `log_bodies` builds one.
fn log_body_index(body: &LogBody) -> usize {
    match body {
        LogBody::SessionStarted { .. } => 0,
        LogBody::TurnStarted { .. } => 1,
        LogBody::ItemCompleted { .. } => 2,
        LogBody::ResponseCompleted { .. } => 3,
        LogBody::BackendSession { .. } => 4,
        LogBody::SubagentResponse { .. } => 5,
        LogBody::SubagentStarted { .. } => 6,
        LogBody::TodosUpdated { .. } => 7,
        LogBody::RunOpened { .. } => 8,
        LogBody::ModelSwitched { .. } => 9,
        LogBody::BackendHandoff { .. } => 10,
        LogBody::SessionMoved { .. } => 11,
        LogBody::TurnCompleted { .. } => 12,
    }
}
const LOG_BODIES: usize = 13;

/// One of every `LogBody` variant, every `Item` among them, and every
/// optional field filled where it has one.
fn log_bodies() -> Vec<LogBody> {
    let turn_id = || TURN.to_string();
    let items = vec![
        Item::UserText {
            text: "look at this".into(),
            images: vec![ImageRef { number: 1, media_type: "image/png".into(), file: "img/1.png".into() }],
        },
        Item::AssistantText { text: "done".into() },
        Item::Reasoning {
            text: "thinking".into(),
            blob: Some(ProviderBlob {
                provider: "anthropic".into(),
                wire_api: WireApi::AnthropicMessages,
                data: json!({"signature": "abc", "n": [1, 2.5, null, true]}),
            }),
        },
        Item::ToolCall { call_id: "call_1".into(), name: "bash".into(), input: json!({"command": "ls -la"}) },
        Item::ToolResult { call_id: "call_1".into(), output: "a\nb".into(), is_error: false },
    ];
    let mut bodies = vec![
        LogBody::SessionStarted {
            cwd: "/home/me/repo".into(),
            krowk_version: "0.9.0".into(),
            protocol_version: 1,
            parent_session_id: Some(SESSION.into()),
            agent: Some("reviewer".into()),
        },
        LogBody::TurnStarted {
            turn_id: turn_id(),
            model: model(),
            provider: "anthropic".into(),
            wire_api: WireApi::CodexAppServer,
            permission_mode: PermissionMode::AcceptEdits,
            effort: Some(Effort::Xhigh),
        },
        LogBody::ResponseCompleted {
            turn_id: turn_id(),
            response_id: Some("resp_1".into()),
            model: "claude-opus-5-5".into(),
            usage: usage(),
            stop_reason: Some("end_turn".into()),
            item_ids: vec!["i1".into(), "i2".into()],
        },
        LogBody::BackendSession {
            turn_id: turn_id(),
            backend: "claude".into(),
            vendor_session_id: "v1".into(),
            transcript_path: Some("/tmp/t.jsonl".into()),
            billing: Some(Billing::Subscription),
        },
        LogBody::SubagentResponse { turn_id: turn_id(), response_id: None, model: "m".into(), usage: usage() },
        LogBody::SubagentStarted {
            turn_id: turn_id(),
            call_id: "call_2".into(),
            subagent_session_id: SESSION.into(),
            description: "look around".into(),
            agent: Some("explorer".into()),
            model: model(),
            ran_by: RanBy::Codex,
            backend_id: Some("b1".into()),
        },
        LogBody::TodosUpdated {
            turn_id: turn_id(),
            todos: vec![
                Todo { content: "one".into(), status: TodoStatus::Pending },
                Todo { content: "two".into(), status: TodoStatus::InProgress },
                Todo { content: "three".into(), status: TodoStatus::Completed },
            ],
        },
        LogBody::RunOpened { turn_id: turn_id(), run: "run-1".into() },
        LogBody::ModelSwitched {
            turn_id: Some(turn_id()),
            from: Some(model()),
            to: ModelRef { instance: "openai".into(), model: "gpt-6".into() },
            reason: SwitchReason::RateLimited,
            detail: Some("429".into()),
        },
        LogBody::BackendHandoff {
            turn_id: turn_id(),
            how: HandoffKind::CatchUp,
            from_instance: Some("anthropic".into()),
            summarized_turns: 3,
            recent_turns: 2,
            fell_back: Some("too long".into()),
        },
        LogBody::SessionMoved { cwd: "/srv/repo".into(), device: "laptop".into(), from_device: "desk".into() },
        LogBody::TurnCompleted {
            turn_id: turn_id(),
            status: TurnStatus::Failed,
            usage: usage(),
            duration_ms: 1234,
            reported_cost_usd: Some(0.25),
            reported_tokens: Some(99),
            error: Some(error()),
        },
    ];
    bodies.extend(items.into_iter().map(|item| LogBody::ItemCompleted { turn_id: turn_id(), item_id: "i1".into(), item }));
    bodies
}

/// Which `LiveEvent` variant this is; exhaustive, as `log_body_index`.
fn live_event_index(ev: &LiveEvent) -> usize {
    match ev {
        LiveEvent::ItemStarted { .. } => 0,
        LiveEvent::ItemDelta { .. } => 1,
        LiveEvent::Cost { .. } => 2,
        LiveEvent::Notice { .. } => 3,
        LiveEvent::Limits { .. } => 4,
        LiveEvent::ApprovalRequested(_) => 5,
        LiveEvent::ApprovalResolved { .. } => 6,
        LiveEvent::Result(_) => 7,
        LiveEvent::BackendAgents { .. } => 8,
        LiveEvent::Background { .. } => 9,
        LiveEvent::SubagentStatus { .. } => 10,
        LiveEvent::TurnUnprompted { .. } => 11,
    }
}
const LIVE_EVENTS: usize = 12;

fn run_result(worktree: Option<Box<KeptWorktree>>) -> RunResult {
    RunResult {
        session_id: SESSION.into(),
        turn_id: TURN.into(),
        status: TurnStatus::Completed,
        is_error: false,
        result: "all done".into(),
        model: model(),
        usage: usage(),
        cost_usd: Some(0.5),
        duration_ms: 9_000,
        num_model_calls: 4,
        error: Some(error()),
        unread_steers: vec!["also this".into()],
        switch_offer: Some(SwitchOffer { from: model(), to: model(), resets_at_ms: Some(5) }),
        worktree,
    }
}

/// One of every `LiveEvent` variant, every `ItemKind` and `Delta` among
/// them.
fn live_events() -> Vec<LiveEvent> {
    let (s, t) = (|| SESSION.to_string(), || TURN.to_string());
    let mut events: Vec<LiveEvent> = [
        ItemKind::UserText,
        ItemKind::AssistantText,
        ItemKind::Reasoning,
        ItemKind::ToolCall { call_id: "c".into(), name: "bash".into() },
        ItemKind::ToolResult { call_id: "c".into() },
    ]
    .into_iter()
    .map(|item| LiveEvent::ItemStarted { session_id: s(), turn_id: t(), item_id: "i1".into(), item })
    .collect();
    events.extend(
        [Delta::Text { text: "he".into() }, Delta::ToolInput { partial_json: "{\"comm".into() }]
            .into_iter()
            .map(|delta| LiveEvent::ItemDelta { session_id: s(), turn_id: t(), item_id: "i1".into(), delta }),
    );
    events.extend([
        LiveEvent::Cost { session_id: s(), turn_id: t(), cost_usd: Some(1.5), turn_cost_usd: None, generated_tokens: 42 },
        LiveEvent::Notice { session_id: s(), turn_id: t(), text: "heads up".into() },
        LiveEvent::Limits {
            session_id: s(),
            turn_id: t(),
            instance: "anthropic".into(),
            limit: LimitStatus {
                status: LimitState::Warning,
                window: Some("five_hour".into()),
                used_percent: Some(81.5),
                resets_at_ms: Some(7),
            },
        },
        LiveEvent::ApprovalRequested(ApprovalRequest {
            session_id: s(),
            turn_id: t(),
            request_id: "req_1".into(),
            tool: "bash".into(),
            input: json!({"command": "rm -rf build", "nested": {"a": [1]}}),
            summary: "rm -rf build".into(),
            reason: "deletes files".into(),
            remember: vec!["Bash(rm:*)".into()],
            questions: vec![Question {
                id: "q1".into(),
                header: "Which".into(),
                question: "Which one?".into(),
                options: vec![QuestionOption { label: "A".into(), description: "the first".into() }],
                multi_select: true,
                secret: true,
            }],
        }),
        LiveEvent::ApprovalResolved {
            session_id: s(),
            turn_id: t(),
            request_id: "req_1".into(),
            decision: ApprovalDecision::AllowProject,
        },
        LiveEvent::Result(run_result(Some(Box::new(KeptWorktree {
            path: "/home/me/.krowk/wt/1".into(),
            branch: "krowk/1".into(),
            commits: 3,
            uncommitted_changes: true,
        })))),
        LiveEvent::Result(run_result(None)),
        LiveEvent::BackendAgents {
            session_id: s(),
            agents: vec![BackendAgent { task_id: "t1".into(), description: "digging".into(), agent: Some("x".into()) }],
        },
        LiveEvent::Background { session_id: s(), running: 2 },
        LiveEvent::SubagentStatus {
            session_id: s(),
            status: ChildState::Running,
            tool: Some(ChildTool { name: "bash".into(), started_ms: 11 }),
            last_event_ms: 12,
            waiting: Some(Waiting::Approval),
            tokens: 13,
        },
        LiveEvent::TurnUnprompted { session_id: s(), reason: "a background task finished".into() },
    ]);
    events
}

fn log_event(body: LogBody) -> LogEvent {
    LogEvent {
        id: "0192f0a0-0000-7000-8000-0000000000aa".into(),
        parent_id: Some("0192f0a0-0000-7000-8000-0000000000ab".into()),
        session_id: SESSION.into(),
        time_ms: 1_700_000_000_123,
        body,
    }
}

/// The value through UniFFI's own buffer, as Swift receives and returns
/// it: the custom types (JSON values, `usize`) included.
fn through_ffi<T: Lower<UniFfiTag> + Lift<UniFfiTag>>(value: T) -> T {
    let mut buf = Vec::new();
    <T as Lower<UniFfiTag>>::write(value, &mut buf);
    let mut read = buf.as_slice();
    let back = <T as Lift<UniFfiTag>>::try_read(&mut read).expect("lifts back");
    assert!(read.is_empty(), "the whole buffer is read");
    back
}

#[test]
fn r_proto_2_every_log_body_round_trips_through_decode_stream_line() {
    let bodies = log_bodies();
    let mut seen = [false; LOG_BODIES];
    for body in bodies {
        seen[log_body_index(&body)] = true;
        let line = StreamLine::Log(log_event(body));
        let json = serde_json::to_string(&line).unwrap();
        let decoded = decode_stream_line(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(decoded, line, "{json}");
        assert_eq!(through_ffi(decoded), line, "{json} through UniFFI's buffer");
    }
    assert!(seen.iter().all(|s| *s), "every LogBody variant is built: {seen:?}");
}

#[test]
fn r_proto_2_every_live_event_round_trips_through_decode_stream_line() {
    let mut seen = [false; LIVE_EVENTS];
    for ev in live_events() {
        seen[live_event_index(&ev)] = true;
        let line = StreamLine::Live(ev);
        let json = serde_json::to_string(&line).unwrap();
        let decoded = decode_stream_line(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(decoded, line, "{json}");
        assert_eq!(through_ffi(decoded), line, "{json} through UniFFI's buffer");
    }
    assert!(seen.iter().all(|s| *s), "every LiveEvent variant is built: {seen:?}");
}

#[test]
fn r_proto_2_a_log_event_at_rest_decodes() {
    for body in log_bodies() {
        let ev = log_event(body);
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(decode_log_event(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}")), ev);
    }
}

/// Which `Command` variant this is; exhaustive, as `log_body_index`.
fn command_index(c: &Command) -> usize {
    match c {
        Command::Prompt { .. } => 0,
        Command::Interrupt { .. } => 1,
        Command::Steer { .. } => 2,
        Command::Approve { .. } => 3,
        Command::SwitchModel { .. } => 4,
        Command::Fork { .. } => 5,
        Command::Continue { .. } => 6,
    }
}
const COMMANDS: usize = 7;

fn commands() -> Vec<Command> {
    let image = || ImageInput { number: 1, media_type: "image/jpeg".into(), data: "aGk=".into() };
    let budget = || Some(BudgetLimits { max_usd: Some(2.5), max_tokens: Some(100_000) });
    vec![
        Command::Prompt {
            session_id: Some(SESSION.into()),
            text: "fix the build".into(),
            images: vec![image()],
            model: Some(model()),
            permission_mode: PermissionMode::Plan,
            toolset: Some("lean".into()),
            effort: Some(Effort::High),
            budget: budget(),
        },
        Command::Prompt {
            session_id: None,
            text: "hi".into(),
            images: vec![],
            model: None,
            permission_mode: PermissionMode::Default,
            toolset: None,
            effort: None,
            budget: None,
        },
        Command::Interrupt { session_id: SESSION.into() },
        Command::Steer { session_id: SESSION.into(), text: "and the tests".into(), images: vec![image()] },
        Command::Approve {
            session_id: SESSION.into(),
            request_id: "req_1".into(),
            decision: ApprovalDecision::AllowSession,
            answers: vec![QuestionAnswer { id: "q1".into(), picked: vec!["A".into()], text: Some("why not".into()) }],
        },
        Command::SwitchModel { session_id: Some(SESSION.into()), model: model() },
        Command::Fork { session_id: SESSION.into(), from_event_id: "0192f0a0-0000-7000-8000-0000000000aa".into() },
        Command::Continue { session_id: SESSION.into(), budget: budget() },
    ]
}

#[test]
fn r_proto_1_r_proto_2_every_command_encodes_to_the_daemons_wire_form() {
    let mut seen = [false; COMMANDS];
    for command in commands() {
        seen[command_index(&command)] = true;
        let command = through_ffi(command);
        let wire = encode_command(command.clone()).unwrap();
        assert_eq!(wire, serde_json::to_string(&command).unwrap(), "the daemon's own serialization");
        let back: Command = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, command, "{wire}");
    }
    assert!(seen.iter().all(|s| *s), "every Command variant is built: {seen:?}");
    let wire = encode_command(Command::Interrupt { session_id: SESSION.into() }).unwrap();
    assert_eq!(wire, format!(r#"{{"type":"interrupt","sessionId":"{SESSION}"}}"#));
}

#[test]
fn r_proto_2_a_line_of_an_unknown_type_is_a_decode_error() {
    let line = json!({"type": "item.teleported", "sessionId": SESSION, "turnId": TURN}).to_string();
    match decode_stream_line(line) {
        Err(KrowkError::Decode { detail }) => assert!(detail.contains("stream line"), "{detail}"),
        other => panic!("{other:?}"),
    }
    let event = json!({"id": SESSION, "sessionId": SESSION, "timeMs": 1, "type": "turn.teleported"}).to_string();
    assert!(matches!(decode_log_event(event), Err(KrowkError::Decode { .. })));
    assert!(matches!(decode_stream_line("not json".into()), Err(KrowkError::Decode { .. })));
}

#[test]
fn r_proto_2_a_json_value_crosses_as_its_text_and_bad_text_does_not_lift() {
    // ApprovalRequest.input is a serde_json::Value: Swift holds the JSON
    // text, and text that is not JSON is refused on the way back in.
    let mut buf = Vec::new();
    <String as Lower<UniFfiTag>>::write("{\"a\":1}".into(), &mut buf);
    let value = <serde_json::Value as Lift<UniFfiTag>>::try_read(&mut buf.as_slice()).unwrap();
    assert_eq!(value, json!({"a": 1}));
    let mut bad = Vec::new();
    <String as Lower<UniFfiTag>>::write("{nope".into(), &mut bad);
    assert!(<serde_json::Value as Lift<UniFfiTag>>::try_read(&mut bad.as_slice()).is_err());
}
