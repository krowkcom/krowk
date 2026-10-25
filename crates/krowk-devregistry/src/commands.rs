//! Commands one of a person's devices sends another (R-HOST-4), as the
//! registry's HostCommand and its two controllers have them: bytes sealed
//! to the target, delivered in the target's presence refresh until it
//! answers, and the answer, sealed to the sender, read back by the sender
//! alone. Ten minutes from sending, on the stand-in's clock, a command is
//! neither delivered nor answered; an hour past that it is gone.
//!
//! Only `id`, `sealed` and `answer` are read, and the stand-in keeps the
//! two devices, the bytes and the times, nothing else (R-E2E-1).

use crate::encode::Json;
use crate::errors::{error, invalid};
use crate::http::{Req, Resp};
use crate::json::Value;
use crate::store::{App, hex, rfc3339_nano};
use crate::sync::{HostCommand, SyncStore, blob};
use jiff::{SignedDuration, Timestamp};

/// HostCommand::SEALED_BYTES, ANSWER_BYTES, LIFETIME, DELIVERED and GRACE.
const SEALED_BYTES: usize = 16 * 1024;
const ANSWER_BYTES: usize = 4 * 1024;
const LIFETIME: SignedDuration = SignedDuration::from_mins(10);
pub const DELIVERED: usize = 16;
const GRACE: SignedDuration = SignedDuration::from_hours(1);

/// POST /v1/users/:user_id/devices/:device_id/commands, signed by the
/// sender: 201 for a new command, 200 with the stored one for the same id,
/// sender, target and bytes again, 409 `command_id_taken` for the id under
/// anything else. The target must be an active device of the person's, else
/// 404 `no_device`.
pub fn send(app: &App, req: &mut Req, target: &str, by: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = req.read_body(1 << 20).map_err(|_| crate::errors::bad_request())?;
        let mut s = app.lock();
        let now = s.now();
        let who = crate::devices::caller_of(&s.sync, req)?;
        crate::sync::burst(&mut s.sync, &crate::sync::caller(req), "host_commands", now)?;
        let target = active_target(&s.sync, &who.person, target)?;
        let given = command_of(&body)?;
        let f = given.fields();
        let id = match f.get_value("id") {
            Some(Value::Str(id)) if is_uuid(id) => id.to_ascii_lowercase(),
            _ => return Err(invalid("id", "id must be a UUID")),
        };
        let sealed = match f.get_value("sealed") {
            Some(Value::Str(h)) => blob("sealed", h, None, Some(SEALED_BYTES))?,
            _ => return Err(invalid("sealed", "must be hex")),
        };
        sweep(&mut s.sync, now);
        if let Some(held) = s.sync.host_commands.get(&id) {
            if held.sender == by && held.target == target && held.sealed == sealed {
                return Ok(Resp::json(200, &created(&id, held)));
            }
            return Err(error(409, "command_id_taken", &format!("command {id} was already sent with other bytes — send a new command with a fresh id"), None));
        }
        let command = HostCommand { sender: by.to_owned(), target, sealed, answer: None, answered_at: None, created_at: now, expires_at: now + LIFETIME };
        let answer = created(&id, &command);
        s.sync.host_commands.insert(id, command);
        Ok(Resp::json(201, &answer))
    };
    run().unwrap_or_else(|r| r)
}

/// GET /v1/users/:user_id/devices/:device_id/commands/:id, signed by the
/// sender, and only the sender: anyone else is 404 `no_command`.
pub fn show(app: &App, req: &Req, target: &str, id: &str, by: &str) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let mut s = app.lock();
        let now = s.now();
        let who = crate::devices::caller_of(&s.sync, req)?;
        let target = active_target(&s.sync, &who.person, target)?;
        sweep(&mut s.sync, now);
        let id = id.to_ascii_lowercase();
        let command = s.sync.host_commands.get(&id).filter(|c| c.sender == by && c.target == target).ok_or_else(|| no_command(&id))?;
        let answer = Json::map([
            ("id", Json::str(&id)),
            ("answered", Json::Raw(command.answer.is_some().to_string())),
            ("answer", command.answer.as_ref().map_or(Json::Null, |a| Json::str(hex(a)))),
            ("expires_at", Json::str(rfc3339_nano(command.expires_at))),
        ]);
        Ok(Resp::json(200, &Json::map([("command", answer)])))
    };
    run().unwrap_or_else(|r| r)
}

/// PUT /v1/key/device/commands/:id, signed by the target, which the key
/// speaks for (`signed_device` refuses any other). The first answer is
/// kept, the same again is 200 and another 409 `command_answered`; a
/// command not addressed to the signer, or lapsed, is 404 `no_command`.
pub fn answer(app: &App, req: &mut Req, id: &str, by: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = req.read_body(1 << 20).map_err(|_| crate::errors::bad_request())?;
        let mut s = app.lock();
        let now = s.now();
        let id = id.to_ascii_lowercase();
        let command = s.sync.host_commands.get_mut(&id).filter(|c| c.target == by && c.expires_at > now).ok_or_else(|| no_command(&id))?;
        let given = command_of(&body)?;
        let f = given.fields();
        let answer = match f.get_value("answer") {
            Some(Value::Str(h)) => blob("answer", h, None, Some(ANSWER_BYTES))?,
            _ => return Err(invalid("answer", "must be hex")),
        };
        match &command.answer {
            Some(kept) if *kept != answer => {
                return Err(error(409, "command_answered", &format!("command {id} was already answered differently — the first answer is kept"), None));
            }
            Some(_) => {}
            None => (command.answer, command.answered_at) = (Some(answer), Some(now)),
        }
        let answered_at = command.answered_at.map_or(Json::Null, |t| Json::str(rfc3339_nano(t)));
        Ok(Resp::json(200, &Json::map([("command", Json::map([("id", Json::str(&id)), ("answered", Json::Raw("true".into())), ("answered_at", answered_at)]))])))
    };
    run().unwrap_or_else(|r| r)
}

/// What a presence refresh carries: the commands waiting for `device`,
/// unanswered and live, oldest first, at most DELIVERED.
pub fn waiting(s: &SyncStore, device: &str, now: Timestamp) -> Json {
    let mut waiting: Vec<(&String, &HostCommand)> = s.host_commands.iter().filter(|(_, c)| c.target == device && c.answer.is_none() && c.expires_at > now).collect();
    waiting.sort_by_key(|(id, c)| (c.created_at, (*id).clone()));
    let waiting = waiting
        .into_iter()
        .take(DELIVERED)
        .map(|(id, c)| {
            Json::map([("id", Json::str(id)), ("from", Json::str(&c.sender)), ("sealed", Json::str(hex(&c.sealed))), ("created_at", Json::str(rfc3339_nano(c.created_at)))])
        })
        .collect();
    Json::Arr(waiting)
}

/// The commands a device sent or was sent go when it stops acting.
pub fn drop_device(s: &mut SyncStore, gone: impl Fn(&str) -> bool) {
    s.host_commands.retain(|_, c| !gone(&c.sender) && !gone(&c.target));
}

/// HostCommand.sweep!, run here on every send and read rather than hourly.
fn sweep(s: &mut SyncStore, now: Timestamp) {
    s.host_commands.retain(|_, c| c.expires_at + GRACE > now);
}

fn active_target(s: &SyncStore, person: &str, target: &str) -> Result<String, Resp> {
    let target = target.to_ascii_lowercase();
    match s.people.get(person).and_then(|p| p.device(&target)) {
        Some(d) if d.active() => Ok(target),
        _ => Err(error(404, "no_device", &format!("no device {target} is on your device list"), None)),
    }
}

fn no_command(id: &str) -> Resp {
    error(404, "no_command", &format!("no command {id} for this device"), None)
}

/// The body's `command`, from the body only, never the query string.
fn command_of(body: &[u8]) -> Result<Value, Resp> {
    let v = crate::json::parse(body).ok_or_else(crate::errors::bad_request)?;
    Ok(v.get("command").map(|m| m.value.clone()).filter(|v| matches!(v, Value::Obj(_))).unwrap_or(Value::Null))
}

fn created(id: &str, c: &HostCommand) -> Json {
    Json::map([(
        "command",
        Json::map([
            ("id", Json::str(id)),
            ("target", Json::str(&c.target)),
            ("created_at", Json::str(rfc3339_nano(c.created_at))),
            ("expires_at", Json::str(rfc3339_nano(c.expires_at))),
            ("answered", Json::Raw(c.answer.is_some().to_string())),
        ]),
    )])
}

/// 8-4-4-4-12 hex digits, either case.
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36 && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

/// `GET /_host_commands`: every command held, by id, with its two devices
/// and whether it is answered, never its bytes — for tests that reach the
/// stand-in only over HTTP, to assert that nothing was sent. Stand-in only.
pub fn commands_seen(app: &App) -> Resp {
    let s = app.lock();
    let mut held: Vec<(&String, &HostCommand)> = s.sync.host_commands.iter().collect();
    held.sort_by_key(|(id, _)| (*id).clone());
    let held = held
        .into_iter()
        .map(|(id, c)| Json::map([("id", Json::str(id)), ("sender", Json::str(&c.sender)), ("target", Json::str(&c.target)), ("answered", Json::Raw(c.answer.is_some().to_string()))]))
        .collect();
    Resp::json(200, &Json::map([("commands", Json::Arr(held))]))
}
