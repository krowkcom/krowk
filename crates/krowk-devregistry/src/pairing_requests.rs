//! A new machine asking the person's hosts to open a pairing (canon,
//! engineering/devices.md → Adding a device). The routes, states and
//! refusals are the registry's Api::V1::Users::PairingRequestsController
//! and PairingRequest:
//!
//!   POST /v1/users/:user_id/pairing_request   a signed-in machine that is not a
//!                                             device records its name and OS
//!   GET  /v1/users/:user_id/pairing_request   that machine's poll: pending,
//!                                             pairing_open or expired
//!
//! The hosts read pending requests in their presence refresh
//! (`pairing_requests`, `waiting`). A request opens nothing and carries no
//! keys: name and OS are plaintext the machine chose, shown as a hint only.
//! It lapses ten minutes after it is made; a second while one is pending is
//! `409 pairing_request_pending`; and a pairing opening for the person
//! answers every request waiting (`answered`). The phone push is recorded in
//! `SyncStore::notifications` as kind `new_machine`, with no session, no
//! device and nothing of the request in it.

use crate::devices::{caller_of, no_device_list};
use crate::encode::Json;
use crate::errors::{error, invalid};
use crate::http::{Req, Resp};
use crate::json::Value;
use crate::store::{App, generate_slug, rfc3339_nano};
use crate::sync::{Notified, SyncStore, burst, caller};
use jiff::{SignedDuration, Timestamp};

const LIFETIME: SignedDuration = SignedDuration::from_mins(10);
/// How long an expired request stays, so a late poll reads expired.
const GRACE: SignedDuration = SignedDuration::from_hours(1);
/// What a presence refresh carries at most (PairingRequest::DELIVERED).
const DELIVERED: usize = 4;
const POLL_INTERVAL: i64 = 2;
const MAX_NAME_CHARS: usize = 100;
const MAX_OS_CHARS: usize = 32;

#[derive(Clone)]
pub struct PairingRequest {
    pub slug: String,
    pub person: String,
    /// The key the machine signed in with, by digest: its poll is that key's.
    pub key: String,
    pub name: String,
    pub os: String,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    /// Set when a pairing opens for the person after the request.
    pub opened: bool,
    pub order: usize,
}

impl PairingRequest {
    fn pending(&self, now: Timestamp) -> bool {
        !self.opened && self.expires_at > now
    }

    fn state(&self, now: Timestamp) -> &'static str {
        if self.opened {
            "pairing_open"
        } else if self.expires_at <= now {
            "expired"
        } else {
            "pending"
        }
    }
}

fn serialize(r: &PairingRequest, now: Timestamp) -> Json {
    Json::map([
        ("id", Json::str(&r.slug)),
        ("state", Json::str(r.state(now))),
        ("name", Json::str(&r.name)),
        ("os", Json::str(&r.os)),
        ("created_at", Json::str(rfc3339_nano(r.created_at))),
        ("expires_at", Json::str(rfc3339_nano(r.expires_at))),
        ("poll_interval", Json::Int(POLL_INTERVAL)),
    ])
}

/// What a presence refresh carries: the person's pending requests, oldest
/// first, each with only its id, name, OS and times.
pub fn waiting(s: &SyncStore, person: &str, now: Timestamp) -> Json {
    let mut pending: Vec<&PairingRequest> = s.pairing_requests.values().filter(|r| r.person == person && r.pending(now)).collect();
    pending.sort_by_key(|r| r.order);
    let items = pending
        .into_iter()
        .take(DELIVERED)
        .map(|r| {
            Json::map([
                ("id", Json::str(&r.slug)),
                ("name", Json::str(&r.name)),
                ("os", Json::str(&r.os)),
                ("created_at", Json::str(rfc3339_nano(r.created_at))),
                ("expires_at", Json::str(rfc3339_nano(r.expires_at))),
            ])
        })
        .collect();
    Json::Arr(items)
}

/// A pairing opening for the person answers the requests waiting on it
/// (PairingRequest.answered!).
pub fn answered(s: &mut SyncStore, person: &str, now: Timestamp) {
    for r in s.pairing_requests.values_mut().filter(|r| r.person == person && r.pending(now)) {
        r.opened = true;
    }
}

/// One text field of the body's `pairing_request`: a string, trimmed, not
/// blank, within its length, with no control characters (they are printed on
/// other devices' terminals).
fn text(given: &Value, field: &str, max: usize) -> Result<String, Resp> {
    let Some(Value::Str(s)) = given.get(field).map(|m| &m.value) else { return Err(invalid(field, "must be a string")) };
    let s = s.trim().to_owned();
    if s.is_empty() {
        return Err(invalid(field, "can't be blank"));
    }
    if s.chars().count() > max {
        return Err(invalid(field, &format!("is too long (maximum is {max} characters)")));
    }
    if s.chars().any(char::is_control) {
        return Err(invalid(field, "cannot hold control characters"));
    }
    Ok(s)
}

/// POST /v1/users/:user_id/pairing_request. The key alone: a key that speaks
/// for a device is a device, and a person with no device list has nobody to
/// ask. One is pending per person, else `409 pairing_request_pending`.
pub fn create(app: &App, req: &mut Req) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = req.read_body(1 << 20).map_err(|_| error(400, "bad_request", "the body could not be read", None))?;
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        burst(&mut s.sync, &caller(req), "pairing_requests", now)?;
        if s.sync.bindings.contains_key(&who.key) {
            return Err(error(403, "device_mismatch", "this key already speaks for a device on your list — a pairing is asked for by a machine signed in there that is not on it", None));
        }
        if !s.sync.people.get(&who.person).is_some_and(|p| !p.entries.is_empty()) {
            return Err(no_device_list());
        }
        let v = crate::json::parse(&body).ok_or_else(crate::errors::bad_request)?;
        let given = v.get("pairing_request").map(|m| m.value.clone()).filter(|v| matches!(v, Value::Obj(_))).unwrap_or(Value::Null);
        let (name, os) = (text(&given, "name", MAX_NAME_CHARS)?, text(&given, "os", MAX_OS_CHARS)?);
        s.sync.pairing_requests.retain(|_, r| r.expires_at + GRACE > now);
        if s.sync.pairing_requests.values().any(|r| r.person == who.person && r.pending(now)) {
            return Err(error(409, "pairing_request_pending", "another machine's pairing request is still waiting — it lapses in 10 minutes, or when a pairing opens for it", None));
        }
        // A pairing already open is the one this machine joins: answered at
        // once, and no host is nudged to open another.
        let opened = s.sync.pairings.values().any(|p| p.person == who.person && p.live_at(now));
        s.sync.seq += 1;
        let r = PairingRequest { slug: generate_slug("prq"), person: who.person.clone(), key: who.key, name, os, created_at: now, expires_at: now + LIFETIME, opened, order: s.sync.seq };
        let resp = Resp::json(201, &Json::map([("pairing_request", serialize(&r, now))]));
        s.sync.pairing_requests.insert(r.slug.clone(), r);
        // The phone's push says only that a new machine signed in.
        s.sync.notifications.push(Notified { session: String::new(), kind: "new_machine".into(), dedup: String::new(), from_device: String::new(), at: now });
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

/// GET /v1/users/:user_id/pairing_request: this key's latest request.
pub fn show(app: &App, req: &Req) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        let latest = s.sync.pairing_requests.values().filter(|r| r.person == who.person && r.key == who.key).max_by_key(|r| r.order);
        match latest {
            Some(r) => Ok(Resp::json(200, &Json::map([("pairing_request", serialize(r, now))]))),
            None => Err(error(404, "no_pairing_request", "this machine has asked for no pairing", None)),
        }
    };
    run().unwrap_or_else(|r| r)
}
