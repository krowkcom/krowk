//! A command one of the person's devices sends a host, and the host's
//! answer (R-HOST-4): `start`, a session in a directory the host announced,
//! on a model it has ready; `take`, a session another host holds moved to
//! this one, into a clone of its repository in a directory it announced
//! (R-HAND-1), so a device that never hosts can move one; `run`, a task a
//! supervisor hands this machine to run in a fresh worktree of its clone of
//! a repository (R-ALLOC-3); `ping`, answered at once (R-STA-7); and the
//! god agent's four (its D1–D5): `read`, a page of a session's transcript;
//! `changes`, what the host's sessions did since a time; `message` and
//! `interrupt`, a steer of a session, held to the host's `steer` setting.
//! An answer too big for the registry's 4 KB (a transcript page, a long
//! list of changes) goes in the command's answer record, up to 64 KB, and
//! the answer itself is `recorded` (`seal_answer_or_record`). A host older
//! than a command refuses it `unknown_command`, which the asking side says
//! as "that machine needs updating" (`needs_updating`). The
//! registry carries them as bytes
//! (`gruntas_api` `send_host_command`, `answer_host_command`), so each is
//! sealed to the person's devices (`e2e::seal_to_person`) and bound to the
//! command's id and the two devices: one moved to another id, or to another
//! pair of devices, does not open.
//!
//! Pure code: the host that runs a command is gruntas-harness's presence
//! thread, and the clients that send one are `gruntas sync new` and `gruntas sync take`.
//! A `take` runs there as `gruntas sync take` runs: the target still asks.

use crate::device_chain::{Chain, Kind};
use crate::e2e::{self, DeviceId, Label};
use crate::protocol::{BudgetLimits, ModelRef, Origin, PermissionMode};
use crate::user_key::UserKeys;
use serde::{Deserialize, Serialize};

/// The longest prompt a `start` carries.
pub const MAX_PROMPT: usize = 256 * 1024;
/// The most sealed bytes the registry takes for a command
/// (`HostCommand::SEALED_BYTES`): a prompt that seals to more is refused
/// before it is sent.
pub const MAX_SEALED: usize = 16 * 1024;
/// The most sealed bytes the registry takes for an answer
/// (`HostCommand::ANSWER_BYTES`).
pub const MAX_ANSWER: usize = 4 * 1024;
/// The longest refusal message an answer carries: one longer is cut, so
/// the answer always fits.
pub const MAX_MESSAGE: usize = 3 * 1024;
/// The longest refusal code an answer carries.
pub const MAX_CODE: usize = 64;

/// What one device asks a host to do. Unknown fields are ignored: a
/// command carrying `permissionMode`, `toolset`, `effort` or `budget` opens,
/// and those are dropped unread. The remote side picks the place, the model
/// and the words, never the permissions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum HostCommand {
    /// A new session in `cwd`, one of the directories the host announced,
    /// its first turn `prompt` on `model`.
    ///
    /// `sending_session` is the session that sent it, when an agent did
    /// (D4): the started session records it as its parent. Absent, the
    /// started session has no parent.
    Start {
        cwd: String,
        model: ModelRef,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sending_session: Option<String>,
    },
    /// Session `session_id`, held by another host, moved here into `cwd`,
    /// one of the directories the host announced and a clone of the
    /// session's repository: what `gruntas sync take` there does.
    /// `origin` is where the take came from (D7).
    Take {
        session_id: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
    },
    /// A task a supervisor hands this host (R-ALLOC-3, D8–D14): run once,
    /// in a fresh worktree of the host's own clone of `origin`, from the
    /// sealed job. Answered `started` with the child session's id, or
    /// `refused` with a `RunRefusal`'s code.
    Run(RunTask),
    /// An empty request the host answers at once with `pong` (D5, MCP's
    /// `ping`): a liveness check. With `session`, a session registered with
    /// the host's daemon, the answer adds its health and last activity.
    Ping {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// One page of `session`'s transcript, newest first (D3): `page` is
    /// the `next` cursor of the page before, none for the newest. Answered
    /// `page`, through the answer record, or refused `cant_read`.
    Read {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<String>,
    },
    /// What the host's sessions did since `since` (D4): the host's own
    /// board publish time the asker last saw, RFC 3339, so it is the
    /// host's clock on both sides. Answered `changes`.
    Changes { since: String },
    /// `text` given to `session` (D5): a prompt when it is idle, a steer
    /// when a turn runs. `origin`, always an agent's, is checked against
    /// the host's `steer` setting. Answered `delivered`, or refused.
    Message { session: String, text: String, origin: Origin },
    /// `session`'s running turn stopped (D5), held to the `steer` setting
    /// as a message is. Answered `delivered`, or refused `nothing_running`.
    Interrupt { session: String, origin: Origin },
}

/// The longest message text a `message` carries: far within the 16 KB a
/// command seals to.
pub const MAX_STEER_TEXT: usize = 8 * 1024;
/// The longest page cursor a `read` carries.
pub const MAX_CURSOR: usize = 128;
/// The most sealed bytes the registry keeps as a command's answer record
/// (GA02's `answer_record`).
pub const MAX_RECORD: usize = 64 * 1024;

/// The refusal codes of the god agent's commands, beside the older ones
/// (`bad_command`, `host_failed`, `steer_refused`, …).
pub mod refusal {
    /// A host older than the command: "that machine needs updating".
    pub const UNKNOWN_COMMAND: &str = "unknown_command";
    /// No session by that id on the host.
    pub const NO_SESSION: &str = "no_session";
    /// A session the host can't read yet (Codex): "can't read this session yet".
    pub const CANT_READ: &str = "cant_read";
    /// A session the host can't steer yet (Codex, headless, no plugin):
    /// "can't steer this session yet", with the reason.
    pub const CANT_STEER: &str = "cant_steer";
    /// The `steer` setting refused the origin (R-SEC-4).
    pub const STEER_REFUSED: &str = "steer_refused";
    /// An interrupt with no turn running.
    pub const NOTHING_RUNNING: &str = "nothing_running";
    /// A host that knows the command but does not answer it yet: the
    /// god agent's build in progress, its message naming the ticket.
    pub const NOT_YET: &str = "not_yet";
}

/// The words for a refusal from a host older than the command.
pub const NEEDS_UPDATING: &str = "that machine needs updating";

/// One page of a session's transcript (D3, R-GOD-4): redacted and framed
/// as foreign text by the host, newest first, about 12 KB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptPage {
    pub session: String,
    pub text: String,
    /// The cursor a `read` names for the page before this one in time;
    /// none at the start of what the host reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// What a host's sessions did since a time (D4, R-GOD-3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesSince {
    /// The `since` asked.
    pub since: String,
    /// When the host answered, RFC 3339, its own clock: the next `since`.
    pub at: String,
    /// Oldest first.
    pub changes: Vec<Change>,
}

/// One session's state transition or report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub session: String,
    /// RFC 3339, the host's clock.
    pub at: String,
    /// The state it moved to, for a transition, or the one a report set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<crate::status::TaskState>,
    /// A report's summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The longest session id a `ping` names: one longer is refused as it
/// opens.
pub const MAX_SESSION_ID: usize = 128;

/// What a host knows of a session a `ping` named (D5): its health, and its
/// last activity and the time it answered, both by the host's clock, so a
/// reader computes how long it has been quiet without comparing clocks
/// (D6). A session not registered there is `UNKNOWN` with no activity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPing {
    pub health: crate::status::Health,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    /// When the host answered, RFC 3339, its own clock.
    pub at: String,
}

impl SessionPing {
    /// How long the session has been quiet, by the host's clock alone; None
    /// when either time does not read. Never negative.
    pub fn quiet_for(&self) -> Option<jiff::SignedDuration> {
        let at: jiff::Timestamp = self.at.parse().ok()?;
        let last: jiff::Timestamp = self.last_activity.as_deref()?.parse().ok()?;
        Some(at.duration_since(last).max(jiff::SignedDuration::ZERO))
    }
}

/// What a `run` carries. The prompt rides in the job, not here, so a task
/// of any length fits the registry's 16 KB for a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTask {
    /// The repository, as `presence::normalise_origin` writes it: the host
    /// runs it in the trusted directory it announced with this origin.
    pub origin: String,
    /// The model, resolved on the supervisor's machine (D12): what the call
    /// named, else the default a local subagent there would run on. The
    /// host needs that instance ready, else `no_model`.
    pub model: ModelRef,
    /// The sealed job (R-ALLOC-4): where it is stored and its key. The
    /// host reads it and never spends it, so a fallback machine can read
    /// the same job; the supervisor spends it once a child started.
    pub job: JobRef,
    /// The cap the child runs under: what is left of the supervisor's tree
    /// budget, under the budget rules of the harness it runs on (D12).
    #[serde(default)]
    pub budget: BudgetLimits,
    /// The supervisor's device, in hex. The host refuses a `run` whose
    /// `requestedBy` is not the device it came from.
    pub requested_by: String,
    /// The session that delegated the task, when an agent did (D4): the
    /// child records it as its parent. Absent, the child has no parent.
    /// Nothing sends it yet: a session with a parent is read as a subagent
    /// (not bridged for sync), so the sender waits for the ticket that
    /// settles that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sending_session: Option<String>,
    /// A mode stricter than the host's own for the directory, when the
    /// supervisor asks for one. Never looser (D13): the host runs the
    /// stricter of its own and this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// When the supervisor stops waiting for the answer and moves on (D11),
    /// in milliseconds since the Unix epoch: `placement::ANSWER_WAIT` after
    /// it sent the command. Past it, by the host's clock, the host neither
    /// admits the task nor answers `started`, so a task never runs on two
    /// machines because one was slow to answer.
    pub not_after_ms: i64,
}

/// A model as a call may name it (D12): an instance (`claude:work`), a
/// model id, both (the wire's `ModelRef`), or neither.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl From<ModelRef> for ModelChoice {
    fn from(m: ModelRef) -> ModelChoice {
        ModelChoice { instance: Some(m.instance), model: Some(m.model) }
    }
}

/// Where a sealed job is: a transport artifact (`e2e::seal_transport`)
/// stored under `session`, and the key that opens it, which reaches the
/// host only inside this sealed command, never the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRef {
    /// The session the transport is stored under, and bound to.
    pub session: String,
    /// The transport's slug.
    pub transport: String,
    /// The transport's key, 64 hex digits.
    pub key: String,
}

/// Why a host would not run a handed-over task (R-ALLOC-3). Each is a
/// refusal before the child started, so the supervisor moves to its next
/// candidate (D11); `code` is what `HostAnswer::Refused` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunRefusal {
    /// No trusted directory here has the repository's origin (D7).
    NoClone,
    /// No ready instance here for the model asked (D6).
    NoModel,
    /// Every agent slot here is taken (D9).
    NoSlot,
    /// This machine takes no handed-over tasks (`gruntas machines accept off`, D14).
    NotAccepting,
    /// The child's lease was refused as over the person's machine limit (D15).
    MachineLimit,
    /// The job did not open, or did not apply here, or the `run` came past
    /// its `notAfterMs`.
    BadJob,
}

impl RunRefusal {
    pub const ALL: [RunRefusal; 6] = [RunRefusal::NoClone, RunRefusal::NoModel, RunRefusal::NoSlot, RunRefusal::NotAccepting, RunRefusal::MachineLimit, RunRefusal::BadJob];

    pub fn code(self) -> &'static str {
        match self {
            RunRefusal::NoClone => "no_clone",
            RunRefusal::NoModel => "no_model",
            RunRefusal::NoSlot => "no_slot",
            RunRefusal::NotAccepting => "not_accepting",
            RunRefusal::MachineLimit => "machine_limit",
            RunRefusal::BadJob => "bad_job",
        }
    }

    pub fn of(code: &str) -> Option<RunRefusal> {
        RunRefusal::ALL.into_iter().find(|r| r.code() == code)
    }

    /// The answer refusing, with `message` its fix line.
    pub fn answer(self, message: impl Into<String>) -> HostAnswer {
        HostAnswer::refused(self.code(), message)
    }
}

/// The command types a host runs.
const KNOWN: [&str; 8] = ["start", "take", "run", "ping", "read", "changes", "message", "interrupt"];

/// The host's answer to a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum HostAnswer {
    /// The session started: its first turn runs on the host.
    Started { session_id: String },
    /// The session moved here: this host holds it now.
    Taken { session_id: String },
    /// Nothing started, or nothing moved. `message` is the fix line.
    Refused { code: String, message: String },
    /// The answer to a `ping`: empty for a machine, a session's status for
    /// a session.
    Pong {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionPing>,
    },
    /// The answer is in the command's answer record (`open_record`): it
    /// did not fit the 4 KB an answer is.
    Recorded,
    /// The answer to a `read`.
    Page(TranscriptPage),
    /// The answer to a `changes`.
    Changes(ChangesSince),
    /// A `message` or `interrupt` reached the session.
    Delivered { session: String },
}

impl HostAnswer {
    pub fn refused(code: &str, message: impl Into<String>) -> HostAnswer {
        HostAnswer::Refused { code: code.into(), message: message.into() }
    }

    /// Whether this is a host older than the command saying it knows no
    /// such command: said as `NEEDS_UPDATING`.
    pub fn needs_updating(&self) -> bool {
        matches!(self, HostAnswer::Refused { code, .. } if code == refusal::UNKNOWN_COMMAND)
    }

    /// A refusal as the god agent's tools say it, of the machine `name`:
    /// an older host's as "that machine needs updating", any other as its
    /// own fix line. None for an answer that is no refusal.
    pub fn refusal_line(&self, name: &str) -> Option<String> {
        match self {
            _ if self.needs_updating() => Some(format!("{name}: {NEEDS_UPDATING} — its gruntas does not know this question yet; update gruntas there")),
            HostAnswer::Refused { message, .. } => Some(format!("{name}: {message}")),
            _ => None,
        }
    }
}

/// What a command and its answer are bound to: the command's id, lower
/// case as the registry keeps it, then the device it is from and the
/// device it is to. Both ids are 16 bytes, so the id is the rest.
pub fn binding(id: &str, from: &DeviceId, to: &DeviceId) -> Vec<u8> {
    [id.to_ascii_lowercase().as_bytes(), &from.0, &to.0].concat()
}

/// `c` sealed by `me` to the person's devices, for `to` to open, under
/// `id`: a fresh random UUID of the sender's, which the registry keys it by.
pub fn seal_command(c: &HostCommand, id: &str, me: &DeviceId, to: &DeviceId, keys: &UserKeys, current_generation: u32) -> Result<Vec<u8>, String> {
    if let HostCommand::Start { prompt, .. } = c
        && prompt.len() > MAX_PROMPT
    {
        return Err(format!("the prompt is {} bytes, more than a host takes ({MAX_PROMPT})", prompt.len()));
    }
    let json = serde_json::to_vec(c).map_err(|e| format!("the command does not encode: {e}"))?;
    let sealed = e2e::seal_to_person(keys, current_generation, Label::HostCommand, &binding(id, me, to), &json).map_err(|e| e.0)?;
    if sealed.len() > MAX_SEALED {
        return Err(format!("the command seals to {} bytes, more than the registry carries ({MAX_SEALED}) — shorten the prompt", sealed.len()));
    }
    Ok(sealed)
}

/// A command delivered to `me` under `id`, said to be `from`, opened. `Err`
/// is one that does not open, which gets no answer: an answer would need a
/// key the sender proved it holds. `Ok(Err(answer))` opened but cannot run
/// — an unknown `type`, a prompt too long — and `answer` is what to send.
///
/// The sender must be on the verified list `chain`, and not the recovery
/// device, which runs nothing; the command must be sealed under the list's
/// current generation, since a device removed since still holds the older
/// ones.
pub fn open_command(sealed: &[u8], id: &str, from: &DeviceId, me: &DeviceId, keys: &UserKeys, chain: &Chain) -> Result<Result<HostCommand, HostAnswer>, String> {
    // Bounded by what a host takes, not by what today's registry carries:
    // a registry that carries more meets the host's own limit.
    let json = open(sealed, MAX_PROMPT + MAX_SEALED, Label::HostCommand, &binding(id, from, me), from, keys, chain, "command")?;
    let typed: serde_json::Value = serde_json::from_slice(&json).map_err(|_| format!("the command from {from} is not JSON"))?;
    let kind = typed.get("type").and_then(|t| t.as_str()).unwrap_or_default().to_string();
    if !KNOWN.contains(&kind.as_str()) {
        return Ok(Err(HostAnswer::refused("unknown_command", format!("this host does not know the command {kind:?} — upgrade gruntas on it"))));
    }
    let c: HostCommand = match serde_json::from_value(typed) {
        Ok(c) => c,
        Err(e) => return Ok(Err(HostAnswer::refused("bad_command", format!("the {kind} command does not read: {}", e.to_string().escape_debug())))),
    };
    if let HostCommand::Start { prompt, .. } = &c
        && prompt.len() > MAX_PROMPT
    {
        return Ok(Err(HostAnswer::refused("prompt_too_long", format!("the prompt is {} bytes, more than this host takes ({MAX_PROMPT}) — send a shorter one", prompt.len()))));
    }
    // Who the result is for is the device the command is bound to.
    if let HostCommand::Run(RunTask { requested_by, .. }) = &c
        && *requested_by != from.to_string()
    {
        return Ok(Err(HostAnswer::refused("bad_command", format!("the task says it is from {}, not from {from}, which sent it", requested_by.escape_debug()))));
    }
    // Passed on to the registry and to the transport's open.
    if let HostCommand::Run(RunTask { job, .. }) = &c
        && !(session_id_shaped(&job.session) && slug_shaped(&job.transport) && key_shaped(&job.key))
    {
        return Ok(Err(RunRefusal::BadJob.answer("the task's job is not named as a stored job is — send it again from an up-to-date gruntas")));
    }
    // Only ever looked up, but bounded and printable.
    if let HostCommand::Ping { session: Some(s) } = &c
        && (s.is_empty() || s.len() > MAX_SESSION_ID || s.chars().any(char::is_control))
    {
        return Ok(Err(HostAnswer::refused("bad_command", "the ping names no session id this host takes")));
    }
    // Passed on as an argument: only ever a session id.
    if let HostCommand::Take { session_id, .. } = &c
        && !session_id_shaped(session_id)
    {
        return Ok(Err(HostAnswer::refused("bad_command", format!("{:?} is not a session id", session_id.escape_debug().to_string()))));
    }
    if let Some(why) = god_unfit(&c) {
        return Ok(Err(HostAnswer::refused("bad_command", why)));
    }
    Ok(Ok(c))
}

/// Why a god agent's command does not run, as it opens: a session that is
/// no session id (it names a file there), a cursor or text out of bounds,
/// a time that does not read, a steer whose origin is not an agent's (the
/// tools only ever send an agent's, and a person's would pass the
/// `steer` setting unchecked).
fn god_unfit(c: &HostCommand) -> Option<String> {
    let session = |s: &str| (!session_id_shaped(s)).then(|| format!("{:?} is not a session id", s.escape_debug().to_string()));
    match c {
        HostCommand::Read { session: s, page } => session(s).or_else(|| page.as_ref().filter(|p| p.is_empty() || p.len() > MAX_CURSOR || p.chars().any(char::is_control)).map(|_| "the read names no page cursor this host takes".into())),
        HostCommand::Changes { since } => since.parse::<jiff::Timestamp>().is_err().then(|| format!("{:?} is not a time", since.escape_debug().to_string())),
        HostCommand::Message { session: s, text, origin } => session(s).or_else(|| (text.trim().is_empty() || text.len() > MAX_STEER_TEXT).then(|| format!("the message is empty or longer than {MAX_STEER_TEXT} bytes"))).or_else(|| agent_origin(origin)),
        HostCommand::Interrupt { session: s, origin } => session(s).or_else(|| agent_origin(origin)),
        _ => None,
    }
}

fn agent_origin(o: &Origin) -> Option<String> {
    (o.kind != crate::protocol::OriginKind::Agent).then(|| "a steer from another device comes from an agent, and says which".into())
}

/// `a`, `me`'s answer to the command `id` from `to`, sealed for `to`. A
/// refusal's code and message are cut to `MAX_CODE` and `MAX_MESSAGE` bytes
/// of JSON, so it always fits what the registry takes.
pub fn seal_answer(a: &HostAnswer, id: &str, me: &DeviceId, to: &DeviceId, keys: &UserKeys, current_generation: u32) -> Result<Vec<u8>, String> {
    let sealed = seal_any_size(a, id, me, to, keys, current_generation)?;
    if sealed.len() > MAX_ANSWER {
        return Err(format!("the answer seals to {} bytes, more than the registry carries ({MAX_ANSWER})", sealed.len()));
    }
    Ok(sealed)
}

/// `seal_answer` with no limit on the size.
fn seal_any_size(a: &HostAnswer, id: &str, me: &DeviceId, to: &DeviceId, keys: &UserKeys, current_generation: u32) -> Result<Vec<u8>, String> {
    let a = match a {
        HostAnswer::Refused { code, message } => HostAnswer::Refused { code: cut(code, MAX_CODE), message: cut(message, MAX_MESSAGE) },
        done => done.clone(),
    };
    let json = serde_json::to_vec(&a).map_err(|e| format!("the answer does not encode: {e}"))?;
    e2e::seal_to_person(keys, current_generation, Label::HostAnswer, &binding(id, me, to), &json).map_err(|e| e.0)
}

/// `a`, too big for an answer, sealed as the command `id`'s answer record:
/// bound as the answer is, under a label of its own, at most `MAX_RECORD`
/// bytes. A record holds a whole answer, never another `recorded`.
pub fn seal_record(a: &HostAnswer, id: &str, me: &DeviceId, to: &DeviceId, keys: &UserKeys, current_generation: u32) -> Result<Vec<u8>, String> {
    if matches!(a, HostAnswer::Recorded) {
        return Err("an answer record holds an answer, not a pointer to one".into());
    }
    let json = serde_json::to_vec(a).map_err(|e| format!("the answer does not encode: {e}"))?;
    let sealed = e2e::seal_to_person(keys, current_generation, Label::AnswerRecord, &binding(id, me, to), &json).map_err(|e| e.0)?;
    if sealed.len() > MAX_RECORD {
        return Err(format!("the answer seals to {} bytes, more than an answer record carries ({MAX_RECORD})", sealed.len()));
    }
    Ok(sealed)
}

/// The command `id`'s answer record from `from`, the host, opened by `me`:
/// held to the device list as `open_answer` is.
pub fn open_record(sealed: &[u8], id: &str, from: &DeviceId, me: &DeviceId, keys: &UserKeys, chain: &Chain) -> Result<HostAnswer, String> {
    let json = open(sealed, MAX_RECORD, Label::AnswerRecord, &binding(id, from, me), from, keys, chain, "answer record")?;
    match serde_json::from_slice(&json) {
        Ok(HostAnswer::Recorded) => Err(format!("the answer record from {from} points at another")),
        Ok(a) => Ok(a),
        Err(e) => Err(format!("the answer record from {from} does not read: {}", e.to_string().escape_debug())),
    }
}

/// `a` sealed as `seal_answer` seals it when that fits the registry's 4 KB;
/// else the answer is `recorded` and the second bytes are its answer
/// record, which the host writes before it answers.
pub fn seal_answer_or_record(a: &HostAnswer, id: &str, me: &DeviceId, to: &DeviceId, keys: &UserKeys, current_generation: u32) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    let sealed = seal_any_size(a, id, me, to, keys, current_generation)?;
    if sealed.len() <= MAX_ANSWER {
        return Ok((sealed, None));
    }
    let record = seal_record(a, id, me, to, keys, current_generation)?;
    Ok((seal_answer(&HostAnswer::Recorded, id, me, to, keys, current_generation)?, Some(record)))
}

/// The answer `from`, the host, gave `me` to the command `id`, opened: the
/// host must be on the verified list `chain`, as `open_command` holds the
/// sender to it.
pub fn open_answer(sealed: &[u8], id: &str, from: &DeviceId, me: &DeviceId, keys: &UserKeys, chain: &Chain) -> Result<HostAnswer, String> {
    let json = open(sealed, MAX_ANSWER, Label::HostAnswer, &binding(id, from, me), from, keys, chain, "answer")?;
    serde_json::from_slice(&json).map_err(|e| format!("the answer from {from} does not read: {}", e.to_string().escape_debug()))
}

/// The checks both opens share: the device on the list and not the
/// recovery kit, the bytes within `most`, the generation the list's
/// current one, then the seal.
#[allow(clippy::too_many_arguments)]
fn open(sealed: &[u8], most: usize, label: Label, binding: &[u8], from: &DeviceId, keys: &UserKeys, chain: &Chain, what: &str) -> Result<Vec<u8>, String> {
    let listed = chain.devices().iter().find(|d| d.id() == *from).ok_or_else(|| format!("the {what} is from device {from}, which is not on your device list"))?;
    if listed.kind == Kind::Recovery {
        return Err(format!("the {what} is from your recovery kit, which sends nothing"));
    }
    if sealed.len() > most {
        return Err(format!("the {what} from {from} is {} bytes, more than {most}", sealed.len()));
    }
    let generation = e2e::person_generation(sealed).map_err(|e| e.0)?;
    if generation < chain.generation() {
        return Err(format!("the {what} from {from} is sealed under user key generation {generation}, older than the generation {} your device list names current", chain.generation()));
    }
    e2e::open_from_person(sealed, label, binding, keys).map_err(|e| e.0)
}

/// Whether `s` is a session id as text: 8-4-4-4-12 hex digits.
pub fn session_id_shaped(s: &str) -> bool {
    s.len() == 36 && s.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() })
}

/// Whether `s` is a registry slug as text: 1–64 ASCII letters, digits,
/// `_` and `-`. A transport named anywhere (a `run`'s job, a child's
/// `task.result`) is held to it before it is passed on.
pub fn slug_shaped(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `s` is a transport key as text: 64 hex digits.
pub fn key_shaped(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The stricter of two permission modes (D13): what a `run`'s child runs
/// in, of the host's own mode for the directory (already made `default`
/// when it asks nobody, as for `start`) and the `run`'s `permissionMode`.
/// From strictest: `plan`, `default`, `acceptEdits`, `bypassPermissions`,
/// `unhinged`. A `run` can only narrow the host's mode, never loosen it.
pub fn stricter(a: PermissionMode, b: PermissionMode) -> PermissionMode {
    let rank = |m: PermissionMode| match m {
        PermissionMode::Plan => 0,
        PermissionMode::Default => 1,
        PermissionMode::AcceptEdits => 2,
        PermissionMode::BypassPermissions => 3,
        PermissionMode::Unhinged => 4,
    };
    if rank(b) < rank(a) { b } else { a }
}

/// `s` cut so that it takes at most `most` bytes as JSON, escapes
/// included, on a character boundary, with `…` after what is left.
fn cut(s: &str, most: usize) -> String {
    let encoded = |c: char| serde_json::to_string(&c).map_or(6, |j| j.len() - 2);
    if s.chars().map(encoded).sum::<usize>() <= most {
        return s.to_string();
    }
    let room = most.saturating_sub('…'.len_utf8());
    let mut used = 0;
    let kept: String = s.chars().take_while(|&c| {
        used += encoded(c);
        used <= room
    }).collect();
    kept + "…"
}
