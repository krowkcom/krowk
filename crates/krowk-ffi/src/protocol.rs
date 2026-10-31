//! The client protocol, as Swift sees it (R-PROTO-2, R-PROTO-1): every type
//! reachable from `StreamLine` and `Command` in `krowk_client::protocol`,
//! re-declared with `#[uniffi::remote]` (rule 5). Each copy lists every
//! field and variant of the real type, and the compiler checks it against
//! that type, so a protocol change fails this crate's build until the copy
//! follows: the change reaches the phone in the same PR (R-REPO-1). These
//! are the real types, not mirrors; Swift gets them as generated enums and
//! structs, and JSON only at the edges ([`decode_stream_line`],
//! [`decode_log_event`], [`encode_log_event`], [`encode_command`]).
//!
//! Two field types UniFFI has no word for cross as custom types:
//! `serde_json::Value` (a tool's input, a provider blob's data) as its JSON
//! text, and `usize` (`KeptWorktree::commits`) as a `u64`. `Box<T>`
//! (`RunResult::worktree`) UniFFI takes as `T`.
//!
//! The daemon socket's frames, the host status and the context record are
//! not reachable from either and are not exported.

use krowk_client::protocol::{
    ApprovalDecision, ApprovalRequest, BackendAgent, Billing, BudgetLimits, ChildState, ChildTool, Command, Delta,
    Effort, ErrorInfo, HandoffKind, ImageInput, ImageRef, Item, ItemKind, KeptWorktree, LimitState, LimitStatus,
    LiveEvent, LogBody, LogEvent, ModelRef, PermissionMode, ProviderBlob, Question, QuestionAnswer, QuestionOption,
    RanBy, RunResult, StreamLine, SwitchOffer, SwitchReason, Todo, TodoStatus, TurnStatus, Usage, Waiting, WireApi,
};
use serde::Deserialize;
use serde_json::Value;

use crate::KrowkError;

// A JSON value crosses as its JSON text; Swift parses it if it needs to.
uniffi::custom_type!(Value, String, {
    remote,
    lower: |v| v.to_string(),
    try_lift: |s| Ok(serde_json::from_str(&s)?),
});

// UniFFI has no pointer-sized integer; a count crosses as a u64.
uniffi::custom_type!(usize, u64, {
    remote,
    lower: |n| n as u64,
    try_lift: |n| Ok(usize::try_from(n)?),
});

/// A line a client is told, as `decode_stream_line` reads it: a logged
/// event or a live one.
#[uniffi::remote(Enum)]
pub enum StreamLine {
    Log(LogEvent),
    Live(LiveEvent),
}

#[uniffi::remote(Record)]
pub struct LogEvent {
    pub id: String,
    pub parent_id: Option<String>,
    pub session_id: String,
    pub time_ms: i64,
    pub body: LogBody,
}

#[uniffi::remote(Enum)]
pub enum LogBody {
    SessionStarted {
        cwd: String,
        krowk_version: String,
        protocol_version: u32,
        parent_session_id: Option<String>,
        agent: Option<String>,
    },
    TurnStarted {
        turn_id: String,
        model: ModelRef,
        provider: String,
        wire_api: WireApi,
        permission_mode: PermissionMode,
        effort: Option<Effort>,
    },
    ItemCompleted {
        turn_id: String,
        item_id: String,
        item: Item,
    },
    ResponseCompleted {
        turn_id: String,
        response_id: Option<String>,
        model: String,
        usage: Usage,
        stop_reason: Option<String>,
        item_ids: Vec<String>,
    },
    BackendSession {
        turn_id: String,
        backend: String,
        vendor_session_id: String,
        transcript_path: Option<String>,
        billing: Option<Billing>,
    },
    SubagentResponse {
        turn_id: String,
        response_id: Option<String>,
        model: String,
        usage: Usage,
    },
    SubagentStarted {
        turn_id: String,
        call_id: String,
        subagent_session_id: String,
        description: String,
        agent: Option<String>,
        model: ModelRef,
        ran_by: RanBy,
        backend_id: Option<String>,
    },
    TodosUpdated {
        turn_id: String,
        todos: Vec<Todo>,
    },
    RunOpened {
        turn_id: String,
        run: String,
    },
    ModelSwitched {
        turn_id: Option<String>,
        from: Option<ModelRef>,
        to: ModelRef,
        reason: SwitchReason,
        detail: Option<String>,
    },
    BackendHandoff {
        turn_id: String,
        how: HandoffKind,
        from_instance: Option<String>,
        summarized_turns: u32,
        recent_turns: u32,
        fell_back: Option<String>,
    },
    SessionMoved {
        cwd: String,
        device: String,
        from_device: String,
    },
    TurnCompleted {
        turn_id: String,
        status: TurnStatus,
        usage: Usage,
        duration_ms: u64,
        reported_cost_usd: Option<f64>,
        reported_tokens: Option<i64>,
        error: Option<ErrorInfo>,
    },
}

#[uniffi::remote(Enum)]
pub enum LiveEvent {
    ItemStarted {
        session_id: String,
        turn_id: String,
        item_id: String,
        item: ItemKind,
    },
    ItemDelta {
        session_id: String,
        turn_id: String,
        item_id: String,
        delta: Delta,
    },
    Cost {
        session_id: String,
        turn_id: String,
        cost_usd: Option<f64>,
        turn_cost_usd: Option<f64>,
        generated_tokens: i64,
    },
    Notice {
        session_id: String,
        turn_id: String,
        text: String,
    },
    Limits {
        session_id: String,
        turn_id: String,
        instance: String,
        limit: LimitStatus,
    },
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved {
        session_id: String,
        turn_id: String,
        request_id: String,
        decision: ApprovalDecision,
    },
    Result(RunResult),
    BackendAgents {
        session_id: String,
        agents: Vec<BackendAgent>,
    },
    Background {
        session_id: String,
        running: u32,
    },
    SubagentStatus {
        session_id: String,
        status: ChildState,
        tool: Option<ChildTool>,
        last_event_ms: i64,
        waiting: Option<Waiting>,
        tokens: i64,
    },
    TurnUnprompted {
        session_id: String,
        reason: String,
    },
}

/// What a client sends, as `encode_command` writes it.
#[uniffi::remote(Enum)]
pub enum Command {
    Prompt {
        session_id: Option<String>,
        text: String,
        images: Vec<ImageInput>,
        model: Option<ModelRef>,
        permission_mode: PermissionMode,
        toolset: Option<String>,
        effort: Option<Effort>,
        budget: Option<BudgetLimits>,
    },
    PutImage {
        session_id: Option<String>,
        sha256: String,
        media_type: String,
        data: String,
    },
    Interrupt {
        session_id: String,
    },
    Steer {
        session_id: String,
        text: String,
        images: Vec<ImageInput>,
    },
    Approve {
        session_id: String,
        request_id: String,
        decision: ApprovalDecision,
        answers: Vec<QuestionAnswer>,
    },
    SwitchModel {
        session_id: Option<String>,
        model: ModelRef,
    },
    Fork {
        session_id: String,
        from_event_id: String,
    },
    Continue {
        session_id: String,
        budget: Option<BudgetLimits>,
    },
}

#[uniffi::remote(Record)]
pub struct ModelRef {
    pub instance: String,
    pub model: String,
}

#[uniffi::remote(Enum)]
pub enum WireApi {
    AnthropicMessages,
    OpenaiResponses,
    ChatCompletions,
    ClaudeCode,
    CodexAppServer,
}

/// How hard a model thinks. Where an `Effort?` is expected, write
/// `Effort.none` in full: a bare `.none` there is Swift's `nil`.
#[uniffi::remote(Enum)]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[uniffi::remote(Record)]
pub struct ProviderBlob {
    pub provider: String,
    pub wire_api: WireApi,
    pub data: Value,
}

#[uniffi::remote(Enum)]
pub enum Item {
    UserText { text: String, images: Vec<ImageRef> },
    AssistantText { text: String },
    Reasoning { text: String, blob: Option<ProviderBlob> },
    ToolCall { call_id: String, name: String, input: Value },
    ToolResult { call_id: String, output: String, is_error: bool },
}

#[uniffi::remote(Record)]
pub struct ImageInput {
    pub number: u32,
    pub media_type: String,
    pub data: String,
    pub sha256: Option<String>,
}

#[uniffi::remote(Record)]
pub struct ImageRef {
    pub number: u32,
    pub media_type: String,
    pub file: String,
    pub sha256: Option<String>,
}

#[uniffi::remote(Enum)]
pub enum ItemKind {
    UserText,
    AssistantText,
    Reasoning,
    ToolCall { call_id: String, name: String },
    ToolResult { call_id: String },
}

#[uniffi::remote(Enum)]
pub enum Delta {
    Text { text: String },
    ToolInput { partial_json: String },
}

#[uniffi::remote(Record)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
}

#[uniffi::remote(Enum)]
pub enum RanBy {
    Krowk,
    Claude,
    Codex,
}

#[uniffi::remote(Enum)]
pub enum ChildState {
    Running,
    Done,
    Interrupted,
    Failed,
}

#[uniffi::remote(Record)]
pub struct ChildTool {
    pub name: String,
    pub started_ms: i64,
}

#[uniffi::remote(Enum)]
pub enum Waiting {
    Approval,
    Question,
}

#[uniffi::remote(Enum)]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

#[uniffi::remote(Record)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
    pub http_status: Option<u16>,
    pub resets_at_ms: Option<i64>,
}

#[uniffi::remote(Enum)]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    Unhinged,
}

#[uniffi::remote(Enum)]
pub enum Billing {
    Subscription,
    ApiKey,
}

#[uniffi::remote(Enum)]
pub enum ApprovalDecision {
    Allow,
    AllowSession,
    AllowProject,
    Deny,
}

#[uniffi::remote(Record)]
pub struct ApprovalRequest {
    pub session_id: String,
    pub turn_id: String,
    pub request_id: String,
    pub tool: String,
    pub input: Value,
    pub summary: String,
    pub reason: String,
    pub remember: Vec<String>,
    pub questions: Vec<Question>,
}

#[uniffi::remote(Record)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    pub secret: bool,
}

#[uniffi::remote(Record)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[uniffi::remote(Record)]
pub struct QuestionAnswer {
    pub id: String,
    pub picked: Vec<String>,
    pub text: Option<String>,
}

#[uniffi::remote(Record)]
pub struct BudgetLimits {
    pub max_usd: Option<f64>,
    pub max_tokens: Option<i64>,
}

#[uniffi::remote(Enum)]
pub enum SwitchReason {
    Requested,
    RateLimited,
    SwitchFailed,
}

#[uniffi::remote(Record)]
pub struct SwitchOffer {
    pub from: ModelRef,
    pub to: ModelRef,
    pub resets_at_ms: Option<i64>,
}

#[uniffi::remote(Record)]
pub struct LimitStatus {
    pub status: LimitState,
    pub window: Option<String>,
    pub used_percent: Option<f64>,
    pub resets_at_ms: Option<i64>,
}

#[uniffi::remote(Enum)]
pub enum LimitState {
    Allowed,
    Warning,
    Limited,
}

#[uniffi::remote(Enum)]
pub enum HandoffKind {
    Transcript,
    CatchUp,
    Summary,
}

#[uniffi::remote(Record)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

#[uniffi::remote(Enum)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[uniffi::remote(Record)]
pub struct BackendAgent {
    pub task_id: String,
    pub description: String,
    pub agent: Option<String>,
}

#[uniffi::remote(Record)]
pub struct RunResult {
    pub session_id: String,
    pub turn_id: String,
    pub status: TurnStatus,
    pub is_error: bool,
    pub result: String,
    pub model: ModelRef,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    pub num_model_calls: u32,
    pub error: Option<ErrorInfo>,
    pub unread_steers: Vec<String>,
    pub switch_offer: Option<SwitchOffer>,
    pub worktree: Option<Box<KeptWorktree>>,
}

#[uniffi::remote(Record)]
pub struct KeptWorktree {
    pub path: String,
    pub branch: String,
    pub commits: usize,
    pub uncommitted_changes: bool,
}

/// One line of the daemon's stream, a logged or a live event, from its
/// JSON text.
#[uniffi::export]
pub fn decode_stream_line(json: String) -> Result<StreamLine, KrowkError> {
    let what = decode_error("a stream line");
    let untagged = match serde_json::from_str(&json) {
        Ok(line) => return Ok(line),
        Err(e) => e,
    };
    // StreamLine is untagged, so serde's own error is only "data did not
    // match any variant of untagged enum StreamLine". Decoded again as each
    // kind, only to say what is wrong: the log event's error when the line
    // has an `id`, which every log event has and no live one declares, or
    // when its `type` is a log event's and not a live one's; else the live
    // event's. A field given twice is refused above, and only there.
    let Ok(value) = serde_json::from_str::<Value>(&json) else {
        return Err(what(untagged));
    };
    let (Err(log), Err(live)) = (LogEvent::deserialize(&value), LiveEvent::deserialize(&value)) else {
        return Err(what(untagged));
    };
    let ty = value.get("type").and_then(Value::as_str).unwrap_or_default();
    if value.get("id").is_some() || (is_tag::<LogBody>(ty) && !is_tag::<LiveEvent>(ty)) {
        Err(what(log))
    } else {
        Err(what(live))
    }
}

/// Whether `ty` is one of the `type` tags of `T`: `{"type": ty}` alone
/// fails for a missing field, not for an unknown variant.
fn is_tag<T: serde::de::DeserializeOwned>(ty: &str) -> bool {
    match serde_json::from_value::<T>(serde_json::json!({ "type": ty })) {
        Ok(_) => true,
        Err(e) => !e.to_string().contains("unknown variant"),
    }
}

/// One line of a session's log, as the log at rest holds it, from its JSON
/// text.
#[uniffi::export]
pub fn decode_log_event(json: String) -> Result<LogEvent, KrowkError> {
    serde_json::from_str(&json).map_err(decode_error("a log event"))
}

/// One line of a session's log as the log at rest holds it: its JSON
/// text, which `decode_log_event` reads back. The phone's cache keeps
/// events this way, so Swift never builds or parses their JSON.
#[uniffi::export]
pub fn encode_log_event(event: LogEvent) -> Result<String, KrowkError> {
    serde_json::to_string(&event).map_err(|e| KrowkError::Internal { detail: format!("a log event did not encode: {e}") })
}

/// A command as the daemon reads it off the wire: its JSON text.
#[uniffi::export]
pub fn encode_command(command: Command) -> Result<String, KrowkError> {
    serde_json::to_string(&command).map_err(|e| KrowkError::Internal { detail: format!("a command did not encode: {e}") })
}

fn decode_error(what: &'static str) -> impl Fn(serde_json::Error) -> KrowkError {
    move |e| KrowkError::Decode { detail: format!("that is not {what} this build understands: {e}") }
}
