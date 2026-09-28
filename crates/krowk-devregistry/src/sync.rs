//! Sync: the devices that hold a workspace's account key, the mailbox a new
//! device is approved through, and sessions with their leases — the shapes
//! and refusals of the registry's Api::V1::DevicesController,
//! DeviceApprovalsController, SessionsController and Sessions::
//! LeasesController.
//!
//! Every endpoint needs a key and refuses a free workspace (R-SYNC-1); a
//! token containing `free` is one, as for uploads. Keys and sealed blobs are
//! hex of the sizes canon's crypto.md fixes, and anything else a body carries
//! is not read, so there is nowhere for plaintext to land (R-E2E-1).

use crate::auth::{free_plan, require_key};
use crate::encode::Json;
use crate::errors::{Decoded, decode, error, invalid, not_found, parameter_missing};
use crate::http::{Req, Resp};
use crate::json::{Fields, Value};
use crate::store::{App, generate_slug, hex, rfc3339_nano};
use jiff::{SignedDuration, Timestamp};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const PUBLIC_KEY_BYTES: usize = 32;
const WRAPPED_ACCOUNT_KEY_BYTES: usize = 82;
const WRAPPED_SESSION_KEY_BYTES: usize = 74;
const MAX_SEALED_INDEX_BYTES: usize = 64 << 10;
const APPROVAL_LIFETIME: SignedDuration = SignedDuration::from_mins(15);
const DEFAULT_LEASE_TTL: i64 = 60;
const LEASE_TTL: std::ops::RangeInclusive<i64> = 10..=600;

pub struct Device {
    pub id: String,
    pub public_key: String,
    pub name: String,
    pub created_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub wrapped_account_key: String,
    pub seq: usize,
}

pub struct Approval {
    pub slug: String,
    pub workspace: String,
    pub id: String,
    pub public_key: String,
    pub name: String,
    pub approved: bool,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub approved_by: String,
    pub account_key_id: String,
    pub wrapped_account_key: String,
}

pub struct Session {
    pub id: String,
    pub wrapped_key: String,
    pub sealed_index: String,
    pub fence: u64,
    pub holder: String,
    pub lease_expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub last_written_at: Timestamp,
    pub seq: usize,
}

#[derive(Default)]
pub struct SyncStore {
    /// Workspace → the account key id its first device offered.
    pub account_keys: HashMap<String, String>,
    pub devices: HashMap<(String, String), Device>,
    pub approvals: HashMap<String, Approval>,
    pub sessions: HashMap<(String, String), Session>,
    pub seq: usize,
}

/// crypto.md's device id: the first 16 bytes of
/// `SHA-256("krowk/device-id/v1" ‖ public key)`, hex.
fn fingerprint(public_key: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"krowk/device-id/v1");
    h.update(public_key);
    hex(&h.finalize()[..16])
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// A hex field of a fixed size (or at most `max`), refused as the registry's
/// Sync::Malformed is: `invalid`, naming the field.
fn blob(field: &str, value: &str, bytes: Option<usize>, max: Option<usize>) -> Result<Vec<u8>, Resp> {
    let raw = unhex(value).ok_or_else(|| invalid(field, "must be hex"))?;
    if let Some(n) = bytes
        && raw.len() != n
    {
        return Err(invalid(field, &format!("must be {n} bytes")));
    }
    if let Some(n) = max
        && raw.len() > n
    {
        return Err(invalid(field, &format!("must be at most {n} bytes")));
    }
    Ok(raw)
}

fn id_field(field: &str, value: &str) -> Result<String, Resp> {
    let id = value.to_ascii_lowercase();
    if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) { Ok(id) } else { Err(invalid(field, "must be 32 hex characters")) }
}

/// The paid gate every sync endpoint runs after the key.
fn gate(req: &Req) -> Result<String, Resp> {
    let workspace = require_key(req)?;
    if free_plan(req) {
        return Err(error(
            402,
            "sync_requires_paid_plan",
            "Syncing sessions between devices requires a paid plan. Upgrade this workspace to Pro; everything else krowk does keeps working locally without it.",
            None,
        ));
    }
    Ok(workspace)
}

/// The body's `resource` object, and a 400 for a missing one.
fn body(req: &mut Req, resource: &str) -> Result<Value, Resp> {
    match decode(req, 1 << 20)? {
        Decoded::Value(v) => match v.get(resource) {
            Some(m) if matches!(m.value, Value::Obj(_)) => Ok(m.value.clone()),
            _ => Err(parameter_missing(resource)),
        },
        _ => Err(parameter_missing(resource)),
    }
}

fn required(f: &mut Fields, name: &str) -> Result<String, Resp> {
    let v = f.string(name);
    if v.is_empty() { Err(parameter_missing(name)) } else { Ok(v) }
}

/// A non-negative integer field, or None when it was not sent.
fn number(f: &Fields, name: &str) -> Option<i64> {
    f.get_num(name)
}

fn account_key_mismatch(expected: &str) -> Resp {
    error(
        409,
        "account_key_mismatch",
        &format!("This workspace's devices hold account key {expected}, not the one offered. If this came from a recovery phrase, a word is wrong: recover again with the right phrase."),
        Some(Json::map([("account_key_id", Json::str(expected))])),
    )
}

fn adopt_account_key(s: &mut SyncStore, workspace: &str, id: &str) -> Result<(), Resp> {
    match s.account_keys.get(workspace) {
        Some(held) if held != id => Err(account_key_mismatch(held)),
        Some(_) => Ok(()),
        None => {
            s.account_keys.insert(workspace.to_owned(), id.to_owned());
            Ok(())
        }
    }
}

fn serialize_device(d: &Device) -> Json {
    Json::map([
        ("id", Json::str(&d.id)),
        ("public_key", Json::str(&d.public_key)),
        ("name", Json::str(&d.name)),
        ("created_at", Json::str(rfc3339_nano(d.created_at))),
        ("last_seen_at", Json::str(rfc3339_nano(d.last_seen_at))),
        ("revoked_at", Json::Null),
    ])
}

fn serialize_approval(a: &Approval) -> Json {
    let opt = |s: &str| if s.is_empty() { Json::Null } else { Json::str(s) };
    Json::map([
        ("slug", Json::str(&a.slug)),
        ("id", Json::str(&a.id)),
        ("public_key", Json::str(&a.public_key)),
        ("name", Json::str(&a.name)),
        ("state", Json::str(if a.approved { "approved" } else { "pending" })),
        ("expires_at", Json::str(rfc3339_nano(a.expires_at))),
        ("created_at", Json::str(rfc3339_nano(a.created_at))),
        ("approved_by", opt(&a.approved_by)),
        ("account_key_id", opt(&a.account_key_id)),
        ("wrapped_account_key", opt(&a.wrapped_account_key)),
    ])
}

fn leased(s: &Session, now: Timestamp) -> bool {
    !s.holder.is_empty() && s.lease_expires_at.is_some_and(|e| e > now)
}

fn serialize_lease(s: &Session) -> Json {
    Json::map([
        ("session", Json::str(&s.id)),
        ("device", if s.holder.is_empty() { Json::Null } else { Json::str(&s.holder) }),
        ("fence", Json::Int(s.fence as i64)),
        ("expires_at", s.lease_expires_at.map_or(Json::Null, |e| Json::str(rfc3339_nano(e)))),
    ])
}

fn serialize_session(s: &Session, now: Timestamp) -> Json {
    Json::map([
        ("id", Json::str(&s.id)),
        ("wrapped_key", Json::str(&s.wrapped_key)),
        ("sealed_index", if s.sealed_index.is_empty() { Json::Null } else { Json::str(&s.sealed_index) }),
        ("sealed_index_size", Json::Int((s.sealed_index.len() / 2) as i64)),
        ("fence", Json::Int(s.fence as i64)),
        ("lease", if leased(s, now) { serialize_lease(s) } else { Json::Null }),
        ("created_at", Json::str(rfc3339_nano(s.created_at))),
        ("updated_at", Json::str(rfc3339_nano(s.updated_at))),
        ("last_written_at", Json::str(rfc3339_nano(s.last_written_at))),
    ])
}

pub fn list_devices(app: &App, req: &Req) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    let mut devices: Vec<&Device> = s.sync.devices.iter().filter(|((w, _), _)| *w == workspace).map(|(_, d)| d).collect();
    devices.sort_by_key(|d| d.seq);
    Resp::json(200, &Json::map([("devices", Json::Arr(devices.into_iter().map(serialize_device).collect()))]))
}

/// A device that set sync up itself says so, with the account key it holds.
/// The same public key again is the same device, renamed.
pub fn register_device(app: &App, req: &mut Req) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let v = body(req, "device")?;
        let mut f = v.fields();
        let public_key = required(&mut f, "public_key")?;
        let key = blob("public_key", &public_key, Some(PUBLIC_KEY_BYTES), None)?;
        let name = required(&mut f, "name")?;
        let account = id_field("account_key_id", &required(&mut f, "account_key_id")?)?;
        let mut s = app.lock();
        let now = s.now();
        adopt_account_key(&mut s.sync, &workspace, &account)?;
        let id = fingerprint(&key);
        let seq = s.sync.seq + 1;
        let fresh = !s.sync.devices.contains_key(&(workspace.clone(), id.clone()));
        let d = s.sync.devices.entry((workspace, id.clone())).or_insert_with(|| Device {
            id,
            public_key: hex(&key),
            name: String::new(),
            created_at: now,
            last_seen_at: now,
            wrapped_account_key: String::new(),
            seq,
        });
        d.name = name;
        d.last_seen_at = now;
        let resp = Resp::json(if fresh { 201 } else { 200 }, &serialize_device(d));
        if fresh {
            s.sync.seq = seq;
        }
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

pub fn list_approvals(app: &App, req: &Req) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    let now = s.now();
    let mut pending: Vec<&Approval> = s.sync.approvals.values().filter(|a| a.workspace == workspace && !a.approved && a.expires_at > now).collect();
    pending.sort_by_key(|a| std::cmp::Reverse(a.created_at));
    Resp::json(200, &Json::map([("device_approvals", Json::Arr(pending.into_iter().map(serialize_approval).collect()))]))
}

pub fn request_approval(app: &App, req: &mut Req) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let v = body(req, "device_approval")?;
        let mut f = v.fields();
        let key = blob("public_key", &required(&mut f, "public_key")?, Some(PUBLIC_KEY_BYTES), None)?;
        let name = required(&mut f, "name")?;
        let mut s = app.lock();
        let now = s.now();
        let a = Approval {
            slug: generate_slug("dap"),
            workspace,
            id: fingerprint(&key),
            public_key: hex(&key),
            name,
            approved: false,
            created_at: now,
            expires_at: now + APPROVAL_LIFETIME,
            approved_by: String::new(),
            account_key_id: String::new(),
            wrapped_account_key: String::new(),
        };
        let resp = Resp::json(201, &serialize_approval(&a));
        s.sync.approvals.insert(a.slug.clone(), a);
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

fn approval_expired(a: &Approval) -> Resp {
    error(410, "approval_expired", &format!("this device approval expired at {} — ask again from the new device", rfc3339_nano(a.expires_at)), None)
}

pub fn show_approval(app: &App, req: &Req, slug: &str) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    let now = s.now();
    match s.sync.approvals.get(slug).filter(|a| a.workspace == workspace) {
        None => not_found(),
        Some(a) if !a.approved && a.expires_at <= now => approval_expired(a),
        Some(a) => Resp::json(200, &serialize_approval(a)),
    }
}

/// The answer: the account key wrapped to the request's public key, from a
/// device of the workspace. Leaves the new device registered with its wrap.
pub fn approve(app: &App, req: &mut Req, slug: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let v = body(req, "approval")?;
        let mut f = v.fields();
        let device = id_field("device", &required(&mut f, "device")?)?;
        let account = id_field("account_key_id", &required(&mut f, "account_key_id")?)?;
        let wrapped = blob("wrapped_account_key", &required(&mut f, "wrapped_account_key")?, Some(WRAPPED_ACCOUNT_KEY_BYTES), None)?;
        let mut s = app.lock();
        let now = s.now();
        let seq = s.sync.seq + 1;
        let sync = &mut s.sync;
        let a = sync.approvals.get(slug).filter(|a| a.workspace == workspace).ok_or_else(not_found)?;
        if !a.approved && a.expires_at <= now {
            return Err(approval_expired(a));
        }
        if a.approved {
            return Err(error(409, "already_approved", &format!("{slug} is already approved — the new device has an account key to collect"), None));
        }
        if !sync.devices.contains_key(&(workspace.clone(), device.clone())) {
            return Err(not_found());
        }
        adopt_account_key(sync, &workspace, &account)?;
        let a = sync.approvals.get_mut(slug).unwrap();
        a.approved = true;
        a.approved_by = device;
        a.account_key_id = account;
        a.wrapped_account_key = hex(&wrapped);
        let (id, public_key, name, wrapped) = (a.id.clone(), a.public_key.clone(), a.name.clone(), a.wrapped_account_key.clone());
        let resp = Resp::json(200, &serialize_approval(a));
        let fresh = !sync.devices.contains_key(&(workspace.clone(), id.clone()));
        let d = sync.devices.entry((workspace, id.clone())).or_insert_with(|| Device {
            id,
            public_key,
            name: String::new(),
            created_at: now,
            last_seen_at: now,
            wrapped_account_key: String::new(),
            seq,
        });
        d.name = name;
        d.wrapped_account_key = wrapped;
        if fresh {
            sync.seq = seq;
        }
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

/// A session id as the registry routes it: a UUID, else no such session.
fn session_id(id: &str) -> Option<String> {
    let id = id.to_ascii_lowercase();
    let shape = id.len() == 36 && id.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() });
    shape.then_some(id)
}

pub fn list_sessions(app: &App, req: &Req) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    let now = s.now();
    let mut mine: Vec<&Session> = s.sync.sessions.iter().filter(|((w, _), _)| *w == workspace).map(|(_, x)| x).collect();
    mine.sort_by_key(|x| (std::cmp::Reverse(x.updated_at), std::cmp::Reverse(x.seq)));
    Resp::json(200, &Json::map([("sessions", Json::Arr(mine.into_iter().map(|x| serialize_session(x, now)).collect()))]))
}

pub fn show_session(app: &App, req: &Req, id: &str) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    let now = s.now();
    session_id(id).and_then(|id| s.sync.sessions.get(&(workspace, id))).map_or_else(not_found, |x| Resp::json(200, &serialize_session(x, now)))
}

fn lease_stale(s: &Session, presented: i64, now: Timestamp) -> Resp {
    let held = if leased(s, now) { "" } else { ", and nobody holds it" };
    error(
        409,
        "lease_stale",
        &format!("fence {presented} is not this session's live lease (the session is at {}{held}) — another device took the lease or it lapsed; acquire it again", s.fence),
        None,
    )
}

fn fenced(s: &Session, presented: i64, now: Timestamp) -> Result<(), Resp> {
    if leased(s, now) && presented == s.fence as i64 { Ok(()) } else { Err(lease_stale(s, presented, now)) }
}

/// Creates the session under its id, or writes its sealed index — the
/// latter the lease holder's, naming its fence. The wrapped key never
/// changes.
pub fn put_session(app: &App, req: &mut Req, id: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let id = session_id(id).ok_or_else(not_found)?;
        let v = body(req, "session")?;
        let mut f = v.fields();
        let wrapped = hex(&blob("wrapped_key", &required(&mut f, "wrapped_key")?, Some(WRAPPED_SESSION_KEY_BYTES), None)?);
        let sealed = f.string("sealed_index");
        let sealed = if sealed.is_empty() { String::new() } else { hex(&blob("sealed_index", &sealed, None, Some(MAX_SEALED_INDEX_BYTES))?) };
        let fence = number(&f, "fence");
        let mut s = app.lock();
        let now = s.now();
        let seq = s.sync.seq + 1;
        let key = (workspace, id.clone());
        let Some(x) = s.sync.sessions.get_mut(&key) else {
            let x = Session { id, wrapped_key: wrapped, sealed_index: sealed, fence: 0, holder: String::new(), lease_expires_at: None, created_at: now, updated_at: now, last_written_at: now, seq };
            let resp = Resp::json(201, &serialize_session(&x, now));
            s.sync.sessions.insert(key, x);
            s.sync.seq = seq;
            return Ok(resp);
        };
        if x.wrapped_key != wrapped {
            return Err(invalid("wrapped_key", "is set when the session is created and cannot change"));
        }
        if x.sealed_index != sealed {
            let fence = fence.ok_or_else(|| parameter_missing("fence"))?;
            fenced(x, fence, now)?;
            x.sealed_index = sealed;
            x.last_written_at = now;
            x.updated_at = now;
        }
        Ok(Resp::json(200, &serialize_session(x, now)))
    };
    run().unwrap_or_else(|r| r)
}

fn ttl(f: &Fields) -> Result<SignedDuration, Resp> {
    let secs = number(f, "ttl").unwrap_or(DEFAULT_LEASE_TTL);
    if LEASE_TTL.contains(&secs) {
        Ok(SignedDuration::from_secs(secs))
    } else {
        Err(invalid("ttl", &format!("must be a whole number of seconds from {} to {}", LEASE_TTL.start(), LEASE_TTL.end())))
    }
}

/// A lease call, once its session and device are known to be the
/// workspace's: the store still locked, the session's key, the device and
/// the body's `lease`.
type LeaseCall<'a> = (std::sync::MutexGuard<'a, crate::store::Store>, (String, String), String, Value);

/// The session and the device a lease call names, both the workspace's.
fn lease_call<'a>(app: &'a App, req: &mut Req, id: &str, needs_device: bool) -> Result<LeaseCall<'a>, Resp> {
    let workspace = gate(req)?;
    let id = session_id(id).ok_or_else(not_found)?;
    let v = body(req, "lease")?;
    let mut f = v.fields();
    let device = if needs_device { id_field("device", &required(&mut f, "device")?)? } else { String::new() };
    let s = app.lock();
    let key = (workspace.clone(), id);
    if !s.sync.sessions.contains_key(&key) || (needs_device && !s.sync.devices.contains_key(&(workspace, device.clone()))) {
        return Err(not_found());
    }
    Ok((s, key, device, v))
}

pub fn acquire_lease(app: &App, req: &mut Req, id: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let (mut s, key, device, v) = lease_call(app, req, id, true)?;
        let ttl = ttl(&v.fields())?;
        let now = s.now();
        let x = s.sync.sessions.get_mut(&key).unwrap();
        if leased(x, now) {
            let until = rfc3339_nano(x.lease_expires_at.unwrap());
            return Err(error(
                409,
                "lease_held",
                &format!("device {} holds this session's lease until {until} — send it commands instead, or wait for the lease to lapse", x.holder),
                Some(Json::map([("device", Json::str(&x.holder)), ("expires_at", Json::str(until)), ("fence", Json::Int(x.fence as i64))])),
            ));
        }
        x.fence += 1;
        x.holder = device;
        x.lease_expires_at = Some(now + ttl);
        x.updated_at = now;
        Ok(Resp::json(201, &serialize_lease(x)))
    };
    run().unwrap_or_else(|r| r)
}

pub fn renew_lease(app: &App, req: &mut Req, id: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let (mut s, key, device, v) = lease_call(app, req, id, true)?;
        let f = v.fields();
        let fence = number(&f, "fence").ok_or_else(|| parameter_missing("fence"))?;
        let ttl = ttl(&f)?;
        let now = s.now();
        let x = s.sync.sessions.get_mut(&key).unwrap();
        fenced(x, fence, now)?;
        if x.holder != device {
            x.fence += 1;
            x.holder = device;
        }
        x.lease_expires_at = Some(now + ttl);
        x.updated_at = now;
        Ok(Resp::json(200, &serialize_lease(x)))
    };
    run().unwrap_or_else(|r| r)
}

pub fn release_lease(app: &App, req: &mut Req, id: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let (mut s, key, _, v) = lease_call(app, req, id, false)?;
        let fence = number(&v.fields(), "fence").ok_or_else(|| parameter_missing("fence"))?;
        let now = s.now();
        let x = s.sync.sessions.get_mut(&key).unwrap();
        fenced(x, fence, now)?;
        x.holder.clear();
        x.lease_expires_at = None;
        x.updated_at = now;
        Ok(Resp::empty(204))
    };
    run().unwrap_or_else(|r| r)
}
