//! Pairing a new machine to a person's devices (canon, engineering/
//! devices.md → Adding a device): the mailbox two devices that cannot yet
//! talk to each other run krowk-client's `pairing` through. The routes,
//! states and refusals are the registry's Api::V1::PairingsController,
//! OpenPairingsController, Pairings::* and Pairing:
//!
//!   A  (initiator)  a device on the person's list, which shows the code
//!   B  (joiner)     a machine signed in as the same person, not yet a device
//!
//! **The steps are krowk-client's, not the registry's.** The registry on
//! disk has A open with its SPAKE2 message and B join with its message and
//! confirmation at once. `pairing.rs` cannot run that way: A's SPAKE2
//! identities bind B's device id, which A learns from B's hello, so B
//! speaks first and there are five messages. Here each is a step, in order,
//! each written once:
//!
//!   1. A opens it, signed                    POST /v1/users/:user_id/pairing  → open
//!   2. B finds it                            GET  /v1/users/:user_id/pairing
//!   3. B joins with its hello                PUT  …/join             joiner_message
//!   4. A answers with its SPAKE2 message     PUT  …/answer           initiator_message
//!   5. B confirms                            PUT  …/confirmation     joiner_confirmation → confirmed
//!   6. A replies, sealed, signed             PUT  …/reply            sealed_reply
//!   7. B acknowledges                        PUT  …/acknowledgement  joiner_ack → done
//!
//! Every blob is opaque here. What the mailbox does is refuse anything out
//! of order and end the pairing at the first failure — either side's
//! DELETE, a second join, a step out of turn or from the wrong side, or ten
//! minutes passing — and a pairing that has ended is never read again
//! (`410 pairing_gone`), so there is no retry against the same code. One is
//! live per person at a time, and a start-over ends it. Once B has joined,
//! the pairing is its two parties' alone — the opening device's keys and
//! the key B joined with — and any other key reads it as not existing. A step is not retry-safe: sent twice, the
//! second is out of turn and ends it, so a side whose answer was lost reads
//! the pairing to see whether its step landed.

use crate::devices::{Caller, caller_of, key_device, signed_device};
use crate::encode::Json;
use crate::errors::{error, not_found, parameter_missing};
use crate::http::{Req, Resp};
use crate::json::Value;
use crate::store::{App, generate_slug, hex, rfc3339_nano};
use crate::sync::{burst, caller, unhex};
use jiff::{SignedDuration, Timestamp};

const LIFETIME: SignedDuration = SignedDuration::from_mins(10);
/// How long an ended pairing stays, so a late poll hears 410 rather than 404.
const GRACE: SignedDuration = SignedDuration::from_hours(1);
const POLL_INTERVAL: i64 = 2;
/// Pairing::MAX_*: the caps bound the rows, not the protocol.
const MAX_MESSAGE_BYTES: usize = 64;
const MAX_CONFIRMATION_BYTES: usize = 1 << 10;
const MAX_REPLY_BYTES: usize = 256 << 10;
const MAX_ACK_BYTES: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Confirmed,
    Done,
    Dead,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Confirmed => "confirmed",
            State::Done => "done",
            State::Dead => "dead",
        }
    }
}

pub struct Pairing {
    pub slug: String,
    pub person: String,
    pub initiator: String,
    pub initiator_name: String,
    pub state: State,
    pub initiator_message: Option<Vec<u8>>,
    pub joiner_message: Option<Vec<u8>>,
    pub joiner_confirmation: Option<Vec<u8>>,
    pub sealed_reply: Option<Vec<u8>>,
    pub joiner_ack: Option<Vec<u8>>,
    /// The key B joined with, by digest: its later steps are that key's.
    pub joiner_key: Option<String>,
    pub expires_at: Timestamp,
    /// When it was opened among the registry's records, so the person's
    /// latest is the one a later open made, whatever the clock says.
    pub order: usize,
}

impl Pairing {
    fn live(&self) -> bool {
        matches!(self.state, State::Open | State::Confirmed)
    }

    fn gone(&self, now: Timestamp) -> bool {
        self.state == State::Dead || self.expires_at <= now
    }
}

fn serialize(p: &Pairing) -> Json {
    let blob = |b: &Option<Vec<u8>>| b.as_ref().map_or(Json::Null, |b| Json::str(hex(b)));
    Json::map([
        ("id", Json::str(&p.slug)),
        ("state", Json::str(p.state.name())),
        ("initiator_device", Json::map([("id", Json::str(&p.initiator)), ("name", Json::str(&p.initiator_name))])),
        ("initiator_message", blob(&p.initiator_message)),
        ("joiner_message", blob(&p.joiner_message)),
        ("joiner_confirmation", blob(&p.joiner_confirmation)),
        ("sealed_reply", blob(&p.sealed_reply)),
        ("joiner_ack", blob(&p.joiner_ack)),
        ("expires_at", Json::str(rfc3339_nano(p.expires_at))),
        ("poll_interval", Json::Int(POLL_INTERVAL)),
    ])
}

fn gone(slug: &str) -> Resp {
    error(410, "pairing_gone", &format!("{slug} has ended — run `krowk devices add` again for a new code"), None)
}

fn out_of_turn(slug: &str) -> Resp {
    error(409, "pairing_out_of_turn", &format!("that step came out of turn, so {slug} has ended — run `krowk devices add` again"), None)
}

fn read_body(req: &mut Req) -> Result<Vec<u8>, Resp> {
    req.read_body(1 << 20).map_err(|_| error(400, "bad_request", "the body could not be read", None))
}

/// `pairing[field]`, hex, within its cap.
fn blob(body: &[u8], field: &str, max: usize) -> Result<Vec<u8>, Resp> {
    let v = crate::json::parse(body).ok_or_else(crate::errors::bad_request)?;
    let pairing = v.get("pairing").map(|m| &m.value).filter(|v| matches!(v, Value::Obj(_))).ok_or_else(|| parameter_missing("pairing"))?;
    let Some(Value::Str(s)) = pairing.get(field).map(|m| &m.value) else { return Err(parameter_missing(field)) };
    let b = unhex(s).ok_or_else(|| crate::errors::invalid(field, "must be hex"))?;
    if b.len() > max {
        return Err(crate::errors::invalid(field, &format!("must be at most {max} bytes")));
    }
    Ok(b)
}

/// POST /v1/users/:user_id/pairing: A's open, signed by a device on the person's list —
/// not the recovery kit. One live pairing per person, else `409
/// pairing_open`; one past its window is ended first.
pub fn open(app: &App, req: &mut Req) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = read_body(req)?;
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        burst(&mut s.sync, &caller(req), "pairings", now)?;
        key_device(&s.sync, &who)?;
        let device = signed_device(&mut s.sync, req, &who, &body)?;
        let listed = s.sync.people[&who.person].device(&device).cloned().expect("a signed device is listed");
        if listed.kind == "recovery" {
            return Err(error(403, "device_mismatch", "a pairing is opened by one of your devices, not the recovery kit", None));
        }
        s.sync.pairings.retain(|_, p| p.expires_at + GRACE > now);
        for p in s.sync.pairings.values_mut().filter(|p| p.person == who.person && p.live() && p.expires_at <= now) {
            p.state = State::Dead;
        }
        if s.sync.pairings.values().any(|p| p.person == who.person && p.live()) {
            return Err(error(409, "pairing_open", "a pairing is already open for your account — finish it, or run `krowk devices add` again after it lapses (10 minutes)", None));
        }
        let p = Pairing {
            slug: generate_slug("pai"),
            person: who.person,
            initiator: device,
            initiator_name: listed.name,
            state: State::Open,
            initiator_message: None,
            joiner_message: None,
            joiner_confirmation: None,
            sealed_reply: None,
            joiner_ack: None,
            joiner_key: None,
            expires_at: now + LIFETIME,
            order: s.sync.seq + 1,
        };
        s.sync.seq += 1;
        let resp = Resp::json(201, &serialize(&p));
        s.sync.pairings.insert(p.slug.clone(), p);
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

/// The person's one pairing — the latest they opened — by slug; empty when
/// they have none. Every call under `/v1/users/:user_id/pairing` is about it.
pub fn latest(app: &App, req: &Req) -> String {
    let person = crate::store::person_for(&crate::auth::token(req).unwrap_or_default());
    let s = app.lock();
    s.sync.pairings.values().filter(|p| p.person == person).max_by_key(|p| p.order).map(|p| p.slug.clone()).unwrap_or_default()
}

/// GET /v1/users/:user_id/pairing: to either party, their pairing as
/// `show` reads it; to anyone else of the person, the open one still
/// waiting for a joiner, as `find_open` finds it.
pub fn read(app: &App, req: &Req) -> Resp {
    let slug = latest(app, req);
    let is_party = {
        let s = app.lock();
        caller_of(&s.sync, req).ok().zip(s.sync.pairings.get(&slug)).is_some_and(|(who, p)| party(&s.sync, &who, p))
    };
    if is_party { show(app, req, &slug) } else { find_open(app, req) }
}

/// B's way in, the one open pairing of the person
/// the new machine signed in as, still waiting for a joiner. The code is
/// never sent here: it is how the two sides check each other, not how B
/// finds the pairing. A key alone: B is not a device yet.
fn find_open(app: &App, req: &Req) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        let found = s.sync.pairings.values().find(|p| p.person == who.person && p.state == State::Open && p.joiner_message.is_none() && p.expires_at > now);
        found.map(|p| Resp::json(200, &serialize(p))).ok_or_else(|| {
            error(404, "no_pairing", "none of your devices has a pairing open — run `krowk devices add` on one of them first", None)
        })
    };
    run().unwrap_or_else(|r| r)
}

/// The person's own pairing by id: another person's reads as not existing.
fn find<'a>(s: &'a mut crate::sync::SyncStore, who: &Caller, slug: &str) -> Result<&'a mut Pairing, Resp> {
    s.pairings.get_mut(slug).filter(|p| p.person == who.person).ok_or_else(not_found)
}

/// A pairing is its two parties' and nobody else's, even among the
/// person's own keys: a key of the device that opened it, and the key B
/// joined with. Any other reads as not existing (PairingCalls#
/// pairing_of_a_party).
fn party(s: &crate::sync::SyncStore, who: &Caller, p: &Pairing) -> bool {
    s.bindings.get(&who.key) == Some(&p.initiator) || p.joiner_key.as_deref() == Some(who.key.as_str())
}

fn find_as_party<'a>(s: &'a mut crate::sync::SyncStore, who: &Caller, slug: &str) -> Result<&'a mut Pairing, Resp> {
    let found = s.pairings.get(slug).filter(|p| p.person == who.person).ok_or_else(not_found)?;
    if !party(s, who, found) {
        return Err(not_found());
    }
    find(s, who, slug)
}

/// Either side's poll. A finished pairing reads
/// until its window closes, so A's poll sees the ack that finished it; a
/// failed or lapsed one never does.
fn show(app: &App, req: &Req, slug: &str) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        let p = find_as_party(&mut s.sync, &who, slug)?;
        if p.gone(now) {
            p.state = State::Dead;
            return Err(gone(slug));
        }
        Ok(Resp::json(200, &serialize(p)))
    };
    run().unwrap_or_else(|r| r)
}

/// DELETE /v1/users/:user_id/pairing: either side's failure report — a MAC that did
/// not check, a person who said no, a ^C. Ended at once, and for good.
pub fn destroy(app: &App, req: &Req, slug: &str) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        let p = find_as_party(&mut s.sync, &who, slug)?;
        if p.gone(now) || p.state == State::Done {
            p.state = State::Dead;
            return Err(gone(slug));
        }
        p.state = State::Dead;
        Ok(Resp::empty(204))
    };
    run().unwrap_or_else(|r| r)
}

/// Which side a step is: B by the key it joined with, A by the device that
/// opened the pairing, signed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Joiner,
    Initiator,
}

/// One step: the side checked, the pairing not ended, the step in turn —
/// else the pairing is dead — and then written once.
pub fn step(app: &App, req: &mut Req, slug: &str, side: Side, field: &'static str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = read_body(req)?;
        let max = match field {
            "joiner_message" | "initiator_message" => MAX_MESSAGE_BYTES,
            "joiner_confirmation" => MAX_CONFIRMATION_BYTES,
            "sealed_reply" => MAX_REPLY_BYTES,
            _ => MAX_ACK_BYTES,
        };
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        if field == "joiner_message" {
            burst(&mut s.sync, &caller(req), "pairing_joins", now)?;
        }
        let signer = if side == Side::Initiator {
            key_device(&s.sync, &who)?;
            Some(signed_device(&mut s.sync, req, &who, &body)?)
        } else {
            None
        };
        let bytes = blob(&body, field, max)?;
        let p = if field == "joiner_message" { find(&mut s.sync, &who, slug)? } else { find_as_party(&mut s.sync, &who, slug)? };
        if p.gone(now) || p.state == State::Done {
            p.state = State::Dead;
            return Err(gone(slug));
        }
        let from_side = match side {
            Side::Initiator => signer.as_deref() == Some(p.initiator.as_str()),
            Side::Joiner => field == "joiner_message" || p.joiner_key.as_deref() == Some(who.key.as_str()),
        };
        let in_turn = from_side
            && match field {
                "joiner_message" => p.state == State::Open && p.joiner_message.is_none(),
                "initiator_message" => p.state == State::Open && p.joiner_message.is_some() && p.initiator_message.is_none(),
                "joiner_confirmation" => p.state == State::Open && p.initiator_message.is_some() && p.joiner_confirmation.is_none(),
                "sealed_reply" => p.state == State::Confirmed && p.sealed_reply.is_none(),
                _ => p.state == State::Confirmed && p.sealed_reply.is_some() && p.joiner_ack.is_none(),
            };
        if !in_turn {
            p.state = State::Dead;
            return Err(out_of_turn(slug));
        }
        match field {
            "joiner_message" => {
                p.joiner_message = Some(bytes);
                p.joiner_key = Some(who.key.clone());
            }
            "initiator_message" => p.initiator_message = Some(bytes),
            "joiner_confirmation" => {
                p.joiner_confirmation = Some(bytes);
                p.state = State::Confirmed;
            }
            "sealed_reply" => p.sealed_reply = Some(bytes),
            _ => {
                p.joiner_ack = Some(bytes);
                p.state = State::Done;
            }
        }
        Ok(Resp::json(200, &serialize(p)))
    };
    run().unwrap_or_else(|r| r)
}
