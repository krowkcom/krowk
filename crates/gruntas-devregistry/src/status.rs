//! The status layer's registry records (R-STA-5, R-STA-6), standing in for
//! the registry's (gruntas/cloud `registry/`, ticket SL02): each device's
//! sealed status board, latest only, its bytes never opened; and each
//! machine's counts under a random statistics id, which holds the machine's
//! health and its agents per A2A state and per health status, and nothing
//! else. The wire contract is `gruntas_api::sync`'s `put_status_board`,
//! `status_board`, `put_statistics` and `statistics`.

use crate::devices::{caller_of, key_device, signed_device};
use crate::encode::Json;
use crate::errors::{error, invalid};
use crate::http::{Req, Resp};
use crate::json::{Member, Value};
use crate::store::{App, hex, rfc3339_nano};
use crate::sync::{blob, burst, caller};
use jiff::Timestamp;

/// The most sealed bytes a board takes (D8), as `status::MAX_SEALED`.
const BOARD_SEALED_BYTES: usize = gruntas_client::status::MAX_SEALED;
/// More agents than one machine counts in any state.
const MOST_COUNTED: u64 = 1_000_000;
const TASK_STATES: [&str; 9] = [
    "TASK_STATE_UNSPECIFIED",
    "TASK_STATE_SUBMITTED",
    "TASK_STATE_WORKING",
    "TASK_STATE_COMPLETED",
    "TASK_STATE_FAILED",
    "TASK_STATE_CANCELED",
    "TASK_STATE_INPUT_REQUIRED",
    "TASK_STATE_REJECTED",
    "TASK_STATE_AUTH_REQUIRED",
];
const HEALTH: [&str; 3] = ["SERVING", "NOT_SERVING", "UNKNOWN"];

/// A device's board as the registry keeps it: the bytes, opaque, and when
/// they were published. Nothing else (R-E2E-1).
#[derive(Clone, Debug, PartialEq)]
pub struct Board {
    pub sealed: Vec<u8>,
    pub updated_at: Timestamp,
}

/// A machine's counts as the registry keeps them: the health and the two
/// tallies, by name, and when they were sent. Not tied to a device.
#[derive(Clone, Debug, PartialEq)]
pub struct Statistics {
    pub health: String,
    pub states: Vec<(String, u64)>,
    pub health_counts: Vec<(String, u64)>,
    pub updated_at: Timestamp,
}

fn read_body(req: &mut Req) -> Result<Vec<u8>, Resp> {
    req.read_body(1 << 20).map_err(|_| error(400, "bad_request", "the body could not be read", None))
}

fn no_status_board() -> Resp {
    error(404, "no_status_board", "that device has published no status board", None)
}

fn no_statistics() -> Resp {
    error(404, "no_statistics", "nothing was sent under that statistics id", None)
}

/// PUT /v1/key/device/status_board: the sealed board of the device the key
/// speaks for, signed by it, in place of the last.
pub fn put_board(app: &App, req: &mut Req) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = read_body(req)?;
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        let bound = key_device(&s.sync, &who)?;
        let device = signed_device(&mut s.sync, req, &who, &body)?;
        if device != bound {
            return Err(error(403, "device_mismatch", &format!("this key speaks for device {bound} but the request is signed by device {device}"), None));
        }
        burst(&mut s.sync, &caller(req), "status_boards", now)?;
        let v = crate::json::parse(&body).ok_or_else(crate::errors::bad_request)?;
        let given = v.get("status_board").map(|m| m.value.clone()).filter(|v| matches!(v, Value::Obj(_))).unwrap_or(Value::Null);
        let sealed = match given.fields().get_value("sealed") {
            Some(Value::Str(h)) => {
                if h.len() > 2 * BOARD_SEALED_BYTES {
                    return Err(error(413, "status_board_too_large", &format!("a status board is at most {BOARD_SEALED_BYTES} bytes sealed"), None));
                }
                blob("sealed", h, None, None)?
            }
            _ => return Err(invalid("sealed", "must be hex")),
        };
        s.sync.status_boards.insert(device.clone(), Board { sealed, updated_at: now });
        Ok(Resp::json(200, &Json::map([("status_board", Json::map([("device", Json::str(&device)), ("updated_at", Json::str(rfc3339_nano(now)))]))])))
    };
    run().unwrap_or_else(|r| r)
}

/// GET /v1/users/:user_id/devices/:id/status_board: the latest board of one
/// of the person's active devices. Not signed, but the key's own device
/// must be active, as for presences.
pub fn show_board(app: &App, req: &Req, id: &str) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let s = app.lock();
        let who = caller_of(&s.sync, req)?;
        key_device(&s.sync, &who)?;
        let person = s.sync.people.get(&who.person).cloned().unwrap_or_default();
        let device = person.devices.iter().find(|d| d.active() && d.id.eq_ignore_ascii_case(id)).ok_or_else(no_status_board)?;
        let b = s.sync.status_boards.get(&device.id).ok_or_else(no_status_board)?;
        let record = Json::map([("device", Json::str(&device.id)), ("sealed", Json::str(hex(&b.sealed))), ("updated_at", Json::str(rfc3339_nano(b.updated_at)))]);
        Ok(Resp::json(200, &Json::map([("status_board", record)])))
    };
    run().unwrap_or_else(|r| r)
}

fn statistics_id(id: &str) -> Result<(), Resp> {
    if gruntas_client::status::is_statistics_id(id) {
        Ok(())
    } else {
        Err(invalid("statistics_id", "must be 32 lower-case hex digits"))
    }
}

/// A tally: an object of names from `allowed` to whole numbers.
fn tally(field: &str, v: Option<&Value>, allowed: &[&str]) -> Result<Vec<(String, u64)>, Resp> {
    let members: &[Member] = match v {
        None | Some(Value::Null) => &[],
        Some(Value::Obj(m)) => m,
        _ => return Err(invalid(field, "must be an object of counts")),
    };
    let mut out: Vec<(String, u64)> = Vec::new();
    for m in members {
        if !allowed.contains(&m.key.as_str()) || out.iter().any(|(k, _)| *k == m.key) {
            return Err(invalid(field, &format!("{} is not a name it counts", m.key.escape_debug())));
        }
        let n = match &m.value {
            Value::Num(n) => n.parse::<u64>().ok().filter(|n| *n <= MOST_COUNTED),
            _ => None,
        };
        out.push((m.key.clone(), n.ok_or_else(|| invalid(field, "must count in whole numbers"))?));
    }
    out.sort();
    Ok(out)
}

/// PUT /v1/users/:user_id/statistics/:id: a machine's counts, latest only.
/// The key's alone: no device signs it. Anything beside the three fields
/// is refused rather than dropped, so nothing else is ever stored.
pub fn put_statistics(app: &App, req: &mut Req, id: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        statistics_id(id)?;
        let body = read_body(req)?;
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        key_device(&s.sync, &who)?;
        burst(&mut s.sync, &caller(req), "statistics", now)?;
        let v = crate::json::parse(&body).ok_or_else(crate::errors::bad_request)?;
        let Some(Value::Obj(given)) = v.get("statistics").map(|m| &m.value) else {
            return Err(invalid("statistics", "must be an object"));
        };
        if let Some(m) = given.iter().find(|m| !["health", "states", "health_counts"].contains(&m.key.as_str())) {
            return Err(invalid("statistics", &format!("{} is not a field it takes", m.key.escape_debug())));
        }
        let f = |k: &str| given.iter().find(|m| m.key == k).map(|m| &m.value);
        let health = match f("health") {
            Some(Value::Str(h)) if HEALTH.contains(&h.as_str()) => h.clone(),
            _ => return Err(invalid("health", "must be SERVING, NOT_SERVING or UNKNOWN")),
        };
        let states = tally("states", f("states"), &TASK_STATES)?;
        let health_counts = tally("health_counts", f("health_counts"), &HEALTH)?;
        s.sync.statistics.insert((who.person.clone(), id.to_owned()), Statistics { health, states, health_counts, updated_at: now });
        Ok(Resp::json(200, &Json::map([("statistics", Json::map([("id", Json::str(id)), ("updated_at", Json::str(rfc3339_nano(now)))]))])))
    };
    run().unwrap_or_else(|r| r)
}

fn counts_json(c: &[(String, u64)]) -> Json {
    Json::map_of(c.iter().map(|(k, n)| (k.clone(), Json::Int(*n as i64))).collect())
}

fn statistics_json(id: &str, st: &Statistics) -> Json {
    Json::map([
        ("id", Json::str(id)),
        ("health", Json::str(&st.health)),
        ("states", counts_json(&st.states)),
        ("health_counts", counts_json(&st.health_counts)),
        ("updated_at", Json::str(rfc3339_nano(st.updated_at))),
    ])
}

/// GET /v1/users/:user_id/statistics/:id: the counts last sent under it.
pub fn show_statistics(app: &App, req: &Req, id: &str) -> Resp {
    let run = || -> Result<Resp, Resp> {
        statistics_id(id)?;
        let s = app.lock();
        let who = caller_of(&s.sync, req)?;
        key_device(&s.sync, &who)?;
        let st = s.sync.statistics.get(&(who.person.clone(), id.to_owned())).ok_or_else(no_statistics)?;
        Ok(Resp::json(200, &Json::map([("statistics", statistics_json(id, st))])))
    };
    run().unwrap_or_else(|r| r)
}

/// `GET /_status_layer`: every board and every count the stand-in holds,
/// as stored, for a test to see that nothing beside the sealed bytes and
/// the counts was kept (R-STA-6). Stand-in only.
pub fn seen(app: &App) -> Resp {
    let s = app.lock();
    let mut boards: Vec<(&String, &Board)> = s.sync.status_boards.iter().collect();
    boards.sort_by_key(|(d, _)| (*d).clone());
    let boards = boards.into_iter().map(|(d, b)| Json::map([("device", Json::str(d)), ("sealed", Json::str(hex(&b.sealed))), ("updated_at", Json::str(rfc3339_nano(b.updated_at)))])).collect();
    let mut stats: Vec<(&(String, String), &Statistics)> = s.sync.statistics.iter().collect();
    stats.sort_by_key(|(k, _)| (*k).clone());
    let stats = stats.into_iter().map(|((_, id), st)| statistics_json(id, st)).collect();
    Resp::json(200, &Json::map([("status_boards", Json::Arr(boards)), ("statistics", Json::Arr(stats))]))
}
