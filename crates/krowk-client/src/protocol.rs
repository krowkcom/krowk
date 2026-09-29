//! The client protocol and the session log, as types. Everything a client
//! sends (`Command`), everything it is told (`StreamLine`: a logged
//! `LogEvent` or an ephemeral `LiveEvent`), and every line of a session's
//! log (`LogEvent`) is declared here, once. The JSON Schema in `schema/` is
//! generated from these types, never written beside them (R-PROTO-2), and a
//! test fails when the checked-in copy is stale.
//!
//! The shape is session → turn → item. A session is one conversation; a turn
//! is one prompt and everything the engine did to answer it; an item is one
//! thing inside a turn — the prompt text, a reply, reasoning, a tool call, a
//! tool result. An item streams as `item.started`, any number of
//! `item.delta`, then `item.completed`, all under one stable `itemId`.
//!
//! What is persisted and what is not is the lag rule (R-LAG-1): deltas are
//! live frames and never touch the log; the completed item, and the
//! structure around it, are the log. A client that attaches late reads the
//! log and misses nothing but the typing.
//!
//! Wire names are camelCase, and every id is a UUIDv7 in canonical lowercase
//! form, the same ids krowk.db mints.

pub mod frame;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on any change a client written against the previous version
/// would misread. Additive fields are not such a change.
pub const PROTOCOL_VERSION: u32 = 1;

/// A canonical lowercase UUIDv7, as the schema pins every event id.
pub const UUID7_PATTERN: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";

/// A model, named as the instance that serves it and the provider's own id
/// for it: `{"instance": "anthropic", "model": "claude-opus-5-5"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    /// A name from the instance registry, e.g. `anthropic` or `anthropic:work`.
    pub instance: String,
    /// The provider's model id, e.g. `claude-opus-5-5`.
    pub model: String,
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.instance, self.model)
    }
}

/// The wire API a provider speaks. An opaque blob is replayed only to the
/// same provider on the same wire API (R-LOG-3), so this travels with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum WireApi {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    /// OpenAI's Responses API, run stateless (`store: false`).
    #[serde(rename = "openai-responses")]
    OpenaiResponses,
    /// Chat Completions: xAI, OpenRouter, and any server that speaks it.
    #[serde(rename = "chat-completions")]
    ChatCompletions,
    /// The `claude` binary's stream-json protocol: Claude Code runs the
    /// loop and krowk drives it. A thinking signature it streams is
    /// Claude Code's to replay, never sent to the Messages API by krowk.
    #[serde(rename = "claude-code")]
    ClaudeCode,
    /// `codex app-server`'s JSON-RPC: Codex runs the loop, on its own
    /// login, and krowk drives it. Codex keeps its reasoning to itself, so
    /// nothing streamed here is ever replayed as a blob.
    #[serde(rename = "codex-app-server")]
    CodexAppServer,
}

impl WireApi {
    /// The name the config and the log use.
    pub fn name(self) -> &'static str {
        match self {
            WireApi::AnthropicMessages => "anthropic-messages",
            WireApi::OpenaiResponses => "openai-responses",
            WireApi::ChatCompletions => "chat-completions",
            WireApi::ClaudeCode => "claude-code",
            WireApi::CodexAppServer => "codex-app-server",
        }
    }
}

/// How hard a model thinks, on one ladder for every provider (R-PROV-2).
/// Each model takes some of these rungs under its own names; the rung asked
/// for is mapped onto the nearest one the model takes (`crate::effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Every rung, lowest first.
pub const LADDER: [Effort; 7] = [Effort::None, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];

impl Effort {
    /// The name on the ladder, which is also the wire value every provider
    /// that takes the rung uses for it.
    pub fn name(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Effort> {
        LADDER.into_iter().find(|e| e.name() == s.trim().to_ascii_lowercase())
    }

    pub fn names() -> Vec<&'static str> {
        LADDER.iter().map(|e| e.name()).collect()
    }
}

/// State a provider needs back verbatim and nobody else may read: a thinking
/// signature, redacted thinking, encrypted reasoning. Stored as the provider
/// sent it and replayed unmodified, only to the provider and wire API named
/// here. Anywhere else the reasoning is left out of the request — its text
/// is never passed off as something the other model said.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderBlob {
    /// The provider that produced it, e.g. `anthropic`.
    pub provider: String,
    pub wire_api: WireApi,
    /// Opaque to everything but that provider's client.
    pub data: Value,
}

/// One thing inside a turn, provider-neutral. `reasoning` carries what can
/// be shown, and the provider's own state in `blob`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Item {
    /// What the person asked.
    UserText { text: String },
    /// What the model answered, as text.
    AssistantText { text: String },
    /// The model's reasoning: its readable text (often a summary, or empty
    /// when the provider withholds it) and the blob that lets it be replayed.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blob: Option<ProviderBlob>,
    },
    /// A tool the model asked krowk to run. `callId` is the provider's id,
    /// which the matching result repeats.
    ToolCall { call_id: String, name: String, input: Value },
    /// What running it produced. A refusal or a failure is a result too,
    /// with `isError`, so the model can read why.
    ToolResult { call_id: String, output: String, is_error: bool },
}

/// What an item is, before any of it has arrived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ItemKind {
    UserText,
    AssistantText,
    Reasoning,
    ToolCall { call_id: String, name: String },
    ToolResult { call_id: String },
}

impl Item {
    pub fn kind(&self) -> ItemKind {
        match self {
            Item::UserText { .. } => ItemKind::UserText,
            Item::AssistantText { .. } => ItemKind::AssistantText,
            Item::Reasoning { .. } => ItemKind::Reasoning,
            Item::ToolCall { call_id, name, .. } => ItemKind::ToolCall { call_id: call_id.clone(), name: name.clone() },
            Item::ToolResult { call_id, .. } => ItemKind::ToolResult { call_id: call_id.clone() },
        }
    }
}

/// A piece of an item as it streams.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Delta {
    /// More text, for assistant text and reasoning alike.
    Text { text: String },
    /// More of a tool call's input, as a fragment of its JSON.
    ToolInput { partial_json: String },
}

/// Tokens, split the five ways they are priced. `outputTokens` excludes
/// `reasoningTokens` when the provider reports the split; a provider that
/// does not counts reasoning inside output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Input read at the full rate: what came after the last cache hit.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
    }
}

impl Usage {
    pub fn total(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens + self.reasoning_tokens
    }
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    /// Stopped by an `interrupt`: what arrived before it is kept.
    Interrupted,
    /// The engine could not finish: `error` says why.
    Failed,
}

/// Why something failed, as a code a script branches on and a sentence that
/// names the next action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
    /// The HTTP status the provider answered with, when the failure was an
    /// answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// For a rate or usage limit: when the provider said it lifts, in
    /// milliseconds since the Unix epoch, when it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
}

/// Claude-Code-compatible permission modes (R-PERM-1): `default` asks
/// before edits and commands, `acceptEdits` before commands, `plan` changes
/// nothing, and `bypassPermissions` asks before nothing — though a deny
/// rule, an ask rule and a hook's ask still hold in each of those. krowk's
/// own `unhinged` holds nothing krowk's rules say: every call runs but one a
/// hook blocks (`crate::permissions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    Unhinged,
}

impl PermissionMode {
    pub const NAMES: [&'static str; 5] = ["default", "acceptEdits", "plan", "bypassPermissions", "unhinged"];

    /// The mode as it is written: on the command line, in settings, on the
    /// wire.
    pub fn name(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::Unhinged => "unhinged",
        }
    }

    /// Whether the mode asks a person before nothing: a backend runs in
    /// its loosest setting, and krowk's evaluator answers what it asks.
    pub fn asks_nothing(self) -> bool {
        matches!(self, PermissionMode::BypassPermissions | PermissionMode::Unhinged)
    }

    pub fn parse(s: &str) -> Option<PermissionMode> {
        Some(match s {
            "default" => PermissionMode::Default,
            "acceptEdits" => PermissionMode::AcceptEdits,
            "plan" => PermissionMode::Plan,
            "bypassPermissions" => PermissionMode::BypassPermissions,
            "unhinged" => PermissionMode::Unhinged,
            _ => return None,
        })
    }
}

/// What a backend session is billed to, as the vendor reports it: the
/// account's subscription, or an API key (R-INST-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Billing {
    Subscription,
    ApiKey,
}

/// An answer to a permission request: allow this call, allow calls like it
/// for the rest of the session or for good in this project (the request's
/// `remember` rules), or deny it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Allow,
    AllowSession,
    AllowProject,
    Deny,
}

/// A tool call waiting for a person's say (R-PERM-2): sent to every client
/// of the session as `approval.requested`, and answered by one of them with
/// `approve`. It waits until answered or the turn is interrupted; a host
/// with no client that answers never sends one, and refuses the call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub session_id: String,
    pub turn_id: String,
    /// What `approve` names.
    pub request_id: String,
    /// The tool, as the model called it.
    pub tool: String,
    /// Its input, as the model gave it.
    pub input: Value,
    /// What it would do, in a line, e.g. Bash `rm -rf build` or Write /x/y.
    pub summary: String,
    /// Why it is asked rather than run.
    pub reason: String,
    /// The rules `allowSession` or `allowProject` would remember; none when
    /// this call can only be allowed once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remember: Vec<String>,
}

/// What a client asks of the engine. `prompt`, `interrupt`, `steer`,
/// `approve`, `switchModel` and `continue` are served today; the rest are typed now so
/// every client is written against the whole vocabulary, and are refused
/// with `not_implemented` until their tickets land.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Command {
    /// Start a turn: in `sessionId` when given, else in a new session.
    Prompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        text: String,
        /// The model for this turn; the session's last one when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(default)]
        permission_mode: PermissionMode,
        /// The toolset preset for this turn (`claude`, `gpt`, `grok`); the
        /// config's, else the one the model's family picks, when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        toolset: Option<String>,
        /// The reasoning effort for this turn, on krowk's ladder; the
        /// instance's, else the provider's default, when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
        /// The spend this session, with its subagents, may reach: the engine
        /// refuses the model call that would go past it (R-BUDGET-1). No
        /// limit when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<BudgetLimits>,
    },
    /// Stop the running turn, keeping what it produced so far.
    Interrupt { session_id: String },
    /// Add input to the running turn without stopping it. The engine takes
    /// it before its next model call, and it is logged there as a
    /// `userText` item.
    Steer { session_id: String, text: String },
    /// Answer an `approval.requested`.
    Approve { session_id: String, request_id: String, decision: ApprovalDecision },
    /// Continue the session on another model, instance or engine
    /// (R-SWITCH-4): checked now — the instance, its key or login, its
    /// binary — and refused with the fix when it cannot run, the session
    /// staying where it was; accepted, it is logged as `model.switched` and
    /// the session's next prompt runs there. While a turn runs it is logged
    /// once the turn is over. Without `sessionId` it is only checked: a
    /// client that has no session yet asks before its first prompt.
    SwitchModel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        model: ModelRef,
    },
    /// Branch the session at an event: the new branch's first event names
    /// it as its parent.
    Fork { session_id: String, from_event_id: String },
    /// Run the turn a backend began by itself (`turn.unprompted`) — Claude
    /// Code answering a background agent that finished — as a turn of the
    /// session, logged like any other. Refused with `nothing_pending` when
    /// no such turn is waiting. A `prompt` that arrives first runs it ahead
    /// of itself, so no prompt ever ends at that turn's answer.
    Continue {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<BudgetLimits>,
    },
}

/// What a session may spend, counted over the session and every subagent
/// it spawned, from the usage the provider metered — never from what a
/// request asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetLimits {
    /// US dollars, priced from models.dev: `--max-usd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    /// Generated tokens — output and reasoning, the part that overshoots a
    /// request's cap: `--max-tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
}

impl BudgetLimits {
    pub fn is_empty(&self) -> bool {
        self.max_usd.is_none() && self.max_tokens.is_none()
    }
}

/// Why a session moved to another model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SwitchReason {
    /// A client asked (`switchModel`).
    Requested,
    /// The instance it was on hit its rate or usage limit, and `rollover`
    /// is `auto` (R-INST-8).
    RateLimited,
    /// The model a turn switched to could not run it — no login, no key,
    /// no binary — so the session went back to the one before (R-SWITCH-4).
    SwitchFailed,
}

/// A switch the host suggests when an instance hits its limit and
/// `rollover` is `offer`, the default (R-INST-7): the client asks the
/// person, and a yes is `switchModel` to `to` and the prompt again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SwitchOffer {
    pub from: ModelRef,
    pub to: ModelRef,
    /// When `from`'s limit lifts, when the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
}

/// How close an instance is to its rate or usage limit, as its provider
/// last said (R-INST-6): Claude Code's rate-limit events, Codex's
/// rate-limit snapshots, a native API's rate-limit headers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LimitStatus {
    /// `allowed`, `warning` (near it) or `limited` (reached).
    pub status: LimitState,
    /// The window the limit counts over, as the provider names it:
    /// `five_hour`, `seven_day`, `requests`, `tokens`, `300m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// How much of it is used, 0–100, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// When it resets, in milliseconds since the Unix epoch, when the
    /// provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LimitState {
    Allowed,
    Warning,
    Limited,
}

/// How a backend was brought up to date with a session that ran somewhere
/// else first (R-SWITCH-2, R-INST-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HandoffKind {
    /// The vendor's own transcript, copied from another instance of the
    /// same vendor into this one's config directory and resumed there: the
    /// full context, not a summary.
    Transcript,
    /// The vendor resumed its own thread, and was told what happened on
    /// other models since it last ran.
    CatchUp,
    /// A new vendor session, seeded with krowk's summary of the earlier
    /// turns and the most recent ones as they happened.
    Summary,
}

/// One line of a session's log: typed, with a UUIDv7 `id`, and a `parentId`
/// naming the event before it on the same branch — so a fork or a rewind is
/// a second child of some earlier event, and the log is a tree (R-LOG-1).
/// Only the root, `session.started`, has no parent, and its id is the
/// session's id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    #[schemars(regex(pattern = UUID7_PATTERN))]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = UUID7_PATTERN))]
    pub parent_id: Option<String>,
    #[schemars(regex(pattern = UUID7_PATTERN))]
    pub session_id: String,
    /// Milliseconds since the Unix epoch, UTC. Order is the parent chain,
    /// never this.
    pub time_ms: i64,
    #[serde(flatten)]
    pub body: LogBody,
}

/// What a log event records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum LogBody {
    /// The root of every session.
    #[serde(rename = "session.started")]
    SessionStarted {
        cwd: String,
        krowk_version: String,
        protocol_version: u32,
        /// The session that spawned this one, for a subagent: its spend is
        /// part of what that session spent, and a budget counts it there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(regex(pattern = UUID7_PATTERN))]
        parent_session_id: Option<String>,
        /// The agent definition a subagent runs, by name, when it runs one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    /// A prompt arrived; the turn runs on `model`. The exact system prompt
    /// and tools it ran with are in the session's `context.jsonl` under
    /// this `turnId` (R-LOG-4).
    #[serde(rename = "turn.started")]
    TurnStarted {
        turn_id: String,
        model: ModelRef,
        provider: String,
        wire_api: WireApi,
        permission_mode: PermissionMode,
        /// The effort asked for, on krowk's ladder, when one was; what the
        /// model was sent is that rung mapped onto the ones it takes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
    },
    /// One item, whole. `itemId` is the id its live frames carried.
    #[serde(rename = "item.completed")]
    ItemCompleted { turn_id: String, item_id: String, item: Item },
    /// One model call finished: what it cost, and which items it produced,
    /// in order — the items that were one message on the provider's wire.
    #[serde(rename = "response.completed")]
    ResponseCompleted {
        turn_id: String,
        /// The provider's id for the response, e.g. Anthropic's `msg_…`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// The model the provider says answered.
        model: String,
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        item_ids: Vec<String>,
    },
    /// The vendor's own session behind a backend turn: what the next
    /// process resumes (`claude --resume`), and where the vendor keeps its
    /// transcript, so the session can travel with it (R-BACK-5). Logged
    /// when a backend first reports it, and again only when it changes.
    #[serde(rename = "backend.session")]
    BackendSession {
        turn_id: String,
        /// The backend, e.g. `claude-code` or `codex-app-server`.
        backend: String,
        /// The vendor's session id, e.g. Claude Code's.
        vendor_session_id: String,
        /// The vendor's transcript file for that session, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_path: Option<String>,
        /// Whether the vendor runs on the account's subscription or an API key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        billing: Option<Billing>,
    },
    /// A model call a backend's own subagent made — Claude Code's `Task` —
    /// which is the subagent's conversation, not this one: metered, so the
    /// budget and the session's cost count it, and never replayed.
    #[serde(rename = "subagent.response")]
    SubagentResponse {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// The model the provider says answered.
        model: String,
        usage: Usage,
    },
    /// A subagent this session started (R-SUB-1): the child session, which
    /// is its own log whose root names this session as its parent, and the
    /// tool call it answers — its final summary is that call's result. The
    /// link both ways is what moves, syncs and rebuilds a session tree.
    #[serde(rename = "subagent.started")]
    SubagentStarted {
        turn_id: String,
        /// The `subagent` tool call the child answers.
        call_id: String,
        #[schemars(regex(pattern = UUID7_PATTERN))]
        subagent_session_id: String,
        /// The few words the model gave it, which the TUI shows.
        description: String,
        /// The agent definition it runs, when any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        model: ModelRef,
    },
    /// The session's todo list, whole, as `todo_write` last set it
    /// (R-TODO-2): each write replaces the list before it.
    #[serde(rename = "todos.updated")]
    TodosUpdated { turn_id: String, todos: Vec<Todo> },
    /// The krowk run this session's evidence is grouped under, opened by
    /// its first `publish` (R-EVID-1). Logged once; every later publish, and
    /// a resumed session's, attaches to it.
    #[serde(rename = "run.opened")]
    RunOpened {
        turn_id: String,
        /// The run's slug, e.g. `run_…`.
        run: String,
    },
    /// The session moved to another model, instance or engine: asked for
    /// (`switchModel`), rolled over from an instance at its limit
    /// (R-INST-8), or back to the one before when the new one could not run
    /// a turn (R-SWITCH-4). The next turn runs on `to` unless its prompt
    /// names another.
    #[serde(rename = "model.switched")]
    ModelSwitched {
        /// The turn it followed, when it followed one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<ModelRef>,
        to: ModelRef,
        reason: SwitchReason,
        /// Why, in words: the limit that was hit and when it lifts, or the
        /// failure the new model's turn ended with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// How a backend turn was brought up to date with what the session did
    /// elsewhere (R-SWITCH-2, R-INST-4). What the vendor was sent is in
    /// `context.jsonl` under the turn, as `handoff`.
    #[serde(rename = "backend.handoff")]
    BackendHandoff {
        turn_id: String,
        how: HandoffKind,
        /// The instance whose transcript was copied, for `transcript`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_instance: Option<String>,
        /// Earlier turns told as a summary, and recent ones as they happened.
        #[serde(default)]
        summarized_turns: u32,
        #[serde(default)]
        recent_turns: u32,
        /// Why a better handoff was not possible: a transcript that could
        /// not be copied, a thread the vendor could not resume.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fell_back: Option<String>,
    },
    /// The turn is over.
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        turn_id: String,
        status: TurnStatus,
        /// Every model call in the turn, summed.
        usage: Usage,
        duration_ms: u64,
        /// What a backend said the turn cost, when it says (Claude Code's
        /// `total_cost_usd`). A budget counts it when it is more than krowk
        /// priced the turn's calls at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reported_cost_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorInfo>,
    },
}

/// One item of a todo list. Fields other harnesses' todo tools add —
/// Claude Code's `activeForm`, an `id`, a `priority` — are taken and
/// dropped, so a model trained on theirs is not refused for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

/// Where a todo stands. The names are the ones models are trained on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// A frame that is never logged: the typing, and the answer to a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum LiveEvent {
    #[serde(rename = "item.started")]
    ItemStarted { session_id: String, turn_id: String, item_id: String, item: ItemKind },
    #[serde(rename = "item.delta")]
    ItemDelta { session_id: String, turn_id: String, item_id: String, delta: Delta },
    /// What the session has spent so far, after each metered model call:
    /// what the status bar shows (R-BUDGET-2). `costUsd` counts the session
    /// and its subagents; null when any of it has no price.
    #[serde(rename = "cost")]
    Cost {
        session_id: String,
        turn_id: String,
        cost_usd: Option<f64>,
        /// This turn's part of it.
        turn_cost_usd: Option<f64>,
        /// Output and reasoning tokens, session and subagents: what
        /// `--max-tokens` counts.
        generated_tokens: i64,
    },
    /// Something for the person at the client and nobody else: an
    /// anonymous upload's claim command, whose token is a secret. Never
    /// logged, never in what a model reads; `krowk -p` prints it on stderr
    /// and leaves it out of `stream-json`. One sent between turns (a
    /// backend let go with its agents) has an empty `turnId`.
    #[serde(rename = "notice")]
    Notice { session_id: String, turn_id: String, text: String },
    /// How close an instance is to its rate or usage limit, whenever its
    /// provider says (R-INST-6).
    #[serde(rename = "limits")]
    Limits { session_id: String, turn_id: String, instance: String, limit: LimitStatus },
    /// A tool call waits for a person's say.
    #[serde(rename = "approval.requested")]
    ApprovalRequested(ApprovalRequest),
    /// A request was answered — by any client, or by an interrupt (`deny`):
    /// every other client that shows it can put it away.
    #[serde(rename = "approval.resolved")]
    ApprovalResolved { session_id: String, turn_id: String, request_id: String, decision: ApprovalDecision },
    /// How a `prompt` came out: the last thing a headless run prints.
    #[serde(rename = "result")]
    Result(RunResult),
    /// The agents a backend runs by itself for the session (Claude Code's
    /// `Agent` tool, often in the background), whole, each time the list
    /// changes — during a turn and between turns alike. Between turns it
    /// reaches a host's watchers (`Host::watch`) rather than a turn's
    /// stream. Empty when the last one has finished.
    #[serde(rename = "backend.agents")]
    BackendAgents { session_id: String, agents: Vec<BackendAgent> },
    /// A backend began a turn nobody prompted — Claude Code answering a
    /// background agent that finished — and it waits for `continue`. Sent
    /// to a host's watchers, between turns.
    #[serde(rename = "turn.unprompted")]
    TurnUnprompted {
        session_id: String,
        /// What it answers, in words: `background agent “x” completed`.
        reason: String,
    },
}

/// One agent a backend runs by itself: it is the vendor's, with its own
/// conversation, and krowk only reports it (its spend is metered as
/// `subagent.response`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendAgent {
    /// The vendor's id for it, e.g. Claude Code's `task_id`.
    pub task_id: String,
    /// The few words the model gave it.
    pub description: String,
    /// The agent definition it runs (Claude Code's `subagent_type`), when
    /// named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// The outcome of one `prompt`, with what it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub session_id: String,
    pub turn_id: String,
    pub status: TurnStatus,
    pub is_error: bool,
    /// The turn's final answer: its last assistant text.
    pub result: String,
    pub model: ModelRef,
    pub usage: Usage,
    /// USD at current models.dev prices, the turn's subagents included;
    /// null when any of it has no price.
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    pub num_model_calls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    /// Steering the host accepted for this turn and the engine never read —
    /// the turn was interrupted or failed first — oldest first, handed back
    /// so the client that sent it can offer it again. Never set on a
    /// completed turn: a turn does not complete with steering unread.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread_steers: Vec<String>,
    /// A turn that failed on its instance's rate or usage limit, with
    /// `rollover` at `offer`: the instance it could continue on (R-INST-7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_offer: Option<SwitchOffer>,
}

/// One line of `--output-format stream-json`, and of anything else that
/// carries the event stream: a logged event exactly as the log has it, or a
/// live frame. The two never share a `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StreamLine {
    Log(LogEvent),
    Live(LiveEvent),
}

impl StreamLine {
    /// The session the frame is about.
    pub fn session_id(&self) -> &str {
        match self {
            StreamLine::Log(ev) => &ev.session_id,
            StreamLine::Live(LiveEvent::ItemStarted { session_id, .. } | LiveEvent::ItemDelta { session_id, .. }) => session_id,
            StreamLine::Live(LiveEvent::Cost { session_id, .. } | LiveEvent::Notice { session_id, .. } | LiveEvent::Limits { session_id, .. }) => session_id,
            StreamLine::Live(LiveEvent::BackendAgents { session_id, .. } | LiveEvent::TurnUnprompted { session_id, .. }) => session_id,
            StreamLine::Live(LiveEvent::ApprovalRequested(r)) => &r.session_id,
            StreamLine::Live(LiveEvent::ApprovalResolved { session_id, .. }) => session_id,
            StreamLine::Live(LiveEvent::Result(r)) => &r.session_id,
        }
    }
}

/// A tool as the model is shown it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input: an object for a function tool,
    /// `{"type": "string"}` for a freeform one.
    pub input_schema: Value,
    /// Set on a freeform tool, whose input is text in this grammar rather
    /// than a JSON object — offered only where the wire API takes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar: Option<Grammar>,
}

/// The grammar a freeform tool's input is written in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Grammar {
    /// `lark`.
    pub syntax: String,
    pub definition: String,
}

/// One line of a session's `context.jsonl`: the exact system prompt and
/// tool definitions a turn ran with, for debugging and cache analysis
/// (R-LOG-4). Kept beside the log rather than in it, so the log stays small
/// and a change of prompt shows up as a diff between two lines here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextRecord {
    pub turn_id: String,
    pub time_ms: i64,
    pub model: ModelRef,
    pub provider: String,
    pub wire_api: WireApi,
    /// The toolset preset the tools came from.
    #[serde(default)]
    pub toolset: String,
    pub system: String,
    pub tools: Vec<ToolDefinition>,
    /// The system prompt's size in tokens, estimated at four bytes a token:
    /// no provider's tokenizer ships with krowk, and a budget compares the
    /// estimate with itself, so growth shows whatever the true ratio is.
    #[serde(default)]
    pub system_tokens: u64,
    /// The tool definitions' size, estimated the same way over the JSON the
    /// provider is sent.
    #[serde(default)]
    pub tools_tokens: u64,
    /// What a backend was sent ahead of the prompt to bring it up to date
    /// with turns it did not run (`backend.handoff`): exactly the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

/// What a client sends the host daemon over its unix socket (R-PROTO-1's
/// second transport), one JSON object a line. The first line is `hello`;
/// everything after it wraps the same `Command` the in-process client
/// hands `Host::execute`, so a client is written once for both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClientFrame {
    /// Names the protocol the client speaks, and the directory a new
    /// session it starts runs in: the daemon serves every directory at once.
    Hello {
        protocol_version: u32,
        cwd: String,
        /// The client's own version, for the daemon's log.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        krowk_version: String,
        /// Whether the client answers `approval.requested` (the TUI). A turn
        /// it runs asks it, and a request left with no such client
        /// following the session is denied. One that does not (`krowk -p`)
        /// runs its turns as the in-process `-p` does: what would be asked
        /// is refused, with what would allow it.
        #[serde(default)]
        answers_approvals: bool,
        /// The daemon's bearer token (`host.token`, beside the socket): what
        /// a WebSocket client proves it is this user with, since TCP cannot
        /// say whose process connected. It rides the hello rather than a
        /// header because browsers and Workers cannot set headers on a
        /// WebSocket. The unix socket ignores it: its permissions already
        /// keep everyone else out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Runs a command; answered by `done` with the same `id`. The session a
    /// `prompt` or `continue` runs in is followed from its first line.
    Execute { id: u64, command: Command },
    /// Follows a session: what its log holds after `afterEventId` (all of
    /// it when absent), the running turn's frames so far, then every frame
    /// as it happens — the same ones each other client following it gets.
    /// Answered by `attached` once the catching-up is sent.
    Attach {
        id: u64,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_event_id: Option<String>,
        /// The last `line.seq` of this session the client has: of the
        /// running turn's frames, only later ones are sent, so a client
        /// resuming from its cursor sees nothing twice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_seq: Option<u64>,
        /// The daemon's `welcome.epoch` the client's `afterSeq` is from. A
        /// `seq` is a daemon's own: one from another daemon, or with no
        /// epoch, is not honoured, and the client is caught up from
        /// `afterEventId` alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<u64>,
    },
    /// Asks how the daemon is; answered by `status`.
    Status { id: u64 },
    /// Reads the instances again — config.json and the credentials file —
    /// for every host, after a client connected or signed one out
    /// (`/connect`): `changed` is that instance, whose backend processes are
    /// not reused; `renamedFrom` and `renamedTo` a rename, whose are.
    /// Answered by `done`.
    Reload {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        renamed_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        renamed_to: Option<String>,
    },
    /// Asks the daemon to exit — `krowk host stop`, or `krowk host enable`
    /// handing over to the service. Refused with `host_busy` while a turn
    /// runs, and with `host_in_use` while another client is connected
    /// unless `force` (a TUI connected to it reconnects to the next one);
    /// answered by `done` just before it goes.
    Stop {
        id: u64,
        #[serde(default)]
        force: bool,
    },
    /// Stops following a session: the client moved to another (`/new`,
    /// `/sessions`). Its frames no longer come, and a request only this
    /// client could have answered is denied.
    Leave { session_id: String },
}

/// What the host daemon sends a client, one JSON object a line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerFrame {
    /// The answer to a `hello` the daemon can serve.
    Welcome {
        protocol_version: u32,
        krowk_version: String,
        pid: u32,
        /// This daemon's run, as the time it started (ms since the epoch):
        /// what a `line.seq` is numbered within, and what an `attach`
        /// resuming by `afterSeq` names.
        #[serde(default)]
        epoch: u64,
    },
    /// The answer to one it cannot: a client of another protocol version.
    /// The connection closes after it.
    Refused { code: String, message: String, fix: String },
    /// A frame of a session the client follows, exactly as the in-process
    /// transport streams it. `session` is the followed session it belongs
    /// to — the line's own, or its parent's for a subagent's — so a client
    /// following several tells their streams apart.
    Line {
        line: StreamLine,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        session: String,
        /// The `execute` whose own stream this is, on the lines sent to the
        /// client that ran it: a prompt that starts a session is told its
        /// lines by this, never by guessing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cmd: Option<u64>,
        /// Its place in the followed session's stream: numbered by the
        /// daemon as the host sends it, from 1, and never reused while the
        /// daemon runs (`welcome.epoch`), however often the session is let
        /// go and followed again. Absent on a line replayed from the log, whose
        /// cursor is its event id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    /// A command's end: its result for a `prompt` or `continue`, nothing for
    /// the rest, or why it did not run.
    Done {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<RunResult>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorInfo>,
    },
    /// An `attach` has caught up: every frame after this one is live.
    /// `running` says whether a turn of the session is under way.
    Attached {
        id: u64,
        session_id: String,
        running: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorInfo>,
    },
    Status { id: u64, status: HostStatus },
    /// A followed session has no command streaming in it any more: its
    /// turn is over (its `result` came first), or the command was refused
    /// before a turn began. A client following it, not running it, stops
    /// waiting here.
    Settled { session: String },
}

/// How the host daemon is: `krowk host status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub pid: u32,
    pub krowk_version: String,
    pub protocol_version: u32,
    pub socket: String,
    pub uptime_ms: u64,
    /// Clients connected now, this one included.
    pub clients: u32,
    /// How long it stays up with nothing to do; none when it never exits
    /// by itself (run as a service).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_exit_ms: Option<u64>,
    /// The loopback address its WebSocket listener is bound to, when one
    /// is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket: Option<String>,
    /// Bytes waiting for clients to read, over every client and session:
    /// bounded however far behind a client is (R-LAG-10).
    #[serde(default)]
    pub queued_bytes: u64,
    /// How many times a client has fallen behind, to be caught up from its
    /// cursor, since the daemon started.
    #[serde(default)]
    pub caught_up: u64,
    /// The sessions it has run since it started, newest first.
    pub sessions: Vec<HostSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostSession {
    pub session_id: String,
    /// A turn of it is under way.
    pub running: bool,
    /// Clients following it now.
    pub clients: u32,
}
