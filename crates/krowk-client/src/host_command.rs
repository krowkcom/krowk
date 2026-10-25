//! A command one of the person's devices sends a host, and the host's
//! answer (R-HOST-4): today only `start`, a session in a directory the host
//! announced, on a model it has ready. The registry carries both as bytes
//! (`krowk_api` `send_host_command`, `answer_host_command`), so each is
//! sealed to the person's devices (`e2e::seal_to_person`) and bound to the
//! command's id and the two devices: one moved to another id, or to another
//! pair of devices, does not open.
//!
//! Pure code: the host that runs a command is krowk-harness's presence
//! thread, and the clients that send one are `krowk sync new` and the phone.

use crate::device_chain::{Chain, Kind};
use crate::e2e::{self, DeviceId, Label};
use crate::protocol::ModelRef;
use crate::user_key::UserKeys;
use serde::{Deserialize, Serialize};

/// The longest prompt a `start` carries, the phone's limit too.
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
    Start { cwd: String, model: ModelRef, prompt: String },
}

/// The host's answer to a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum HostAnswer {
    /// The session started: its first turn runs on the host.
    Started { session_id: String },
    /// Nothing started. `message` is the fix line.
    Refused { code: String, message: String },
}

impl HostAnswer {
    pub fn refused(code: &str, message: impl Into<String>) -> HostAnswer {
        HostAnswer::Refused { code: code.into(), message: message.into() }
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
    let HostCommand::Start { prompt, .. } = c;
    if prompt.len() > MAX_PROMPT {
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
    let kind = typed.get("type").and_then(|t| t.as_str()).unwrap_or_default();
    if kind != "start" {
        return Ok(Err(HostAnswer::refused("unknown_command", format!("this host does not know the command {kind:?} — upgrade krowk on it"))));
    }
    let c: HostCommand = match serde_json::from_value(typed) {
        Ok(c) => c,
        Err(e) => return Ok(Err(HostAnswer::refused("bad_command", format!("the start command does not read: {}", e.to_string().escape_debug())))),
    };
    let HostCommand::Start { prompt, .. } = &c;
    if prompt.len() > MAX_PROMPT {
        return Ok(Err(HostAnswer::refused("prompt_too_long", format!("the prompt is {} bytes, more than this host takes ({MAX_PROMPT}) — send a shorter one", prompt.len()))));
    }
    Ok(Ok(c))
}

/// `a`, `me`'s answer to the command `id` from `to`, sealed for `to`. A
/// refusal's code and message are cut to `MAX_CODE` and `MAX_MESSAGE`, so
/// it always fits what the registry takes.
pub fn seal_answer(a: &HostAnswer, id: &str, me: &DeviceId, to: &DeviceId, keys: &UserKeys, current_generation: u32) -> Result<Vec<u8>, String> {
    let a = match a {
        HostAnswer::Refused { code, message } => HostAnswer::Refused { code: cut(code, MAX_CODE), message: cut(message, MAX_MESSAGE) },
        started => started.clone(),
    };
    let json = serde_json::to_vec(&a).map_err(|e| format!("the answer does not encode: {e}"))?;
    let sealed = e2e::seal_to_person(keys, current_generation, Label::HostAnswer, &binding(id, me, to), &json).map_err(|e| e.0)?;
    if sealed.len() > MAX_ANSWER {
        return Err(format!("the answer seals to {} bytes, more than the registry carries ({MAX_ANSWER})", sealed.len()));
    }
    Ok(sealed)
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

/// `s` cut to at most `most` bytes, on a character boundary.
fn cut(s: &str, most: usize) -> String {
    if s.len() <= most {
        return s.to_string();
    }
    let mut end = most.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
