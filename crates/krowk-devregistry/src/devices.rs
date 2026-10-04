//! Devices a person owns (canon, engineering/devices.md): their signed
//! device list, the user key wrapped to the devices on it, and the keys
//! bound to those devices — the shapes and refusals of the registry's
//! Api::V1::DeviceListsController, DeviceLists::EntriesController,
//! UserKeysController and DeviceList.
//!
//! Every post is verified by krowk-client's own verifier, on top of the
//! whole chain held here replayed from seq 0, so an entry lands only if
//! every client would take it: the stand-in and the clients cannot drift,
//! because they are the same code. What the registry adds on top — the
//! entry posted by one of its signers, exactly the wraps a rotation needs,
//! is
//! checked here as DeviceList#append! checks it. A post lands whole or not
//! at all.
//!
//! A person is named by their key (`store::person_for`). A key speaks for
//! one device once it is bound (`keys.device_id`): at `sync init` to the
//! first device, and otherwise by the device claiming it, signed (`PUT
//! /v1/key/device`), set once. The calls a machine makes before it is a
//! device — reading the chain, init, an append as the recovery kit, the user
//! key, claiming its key, B's pairing steps — take a key bound to none; the
//! rest need the key's device on the list and active (`key_has_no_device`,
//! `device_revoked`). Stricter than the registry on one point: a key bound
//! to one device and signed as another is refused (`device_mismatch`) on
//! every signed call, not only when it claims. Removing a device revokes
//! every key bound to it.

use crate::auth::token;
use crate::encode::Json;
use crate::errors::{error, invalid, parameter_missing};
use crate::http::{Req, Resp};
use crate::json::Value;
use crate::store::{App, hex, person_for, rfc3339_nano, sha256_hex};
use crate::sync::{SyncStore, burst, caller, check_signature, gate, signature_headers, unhex};
use jiff::Timestamp;
use krowk_client::device_chain::{Action, Chain, Entry, Kind, SignedEntry};

/// DeviceList::WRAP_BYTES: the client fixes the formats, the registry
/// bounds them.
const WRAP_BYTES: std::ops::RangeInclusive<usize> = 32..=512;
const MAX_ENTRIES_PER_POST: usize = 32;
const MAX_ENTRIES: usize = 2_000;
const MAX_LISTED_DEVICES: usize = 50;
const MAX_ENTRY_BYTES: usize = 4 << 10;
const PAGE: usize = 500;

/// One entry as it was posted, with when the registry received it.
#[derive(Clone)]
pub struct Stored {
    pub seq: u64,
    pub bytes: Vec<u8>,
    pub signatures: Vec<u8>,
    pub received_at: Timestamp,
}

/// A device the chain named, as the registry projects it for its own
/// refusals.
#[derive(Clone)]
pub struct Listed {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
    pub os: String,
    pub public_key: [u8; 32],
    pub signing_key: [u8; 32],
    pub added_seq: u64,
    pub removed_seq: Option<u64>,
    pub revoked_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

impl Listed {
    fn active(&self) -> bool {
        self.removed_seq.is_none() && self.revoked_at.is_none()
    }
}

#[derive(Clone)]
pub struct Generation {
    pub generation: u32,
    pub key_id: [u8; 16],
    /// The generation before, wrapped under this one; none for the first.
    pub wrapped_previous: Option<Vec<u8>>,
}

/// A person's chain, its epoch, their user key's generations and wraps, and
/// the devices it names.
#[derive(Clone)]
pub struct Person {
    /// Starts at 1, and a start-over moves it on: a new chain, never a
    /// shorter one.
    pub epoch: u64,
    pub entries: Vec<Stored>,
    pub generations: Vec<Generation>,
    /// Generation, device id and the wrap.
    pub wraps: Vec<(u32, String, Vec<u8>)>,
    pub devices: Vec<Listed>,
}

impl Default for Person {
    fn default() -> Person {
        Person { epoch: 1, entries: Vec::new(), generations: Vec::new(), wraps: Vec::new(), devices: Vec::new() }
    }
}

impl Person {
    fn signed(&self) -> Result<Vec<SignedEntry>, Resp> {
        self.entries.iter().map(|e| SignedEntry::from_parts(e.bytes.clone(), &e.signatures).map_err(|e| refused(&e.0))).collect()
    }

    /// The chain held here, verified from seq 0 as a fresh machine would
    /// verify it (DeviceList#verified).
    pub fn verified(&self) -> Result<Option<Chain>, Resp> {
        let entries = self.signed()?;
        if entries.is_empty() {
            return Ok(None);
        }
        Chain::verify(&entries, None).map(Some).map_err(|e| refused(&e.0))
    }

    pub fn device(&self, id: &str) -> Option<&Listed> {
        self.devices.iter().find(|d| d.id == id)
    }
}

fn refused(message: &str) -> Resp {
    error(422, "device_list_invalid", message, None)
}

/// Who a sync call is: the person and the key's digest.
pub struct Caller {
    pub person: String,
    pub key: String,
}

/// The key and the paid gate, then the person behind the key. A key
/// revoked with its device is refused like any unknown key.
pub fn caller_of(s: &SyncStore, req: &Req) -> Result<Caller, Resp> {
    gate(req)?;
    let token = token(req).unwrap_or_default();
    let key = sha256_hex(token.as_bytes());
    if s.revoked_keys.contains(&key) {
        return Err(crate::errors::unauthorized());
    }
    Ok(Caller { person: person_for(&token), key })
}

fn device_revoked(id: &str) -> Resp {
    error(403, "device_revoked", &format!("device {id} was revoked or removed — sign in again on a device on your list"), None)
}

/// The device a signed call is signed by: on the person's list, not removed
/// or revoked, its signature good, and the key's own device — binding a key
/// that speaks for none yet to it.
pub fn signed_device(s: &mut SyncStore, req: &Req, caller: &Caller, body: &[u8]) -> Result<String, Resp> {
    let (device, at, signature) = signature_headers(req)?;
    let person = s.people.get(&caller.person).filter(|p| !p.entries.is_empty()).ok_or_else(no_device_list)?;
    let listed = person.device(&device).cloned();
    let key = listed.as_ref().map(|d| d.signing_key.to_vec()).unwrap_or_default();
    check_signature(s, req, &device, &at, &signature, &key, body)?;
    let listed = listed.ok_or_else(|| device_revoked(&device))?;
    if !listed.active() {
        return Err(device_revoked(&device));
    }
    // The recovery kit posts from whichever machine holds its words, on
    // that machine's key, as the registry lets it.
    if let Some(bound) = s.bindings.get(&caller.key)
        && *bound != device
        && listed.kind != "recovery"
    {
        return Err(error(403, "device_mismatch", &format!("this key speaks for device {bound}, not {device}"), None));
    }
    Ok(device)
}

/// The device the key speaks for, on the list and active: what every sync
/// call but the few a machine makes before it is a device requires
/// (Api::Sync#refuse_key_without_an_active_device).
pub fn key_device(s: &SyncStore, caller: &Caller) -> Result<String, Resp> {
    let Some(bound) = s.bindings.get(&caller.key) else {
        return Err(error(403, "key_has_no_device", "this key speaks for no device yet — set sync up with `krowk sync init`, or join with `krowk sync join`", None));
    };
    match s.people.get(&caller.person).and_then(|p| p.device(bound)) {
        Some(d) if d.active() => Ok(bound.clone()),
        _ => Err(device_revoked(bound)),
    }
}

/// PUT /v1/key/device: the key claims the device it speaks for, signed by
/// that device — the new machine after `join` or `recover`. Set once: a key
/// already speaking for another device is refused, so a key copied off one
/// machine cannot be moved to another.
pub fn claim(app: &App, req: &mut Req) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = read_body(req)?;
        let mut s = app.lock();
        let who = caller_of(&s.sync, req)?;
        let device = signed_device(&mut s.sync, req, &who, &body)?;
        s.sync.bindings.insert(who.key.clone(), device.clone());
        let key_id = format!("key_{}", &who.key[..8]);
        Ok(Resp::json(200, &Json::map([("key_id", Json::str(key_id)), ("device", Json::str(device))])))
    };
    run().unwrap_or_else(|r| r)
}

fn no_device_list() -> Resp {
    error(409, "no_device_list", "you have no device list yet — set sync up with `krowk sync init`", None)
}

fn read_body(req: &mut Req) -> Result<Vec<u8>, Resp> {
    req.read_body(1 << 20).map_err(|_| error(400, "bad_request", "the body could not be read", None))
}

/// GET /v1/users/:user_id/devices: the chain from `after` on, exactly as
/// posted, with when each entry was received, the epoch and the head — and
/// every device it has named, with its revocation state. Not signed: a
/// machine recovering has no device to sign as yet.
pub fn show(app: &App, req: &Req) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let s = app.lock();
        let caller = caller_of(&s.sync, req)?;
        let after = match req.query_get("after").as_str() {
            "" => -1,
            a => a.parse::<i64>().map_err(|_| invalid("after", "must be a seq"))?,
        };
        let person = s.sync.people.get(&caller.person).cloned().unwrap_or_default();
        let page: Vec<&Stored> = person.entries.iter().filter(|e| e.seq as i64 > after).take(PAGE).collect();
        let head = person.entries.last().map_or(Json::Null, |e| {
            Json::map([("seq", Json::Int(e.seq as i64)), ("hash", Json::str(sha256_hex(&e.bytes)))])
        });
        let next = if page.len() == PAGE { page.last().map_or(Json::Null, |e| Json::Int(e.seq as i64)) } else { Json::Null };
        let entries = page
            .iter()
            .map(|e| {
                Json::map([
                    ("seq", Json::Int(e.seq as i64)),
                    ("entry", Json::str(hex(&e.bytes))),
                    ("signatures", Json::str(hex(&e.signatures))),
                    // Unix seconds, as the chain's own times are: what a fresh
                    // machine counts a recovery rotation's delay from.
                    ("received_at", Json::Int(e.received_at.as_second())),
                ])
            })
            .collect();
        let devices = person.devices.iter().map(serialize).collect();
        Ok(Resp::json(200, &Json::map([("epoch", Json::Int(person.epoch as i64)), ("entries", Json::Arr(entries)), ("head", head), ("next", next), ("devices", Json::Arr(devices))])))
    };
    run().unwrap_or_else(|r| r)
}

/// What a device list post carries (Api::DeviceListPosts).
struct Post {
    entries: Vec<SignedEntry>,
    links: Vec<Vec<u8>>,
    wraps: Vec<(String, Vec<u8>)>,
    start_over: bool,
}

fn blob(field: &str, v: Option<&Value>, max: usize) -> Result<Vec<u8>, Resp> {
    let Some(Value::Str(s)) = v else { return Err(parameter_missing(field)) };
    let b = unhex(s).ok_or_else(|| invalid(field, "must be hex"))?;
    if b.len() > max {
        return Err(invalid(field, &format!("must be at most {max} bytes")));
    }
    Ok(b)
}

fn wrap(field: &str, v: Option<&Value>) -> Result<Vec<u8>, Resp> {
    let b = blob(field, v, *WRAP_BYTES.end())?;
    if !WRAP_BYTES.contains(&b.len()) {
        return Err(invalid(field, &format!("must be {} to {} bytes", WRAP_BYTES.start(), WRAP_BYTES.end())));
    }
    Ok(b)
}

fn list<'a>(field: &str, v: Option<&'a Value>) -> Result<&'a [Value], Resp> {
    match v {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Arr(items)) => Ok(items),
        Some(_) => Err(invalid(field, "must be a list")),
    }
}

fn post(body: &[u8]) -> Result<Post, Resp> {
    let v = crate::json::parse(body).ok_or_else(crate::errors::bad_request)?;
    let list_value = v.get("device_list").map(|m| m.value.clone()).filter(|v| matches!(v, Value::Obj(_))).ok_or_else(|| parameter_missing("device_list"))?;
    let field = |name: &str| list_value.get(name).map(|m| &m.value);
    let entries = list("entries", field("entries"))?
        .iter()
        .map(|e| {
            let bytes = blob("entry", e.get("entry").map(|m| &m.value), MAX_ENTRY_BYTES)?;
            let signatures = blob("signatures", e.get("signatures").map(|m| &m.value), 1 + 3 * 80)?;
            SignedEntry::from_parts(bytes, &signatures).map_err(|e| invalid("signatures", &e.0))
        })
        .collect::<Result<Vec<_>, Resp>>()?;
    let links = list("links", field("links"))?.iter().map(|l| wrap("links", Some(l))).collect::<Result<Vec<_>, Resp>>()?;
    let mut wraps = Vec::new();
    for w in list("wraps", field("wraps"))? {
        let device = match w.get("device").map(|m| &m.value) {
            Some(Value::Str(d)) if d.len() == 32 && d.bytes().all(|b| b.is_ascii_hexdigit()) => d.to_ascii_lowercase(),
            _ => return Err(invalid("device", "must be 32 hex characters")),
        };
        if wraps.iter().any(|(d, _)| *d == device) {
            return Err(invalid("wraps", "name each device once"));
        }
        wraps.push((device, wrap("wrapped_key", w.get("wrapped_key").map(|m| &m.value))?));
    }
    Ok(Post { entries, links, wraps, start_over: false })
}

/// POST /v1/users/:user_id/devices for a person with no list yet: `krowk
/// sync init`; and POST …/devices/reset (`start_over`), which replaces the
/// person's list with a new epoch. Signed by the first device the first
/// entry adds, whose key the registry has only from the entry; always a
/// fresh sign-in; `409 chain_exists` for a person with a chain, unless it
/// is a start-over.
pub fn create(app: &App, req: &mut Req, start_over: bool) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = read_body(req)?;
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        burst(&mut s.sync, &caller(req), "device_list_inits", now)?;
        let p = Post { start_over, ..post(&body)? };
        let first = p.entries.first().ok_or_else(|| refused("sync init posts the list's first entry, adding the device setting sync up"))?;
        let e = Entry::decode(&first.bytes).map_err(|e| refused(&e.0))?;
        let device = e.subjects.iter().find(|s| s.kind == Kind::Device).filter(|_| e.seq == 0).ok_or_else(|| refused("sync init posts the list's first entry, adding the device setting sync up"))?;
        let (signer, at, signature) = signature_headers(req)?;
        let fingerprint = device.id().to_string();
        check_signature(&mut s.sync, req, &signer, &at, &signature, &device.signing.0, &body)?;
        if signer != fingerprint {
            return Err(error(403, "device_mismatch", &format!("the list's first entry adds device {fingerprint} but the request is signed by {signer}"), None));
        }
        let written = append(&mut s.sync, &who, p, None, true, now)?;
        let epoch = s.sync.people[&who.person].epoch;
        let seqs = written.into_iter().map(|q| Json::map([("seq", Json::Int(q as i64))])).collect();
        Ok(Resp::json(201, &Json::map([("epoch", Json::Int(epoch as i64)), ("entries", Json::Arr(seqs))])))
    };
    run().unwrap_or_else(|r| r)
}

/// POST /v1/users/:user_id/devices for a person with a list: an append,
/// signed by the device that signed every entry of it.
/// POST /v1/users/:user_id/devices: an init for a person with no list yet,
/// an append for one with a list.
/// Which, by the post's first entry: seq 0 starts a list (`409
/// chain_exists` if there is one), any other extends it.
pub fn post_devices(app: &App, req: &mut Req) -> Resp {
    let body = match read_body(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let starts = post(&body).ok().and_then(|p| p.entries.first().and_then(|e| Entry::decode(&e.bytes).ok())).is_some_and(|e| e.seq == 0);
    let mut cursor = std::io::Cursor::new(body);
    let mut inner = Req { method: req.method.clone(), path: req.path.clone(), query: req.query.clone(), headers: req.headers.clone(), host: req.host.clone(), remote: req.remote.clone(), body: &mut cursor };
    if starts { create(app, &mut inner, false) } else { append_entries(app, &mut inner) }
}

fn append_entries(app: &App, req: &mut Req) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = read_body(req)?;
        let mut s = app.lock();
        let now = s.now();
        let who = caller_of(&s.sync, req)?;
        burst(&mut s.sync, &caller(req), "device_list_entries", now)?;
        let signer = signed_device(&mut s.sync, req, &who, &body)?;
        let p = post(&body)?;
        let written = append(&mut s.sync, &who, p, Some(signer), false, now)?;
        let seqs = written.into_iter().map(|q| Json::map([("seq", Json::Int(q as i64))])).collect();
        Ok(Resp::json(201, &Json::map([("entries", Json::Arr(seqs))])))
    };
    run().unwrap_or_else(|r| r)
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Device => "device",
        Kind::Recovery => "recovery",
    }
}

/// DeviceList#append!, on a copy of the person that replaces theirs only if
/// every check passes. Returns the seqs written.
fn append(s: &mut SyncStore, caller: &Caller, p: Post, signer: Option<String>, init: bool, now: Timestamp) -> Result<Vec<u64>, Resp> {
    if !(1..=MAX_ENTRIES_PER_POST).contains(&p.entries.len()) {
        return Err(refused(&format!("a post carries 1 to {MAX_ENTRIES_PER_POST} entries")));
    }
    let decoded = p.entries.iter().map(|e| Entry::decode(&e.bytes).map_err(|e| refused(&e.0))).collect::<Result<Vec<_>, _>>()?;
    let mut person = s.people.get(&caller.person).cloned().unwrap_or_default();
    let mut revoke: Vec<String> = Vec::new();
    if init && !person.entries.is_empty() {
        if !p.start_over {
            return Err(error(409, "chain_exists", "you already have a device list — add this machine from one of your devices with `krowk sync join`, or start over with `krowk sync init --start-over` if every device and the kit are lost", None));
        }
        // A new chain, never a shorter one: the old devices are refused and
        // their keys revoked, but for the key doing it.
        revoke.extend(person.devices.iter().map(|d| d.id.clone()));
        person = Person { epoch: person.epoch + 1, ..Person::default() };
    }
    if !init && person.entries.is_empty() {
        return Err(no_device_list());
    }
    if person.entries.len() + p.entries.len() > MAX_ENTRIES {
        return Err(error(422, "device_list_full", &format!("your device list has taken as many entries as it may ({MAX_ENTRIES} in all)"), None));
    }
    let mut chain = if init { None } else { person.verified()? };
    let base = chain.as_ref().map_or(0, Chain::generation);
    let (mut rotations, mut added) = (Vec::new(), Vec::new());
    let mut written = Vec::new();
    let mut signer = signer;
    for (entry, e) in p.entries.iter().zip(&decoded) {
        let (seq, prev) = chain.as_ref().map_or((0, [0; 32]), |c| (c.head().seq + 1, c.head().hash));
        if e.seq != seq || e.prev != prev {
            let head = chain.as_ref().map_or(Json::Null, |c| Json::Int(c.head().seq as i64));
            return Err(error(409, "device_list_stale", "this entry does not extend your device list — fetch the list, verify it and try again", Some(Json::map([("seq", head)]))));
        }
        let generation_before = chain.as_ref().map_or(0, Chain::generation);
        let next = match &chain {
            None => Chain::verify(std::slice::from_ref(entry), None),
            Some(c) => c.extend(entry),
        }
        .map_err(|e| refused(&e.0))?;
        let poster = signer.clone().or_else(|| e.subjects.iter().find(|s| s.kind == Kind::Device).map(|s| s.id().to_string()));
        if !e.signers.iter().any(|id| Some(id.to_string()) == poster) {
            return Err(refused(&format!("entry {} is not signed by the device that posted it", e.seq)));
        }
        let rotated = next.generation() > generation_before;
        if rotated {
            rotations.push((e.generation, e.key_id.0));
        }
        match e.action {
            Action::Add => {
                for subject in &e.subjects {
                    let id = subject.id().to_string();
                    person.devices.push(Listed {
                        id: id.clone(),
                        kind: kind_name(subject.kind),
                        name: subject.name.clone(),
                        os: subject.os.clone(),
                        public_key: subject.device.0,
                        signing_key: subject.signing.0,
                        added_seq: e.seq,
                        removed_seq: None,
                        revoked_at: None,
                        created_at: now,
                    });
                    added.push(id);
                }
                signer = signer.or_else(|| added.first().cloned());
            }
            Action::Remove => {
                let id = e.subjects[0].id().to_string();
                remove(&mut person, &id, e.seq, now, &mut revoke);
            }
            Action::RotateRecovery => {
                let old: Vec<String> = person.devices.iter().filter(|d| d.kind == "recovery" && d.removed_seq.is_none()).map(|d| d.id.clone()).collect();
                for id in old {
                    remove(&mut person, &id, e.seq, now, &mut revoke);
                }
                let subject = &e.subjects[0];
                let id = subject.id().to_string();
                person.devices.push(Listed { id: id.clone(), kind: "recovery", name: subject.name.clone(), os: subject.os.clone(), public_key: subject.device.0, signing_key: subject.signing.0, added_seq: e.seq, removed_seq: None, revoked_at: None, created_at: now });
                added.push(id);
            }
        }
        person.entries.push(Stored { seq: e.seq, bytes: entry.bytes.clone(), signatures: entry.signatures_bytes(), received_at: now });
        written.push(e.seq);
        chain = Some(next);
    }
    let chain = chain.expect("at least one entry");
    let rotated = chain.generation() > base;
    let listed: Vec<String> = chain.devices().iter().map(|d| d.id().to_string()).collect();
    if rotated {
        let revoked: Vec<&str> = person.devices.iter().filter(|d| d.removed_seq.is_none() && d.revoked_at.is_some()).map(|d| d.name.as_str()).collect();
        if !revoked.is_empty() {
            return Err(refused(&format!("a new key generation would be wrapped to {}, revoked from the dashboard — remove it in the same post, or un-revoke it", revoked.join(", "))));
        }
    }
    if person.devices.iter().filter(|d| d.removed_seq.is_none()).count() > MAX_LISTED_DEVICES {
        return Err(error(422, "device_list_full", &format!("a device list holds at most {MAX_LISTED_DEVICES} devices — remove one first"), None));
    }
    let expected = rotations.iter().filter(|(g, _)| *g > 1).count();
    if p.links.len() != expected {
        return Err(refused(&format!("the post makes {} generation(s) and needs {expected} link(s) to the one before, not {}", rotations.len(), p.links.len())));
    }
    let mut links = p.links.into_iter();
    for (generation, key_id) in rotations {
        let wrapped_previous = if generation > 1 { links.next() } else { None };
        person.generations.push(Generation { generation, key_id, wrapped_previous });
    }
    // Exactly the devices named: every listed device when anything rotated,
    // the added ones otherwise. A proposed kit is not a device yet.
    let mut wanted: Vec<String> = if rotated { listed } else { added.into_iter().filter(|id| listed.contains(id)).collect() };
    let mut named: Vec<String> = p.wraps.iter().map(|(d, _)| d.clone()).collect();
    wanted.sort();
    named.sort();
    if wanted != named {
        return Err(refused(&format!("the user key at generation {} is wrapped to exactly {}", chain.generation(), if wanted.is_empty() { "no device".into() } else { wanted.join(", ") })));
    }
    for (device, wrapped) in p.wraps {
        person.wraps.push((chain.generation(), device, wrapped));
    }
    // Commit: the person, every key bound to a device this post removed, and
    // at init the key doing it bound to the first device.
    for (key, device) in &s.bindings {
        if revoke.contains(device) && *key != caller.key {
            s.revoked_keys.insert(key.clone());
        }
    }
    if init && person.epoch > 1 && s.people.get(&caller.person).is_some_and(|old| old.epoch < person.epoch) {
        for p in s.pairings.values_mut().filter(|p| p.person == caller.person) {
            p.state = crate::pairings::State::Dead;
        }
    }
    if init {
        let first = signer.expect("seq 0 adds a device");
        s.bindings.insert(caller.key.clone(), first);
    }
    s.people.insert(caller.person.clone(), person);
    Ok(written)
}

/// Off the list for good, and every key bound to it revoked with it.
fn remove(person: &mut Person, id: &str, seq: u64, now: Timestamp, revoke: &mut Vec<String>) {
    if let Some(d) = person.devices.iter_mut().find(|d| d.id == id) {
        d.removed_seq = Some(seq);
        d.revoked_at = d.revoked_at.or(Some(now));
    }
    revoke.push(id.to_owned());
}

/// GET /v1/users/:user_id/devices/:id/key: every generation's id and its
/// wrap of the one before, and the generations wrapped to device `:id` —
/// which must be the signing device: its own and no other's.
pub fn user_key(app: &App, req: &mut Req, id: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let body = read_body(req)?;
        let mut s = app.lock();
        let caller = caller_of(&s.sync, req)?;
        let device = signed_device(&mut s.sync, req, &caller, &body)?;
        if !device.eq_ignore_ascii_case(id) {
            return Err(error(403, "device_mismatch", &format!("the request names device {id} but is signed by device {device}"), None));
        }
        let person = &s.sync.people[&caller.person];
        let generations = person
            .generations
            .iter()
            .map(|g| {
                Json::map([
                    ("generation", Json::Int(i64::from(g.generation))),
                    ("key_id", Json::str(hex(&g.key_id))),
                    ("wrapped_previous", g.wrapped_previous.as_ref().map_or(Json::Null, |w| Json::str(hex(w)))),
                ])
            })
            .collect();
        let wraps = person
            .wraps
            .iter()
            .filter(|(_, d, _)| *d == device)
            .map(|(g, _, w)| Json::map([("generation", Json::Int(i64::from(*g))), ("wrapped_key", Json::str(hex(w)))]))
            .collect();
        Ok(Resp::json(200, &Json::map([("generations", Json::Arr(generations)), ("wraps", Json::Arr(wraps))])))
    };
    run().unwrap_or_else(|r| r)
}

/// The person's devices as the chain named them, removed ones too, in the
/// registry's DeviceSerializer shape.
pub fn serialize(d: &Listed) -> Json {
    Json::map([
        ("id", Json::str(&d.id)),
        ("kind", Json::str(d.kind)),
        ("public_key", Json::str(hex(&d.public_key))),
        ("signing_key", Json::str(hex(&d.signing_key))),
        ("name", Json::str(&d.name)),
        ("os", if d.os.is_empty() { Json::Null } else { Json::str(&d.os) }),
        ("added_seq", Json::Int(d.added_seq as i64)),
        ("removed_seq", d.removed_seq.map_or(Json::Null, |q| Json::Int(q as i64))),
        ("created_at", Json::str(rfc3339_nano(d.created_at))),
        ("last_seen_at", Json::Null),
        ("revoked_at", d.revoked_at.map_or(Json::Null, |t| Json::str(rfc3339_nano(t)))),
    ])
}

