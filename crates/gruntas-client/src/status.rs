//! The status layer's contract (R-STA-1, D1, D8, D9): what every agent
//! session's status is, the board a machine publishes of its sessions, and
//! the counts the registry is allowed to see.
//!
//! A session's status is one A2A v1.0 `Task` (a2aproject/A2A,
//! specification/a2a.proto, in its ProtoJSON form): its id is the session
//! id, `status.state` an A2A `TaskState`, `status.message` the one-line
//! summary and `status.timestamp` when the state was set. What gruntas adds
//! rides in the Task's `metadata` under one extension URI
//! (`EXTENSION_URI`), every field tagged with who said it: the agent
//! (reported) or the harness (observed). Liveness is the gRPC Health
//! Checking Protocol's vocabulary (`Health`). There is no stalled state:
//! quiet is `lastActivity`, and the reader judges.
//!
//! A machine's `Board` lists its registered sessions' Tasks, sealed to the
//! person's devices (`seal_board`, `open_board`) like presence, within a
//! 256 KB budget (`Board::fit`). The registry never opens it; what it sees
//! is `Counts`: the machine's health and how many agents are in each state,
//! under a random statistics id that is not the device id.
//!
//! Pure code: the daemon that keeps and publishes the board, and the views
//! that read it, are elsewhere.

use crate::device_chain::{refused_in_name, Chain, Kind};
use crate::e2e::{self, DeviceId, Label};
use crate::user_key::UserKeys;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The gruntas extension's URI: the key a Task's gruntas fields ride under
/// in its `metadata`, and a history message's report under in its own.
pub const EXTENSION_URI: &str = "https://gruntas.com/a2a/status/v1";

/// The most sealed bytes a board takes (D8). Over it, `Board::fit` drops
/// and shortens in the spec's order.
pub const MAX_SEALED: usize = 256 * 1024;
/// The board's JSON is padded with spaces to a multiple of this before it
/// is sealed, so its length says little about what it lists.
pub const PAD: usize = 1024;
/// The most reports a Task's history keeps (D2).
pub const MAX_HISTORY: usize = 10;
/// The reports a Task keeps when its board is over budget (D8).
pub const TRIMMED_HISTORY: usize = 3;
/// A finished Task older than this is the first to go from a board over
/// budget (D8), in seconds.
pub const FINISHED_KEPT_SECONDS: i64 = 60 * 60;
/// A summary shortened for the budget keeps at most this many characters,
/// an ellipsis included.
pub const SHORT_SUMMARY: usize = 80;
/// The most Tasks one board lists: the daemon's own limit on registrations.
pub const MAX_TASKS: usize = 1024;

/// A2A's `TaskState`, as ProtoJSON writes it. A state this build does not
/// know reads as `Unspecified`, rather than refusing the whole board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TaskState {
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
    #[serde(rename = "TASK_STATE_AUTH_REQUIRED")]
    AuthRequired,
    #[serde(rename = "TASK_STATE_UNSPECIFIED")]
    #[serde(other)]
    Unspecified,
}

impl TaskState {
    /// A2A's terminal states: the Task is over.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected)
    }
}

/// The gRPC Health Checking Protocol's serving status (D4): an agent is
/// `SERVING` while its harness process lives, `NOT_SERVING` once it is
/// gone; a machine is `SERVING` while its presence is live, `UNKNOWN` once
/// it lapses. One this build does not know reads as `UNKNOWN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Health {
    Serving,
    NotServing,
    #[serde(other)]
    Unknown,
}

/// Who said a field (D1): the agent reported it, or the harness observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Agent,
    Harness,
}

/// A gruntas field and who said it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Field<T> {
    pub value: T,
    pub source: Provenance,
}

impl<T> Field<T> {
    pub fn agent(value: T) -> Field<T> {
        Field { value, source: Provenance::Agent }
    }

    pub fn harness(value: T) -> Field<T> {
        Field { value, source: Provenance::Harness }
    }
}

/// One session's status: an A2A v1.0 Task, id the session id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// The session id.
    pub id: String,
    pub status: TaskStatus,
    /// The agent's reports, oldest first, the last `MAX_HISTORY` kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
    pub metadata: TaskMetadata,
}

/// A2A's `TaskStatus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    /// The one-line summary, as an agent message with one text part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    /// When the state was set, RFC 3339, the publishing machine's clock.
    pub timestamp: String,
}

/// A2A's `Message`, as far as status uses it: text parts, and the
/// extension's report in a history entry's metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub message_id: String,
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MessageMetadata>,
}

impl Message {
    /// An agent message of one text part: a summary.
    pub fn text(message_id: impl Into<String>, text: impl Into<String>) -> Message {
        Message { message_id: message_id.into(), role: Role::Agent, parts: vec![Part { text: text.into() }], metadata: None }
    }

    /// Its text parts, joined.
    pub fn summary(&self) -> String {
        self.parts.iter().map(|p| p.text.as_str()).collect()
    }
}

/// A2A's `Role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Role {
    #[serde(rename = "ROLE_USER")]
    User,
    #[serde(rename = "ROLE_AGENT")]
    Agent,
    #[serde(rename = "ROLE_UNSPECIFIED")]
    #[serde(other)]
    Unspecified,
}

/// A2A's `Part`, text only: a part of another kind reads as empty text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Part {
    #[serde(default)]
    pub text: String,
}

/// A Task's `metadata`: the gruntas extension, under its URI.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct TaskMetadata {
    #[serde(rename = "https://gruntas.com/a2a/status/v1")]
    pub gruntas: Extension,
}

/// A history message's `metadata`: the report it records, under the
/// extension's URI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MessageMetadata {
    #[serde(rename = "https://gruntas.com/a2a/status/v1")]
    pub report: Report,
}

/// One `gruntas_report` (D2), as its history entry records it; the summary
/// is the message's text.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<TaskState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// When it was reported, RFC 3339, the publishing machine's clock.
    pub timestamp: String,
}

/// The gruntas fields of a Task (D1), each with its provenance. All are
/// optional: a field nobody has said yet is absent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Extension {
    /// Who set `status.state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_source: Option<Provenance>,
    /// The agent's health (D4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Field<Health>>,
    /// The repository's origin, `presence::normalise_origin`'s form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Field<String>>,
    /// The machine's device id, in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<Field<String>>,
    /// `claude` or `codex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Field<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Field<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<Field<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<Field<String>>,
    /// The HEAD commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<Field<String>>,
    /// The worktree's path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Field<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<Field<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Field<String>>,
    /// ACP's plan, entry by entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Field<Vec<PlanEntry>>>,
    /// The pull request's URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<Field<String>>,
    /// Forge checks: empty until a later build fills them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Field<Vec<serde_json::Value>>>,
    /// What the session has cost, in US dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Field<f64>>,
    /// The last activity any hook or report saw, RFC 3339, the publishing
    /// machine's clock (D6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<Field<String>>,
}

/// One entry of an ACP plan (agentclientprotocol.com, `PlanEntry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanPriority,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// A machine's status board (D8): its registered sessions' Tasks, sealed
/// to the person's devices and published as its own registry record. Status
/// only, never a transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    /// The publishing device's id, in hex.
    pub device: String,
    /// When it was published, RFC 3339, the publishing machine's clock: a
    /// reader computes "quiet for" from it and when it received the board,
    /// never against its own clock (D6).
    pub published_at: String,
    /// Every session registered with the daemon, so a reader finds a
    /// session's machine (D5) even when its Task was dropped for the budget.
    #[serde(default)]
    pub sessions: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl Board {
    /// The board's bounds, held on seal and on open: no control, format or
    /// separator character in any string (a reader prints them), at most
    /// `MAX_TASKS` Tasks and sessions and `MAX_HISTORY` reports a Task, and
    /// a publish time that reads as one.
    pub fn check(&self) -> Result<(), String> {
        let v = serde_json::to_value(self).map_err(|e| format!("the board does not encode: {e}"))?;
        if refused_string(&v) {
            return Err("the board has a control character in it".into());
        }
        if self.tasks.len() > MAX_TASKS || self.sessions.len() > MAX_TASKS {
            return Err(format!("the board lists more than {MAX_TASKS} sessions"));
        }
        if let Some(t) = self.tasks.iter().find(|t| t.history.len() > MAX_HISTORY) {
            return Err(format!("the board keeps more than {MAX_HISTORY} reports for session {}", t.id));
        }
        timestamp(&self.published_at).ok_or("the board's publish time is not an RFC 3339 time")?;
        Ok(())
    }

    /// Its size sealed, padded as `seal_board` pads it.
    pub fn sealed_len(&self) -> usize {
        let json = serde_json::to_vec(self).map_or(usize::MAX / 2, |j| j.len());
        e2e::PERSON_OVERHEAD + json.div_ceil(PAD) * PAD
    }

    /// Brings the board within `MAX_SEALED` by D8's rule, each step only
    /// when the one before left it over: finished Tasks whose state was set
    /// more than an hour before the publish time go, then every Task's
    /// history beyond its last `TRIMMED_HISTORY` reports, then summaries
    /// are shortened to `SHORT_SUMMARY` characters. An error when it is
    /// over even then.
    pub fn fit(&mut self) -> Result<(), String> {
        if self.sealed_len() <= MAX_SEALED {
            return Ok(());
        }
        let published = timestamp(&self.published_at).ok_or("the board's publish time is not an RFC 3339 time")?;
        let cutoff = published - jiff::SignedDuration::from_secs(FINISHED_KEPT_SECONDS);
        // A finished Task whose time does not read is old.
        self.tasks.retain(|t| !t.status.state.is_terminal() || timestamp(&t.status.timestamp).is_some_and(|at| at >= cutoff));
        if self.sealed_len() <= MAX_SEALED {
            return Ok(());
        }
        for t in &mut self.tasks {
            let over = t.history.len().saturating_sub(TRIMMED_HISTORY);
            t.history.drain(..over);
        }
        if self.sealed_len() <= MAX_SEALED {
            return Ok(());
        }
        for t in &mut self.tasks {
            for m in t.status.message.iter_mut().chain(t.history.iter_mut()) {
                shorten(m);
            }
        }
        match self.sealed_len() {
            n if n <= MAX_SEALED => Ok(()),
            n => Err(format!("the board seals to {n} bytes even trimmed, more than {MAX_SEALED}")),
        }
    }
}

fn shorten(m: &mut Message) {
    let text = m.summary();
    if text.chars().count() <= SHORT_SUMMARY {
        return;
    }
    let short: String = text.chars().take(SHORT_SUMMARY - 1).collect();
    m.parts = vec![Part { text: short + "…" }];
}

fn timestamp(s: &str) -> Option<jiff::Timestamp> {
    s.parse().ok()
}

fn refused_string(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::String(s) => s.chars().any(refused_in_name),
        Value::Array(a) => a.iter().any(refused_string),
        Value::Object(m) => m.iter().any(|(k, v)| k.chars().any(refused_in_name) || refused_string(v)),
        _ => false,
    }
}

/// `b` sealed to the person's devices, bound to `me`, this device: brought
/// within budget first (`Board::fit`), within its bounds and about `me`,
/// and padded with spaces to a multiple of `PAD` bytes.
pub fn seal_board(b: &Board, keys: &UserKeys, current_generation: u32, me: &DeviceId) -> Result<Vec<u8>, String> {
    let mut b = b.clone();
    b.fit()?;
    b.check()?;
    if b.device != me.to_string() {
        return Err(format!("the board is about device {}, not this one ({me})", b.device));
    }
    let mut json = serde_json::to_vec(&b).map_err(|e| format!("the board does not encode: {e}"))?;
    json.resize(json.len().div_ceil(PAD) * PAD, b' ');
    e2e::seal_to_person(keys, current_generation, Label::StatusBoard, &me.0, &json).map_err(|e| e.0)
}

/// The board `device` sealed, opened. As `presence::open`: it must come
/// from a device on the verified list `chain` (not the recovery kit), be
/// within `MAX_SEALED`, sealed under the list's current generation or
/// later, open bound to `device`, be about `device` and within its bounds.
pub fn open_board(sealed: &[u8], device: &DeviceId, keys: &UserKeys, chain: &Chain) -> Result<Board, String> {
    let listed = chain.devices().iter().find(|d| d.id() == *device).ok_or_else(|| format!("device {device} is not on your device list"))?;
    if listed.kind == Kind::Recovery {
        return Err(format!("device {device} is your recovery kit, which publishes no status"));
    }
    if sealed.len() > MAX_SEALED {
        return Err(format!("the status board from {device} is longer than {MAX_SEALED} bytes"));
    }
    let generation = e2e::person_generation(sealed).map_err(|e| e.0)?;
    if generation < chain.generation() {
        return Err(format!("the status board from {device} is sealed under user key generation {generation}, older than the generation {} your device list names current", chain.generation()));
    }
    let json = e2e::open_from_person(sealed, Label::StatusBoard, &device.0, keys).map_err(|e| e.0)?;
    let b: Board = serde_json::from_slice(&json).map_err(|e| format!("the status board from {device} does not decode: {}", e.to_string().escape_debug()))?;
    b.check()?;
    if b.device != device.to_string() {
        return Err(format!("the status board from {device} is about another device ({})", b.device));
    }
    Ok(b)
}

/// What the registry receives about a machine (D9), under a random
/// statistics id (`statistics_id`) rather than the device id: the machine's
/// health and how many of its agents are in each A2A state and each health
/// status. Nothing per project, and no content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Counts {
    /// The machine's own health: `SERVING` from a machine publishing it.
    pub health: Health,
    /// Agents by A2A state; a state with none is absent.
    #[serde(default)]
    pub states: BTreeMap<TaskState, u32>,
    /// Agents by health; a status with none is absent. An agent whose
    /// health is not known counts as `UNKNOWN`.
    #[serde(default)]
    pub health_counts: BTreeMap<Health, u32>,
}

impl Counts {
    /// The counts of `b`'s Tasks, from a machine that is serving.
    pub fn of(b: &Board) -> Counts {
        let mut c = Counts { health: Health::Serving, states: BTreeMap::new(), health_counts: BTreeMap::new() };
        for t in &b.tasks {
            *c.states.entry(t.status.state).or_default() += 1;
            let health = t.metadata.gruntas.health.as_ref().map_or(Health::Unknown, |h| h.value);
            *c.health_counts.entry(health).or_default() += 1;
        }
        c
    }
}

/// A fresh statistics id: 16 random bytes in lower-case hex, drawn once per
/// device and kept, never derived from the device id.
pub fn statistics_id() -> String {
    e2e::random::<16>().iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `id` has a statistics id's form: 32 lower-case hex digits.
pub fn is_statistics_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
