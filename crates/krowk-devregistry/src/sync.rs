//! Sync: sessions with their leases, chunks and vintages — the shapes and
//! refusals of the registry's Api::V1::SessionsController and
//! Sessions::LeasesController. A person's device list, user key and pairings
//! are `devices.rs` and `pairings.rs`, and every device a call names or is
//! signed by is one that list names. What the account-key design called —
//! registering a device with the workspace (`POST /v1/devices`), and the
//! approval mailbox — is gone, `410 sync_reset`.
//!
//! Every endpoint needs a key and refuses a free workspace (R-SYNC-1); a
//! token containing `free` is one, as for uploads. Every one also needs the
//! device the key speaks for on the person's list and active, so a device
//! removed from the list, or revoked by the dashboard's Revoke (`POST
//! /_settings/devices/:id/revocation`, its stand-in), is refused its reads
//! as well as its writes. The creates meet a burst ceiling of 120 a minute
//! per key, and a workspace holds at most so many sessions
//! (`Config::max_sessions`). A session's log is chunks, kept
//! apart from the artifacts so no artifact listing, card or lookup can see
//! one, and stored under `/_storage` like any artifact's bytes. Keys and sealed blobs are
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

const WRAPPED_SESSION_KEY_BYTES: usize = 74;
/// A rotated session's wrapped key ring is 32 bytes longer per key epoch
/// after the first, up to this many epochs.
const MAX_SESSION_KEY_EPOCHS: usize = 64;
const MAX_SEALED_INDEX_BYTES: usize = 64 << 10;
const DEFAULT_LEASE_TTL: i64 = 60;
const LEASE_TTL: std::ops::RangeInclusive<i64> = 10..=600;

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub wrapped_key: String,
    /// The publisher's signature over the record and its device id, hex,
    /// kept as they came and returned; never checked here (D8b).
    pub record_signature: String,
    pub signer: String,
    pub sealed_index: String,
    pub fence: u64,
    /// The holder's token, as its digest: the registry keeps no copy either.
    pub token_digest: String,
    pub holder: String,
    pub lease_expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub last_written_at: Timestamp,
    pub seq: usize,
    /// The person whose user key seals it (`sessions.owner_user_id`). In the
    /// stand-in a workspace has one person (`store::person_for`), so owning
    /// is never a second filter on top of the workspace's.
    pub owner: String,
    /// The owner's chain epoch when it was created or last re-sealed: what
    /// says a session was sealed under a list since started over.
    pub sealed_epoch: u64,
}

/// The owner's current chain epoch; 0 for a person with no chain yet.
fn epoch_of(people: &HashMap<String, crate::devices::Person>, person: &str) -> u64 {
    people.get(person).map_or(0, |p| p.epoch)
}

/// The re-seal after a start-over (the registry's
/// `SessionsController#resealable?`): the holder may replace a session's
/// wrapped key and record together when the session was sealed under an
/// older epoch of the owner's chain, it has a record signer, the new key
/// and record both differ from the stored ones, and the new record names
/// the writer. The lease is checked where the write happens. Anything short
/// of that is the "cannot change" it always was.
fn resealable(people: &HashMap<String, crate::devices::Person>, x: &Session, wrapped: &str, record: (&str, &str), signer: &str) -> bool {
    x.sealed_epoch < epoch_of(people, &x.owner) && !x.signer.is_empty() && x.wrapped_key != wrapped && !record.0.is_empty() && record.0 != x.record_signature && record.1 == signer
}

/// A wrapped session key: 74 bytes, or a rotated ring 32 bytes longer per
/// key epoch after the first.
fn wrapped_key(value: &str) -> Result<Vec<u8>, Resp> {
    let raw = blob("wrapped_key", value, None, None)?;
    let epochs = raw.len().checked_sub(WRAPPED_SESSION_KEY_BYTES - 32).filter(|n| n % 32 == 0).map_or(0, |n| n / 32);
    if !(1..=MAX_SESSION_KEY_EPOCHS).contains(&epochs) {
        return Err(invalid("wrapped_key", &format!("must be {WRAPPED_SESSION_KEY_BYTES} bytes, or 32 more for each of up to {} key rotations", MAX_SESSION_KEY_EPOCHS - 1)));
    }
    Ok(raw)
}

/// The user key generation a stored wrapped key names (bytes 2..6).
fn generation(wrapped_hex: &str) -> u32 {
    unhex(wrapped_hex).and_then(|b| b.get(2..6).map(|g| u32::from_be_bytes(g.try_into().expect("four bytes")))).unwrap_or(0)
}

/// A key rotation (the registry's `SessionsController#rotatable?`): the
/// holder may replace a session's wrapped key and record together when the
/// new key names a later user key generation and holds exactly one key
/// more, and the new record differs and names the writer. The lease is
/// checked where the write happens.
fn rotatable(x: &Session, wrapped: &str, record: (&str, &str), signer: &str) -> bool {
    !x.signer.is_empty() && generation(wrapped) > generation(&x.wrapped_key) && wrapped.len() == x.wrapped_key.len() + 64 && !record.0.is_empty() && record.0 != x.record_signature && record.1 == signer
}

/// One chunk of a session's log: an upload like an artifact's, under a key
/// of its own, never an artifact.
pub struct Chunk {
    pub slug: String,
    pub index: u64,
    /// The lease fence it was declared under: whose chunk it is.
    pub fence: u64,
    pub byte_size: i64,
    pub checksum: String,
    pub storage_key: String,
    pub upload_tok: String,
    pub upload_til: Timestamp,
    pub uploaded_sum: Option<String>,
    pub ready: bool,
    pub created_at: Timestamp,
}

/// The registry's burst ceiling on a keyed create, per minute.
const KEYED_BURST: usize = 120;
/// The registry's SyncSession::MAX_PER_WORKSPACE.
pub const MAX_SESSIONS: usize = 10_000;
const CHUNK_CONTENT_TYPE: &str = "application/octet-stream";
/// The registry's Artifact::MAX_CHUNK_BYTES, and krowk-api's read limit.
const MAX_CHUNK_BYTES: i64 = 64 << 20;
/// The registry's Artifact::MAX_VINTAGE_BYTES, and krowk-api's read limit.
const MAX_VINTAGE_BYTES: i64 = 256 << 20;
/// The registry's SyncSession::MAX_CHUNKS.
const MAX_CHUNKS: usize = 100_000;

#[derive(Default)]
pub struct SyncStore {
    /// Workspace, session id and index → the chunk.
    pub chunks: HashMap<(String, String, u64), Chunk>,
    /// Workspace and slug → a vintage: its ISO week, and its bytes' upload,
    /// which is a chunk's with no place in a log (index and fence 0).
    pub vintages: HashMap<(String, String), (String, Chunk)>,
    /// Workspace and slug → the slug of the vintage a pending one replaces
    /// ("" for none), which its finalize checks again.
    pub vintage_replaces: HashMap<(String, String), String>,
    /// Vintages a later one replaced: kept, bytes and all, as the registry
    /// keeps them for its retention window, and no longer the week's.
    pub vintages_replaced: std::collections::HashSet<(String, String)>,
    /// Key and create → the minute its count began, and the count.
    pub bursts: HashMap<(String, &'static str), (Timestamp, usize)>,
    /// 0 is MAX_SESSIONS.
    pub max_sessions: usize,
    /// A person's device list, user key and the devices it names, by
    /// person (`devices.rs`): a person's, not a workspace's.
    pub people: HashMap<String, crate::devices::Person>,
    /// Each key, by its token's digest, and the device it speaks for
    /// (`keys.device_id`).
    pub bindings: HashMap<String, String>,
    /// Keys revoked with the device they were bound to, by digest.
    pub revoked_keys: std::collections::HashSet<String>,
    pub pairings: HashMap<String, crate::pairings::Pairing>,
    pub sessions: HashMap<(String, String), Session>,
    pub seq: usize,
    /// The signed requests accepted in the last few minutes, by digest,
    /// until their window closes: a replay is refused (SignedRequest).
    pub signed: HashMap<[u8; 32], Timestamp>,
}

/// How far a signed request's timestamp may be from this clock, in
/// milliseconds: the registry's SignedRequest::SKEW.
const SIGNATURE_SKEW_MS: i64 = 5 * 60 * 1000;

/// A call that acts as a device (canon, engineering/crypto.md → Signed
/// registry requests), checked as the registry checks it, and then `act`
/// with the device that signed it: after the key and the paid gate, the
/// key's own device on the person's list and active
/// (Api::Sync#refuse_key_without_an_active_device), then the three headers,
/// the timestamp within five minutes, the signature by that list's key for
/// the device named, and not seen before.
pub fn signed(app: &App, req: &mut Req, act: impl FnOnce(&App, &mut Req, &str) -> Resp) -> Resp {
    let bytes = match active(app, req).and_then(|_| req.read_body(4 << 20).map_err(|_| error(400, "bad_request", "the body could not be read", None))) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let signer = {
        let mut s = app.lock();
        match crate::devices::caller_of(&s.sync, req).and_then(|who| crate::devices::signed_device(&mut s.sync, req, &who, &bytes)) {
            Ok(d) => d,
            Err(r) => return r,
        }
    };
    let mut body = std::io::Cursor::new(bytes);
    let mut inner = Req {
        method: req.method.clone(),
        path: req.path.clone(),
        query: req.query.clone(),
        headers: req.headers.clone(),
        host: req.host.clone(),
        remote: req.remote.clone(),
        body: &mut body,
    };
    act(app, &mut inner, &signer)
}

/// What every sync call here checks before anything else, signed or not:
/// the key, the paid gate, and the device the key speaks for on the
/// person's list and active. The calls a machine makes before it is a
/// device are `devices.rs` and `pairings.rs`, which skip the last.
pub fn active(app: &App, req: &Req) -> Result<String, Resp> {
    let s = app.lock();
    let who = crate::devices::caller_of(&s.sync, req)?;
    crate::devices::key_device(&s.sync, &who)
}

/// The three signature headers, or the refusal for a request without them.
/// A client from before devices belonged to a person calls an endpoint that
/// is gone: 410, with the fix in the message (Api::V1::SyncResetsController).
pub fn sync_reset() -> Resp {
    error(410, "sync_reset", "sync was reset when devices moved to your account — update krowk, sign in with `krowk auth login` and run `krowk sync init`", None)
}

pub fn signature_headers(req: &Req) -> Result<(String, String, String), Resp> {
    match (req.header("X-Krowk-Device"), req.header("X-Krowk-Timestamp"), req.header("X-Krowk-Signature")) {
        (Some(d), Some(t), Some(s)) => Ok((d.to_ascii_lowercase(), t.to_owned(), s.to_owned())),
        _ => Err(error(401, "signature_required", "this call acts as a device and must be signed by its signing key (X-Krowk-Device, X-Krowk-Timestamp, X-Krowk-Signature)", None)),
    }
}

/// A signed request's timestamp within five minutes, its signature by `key`
/// over the request's lines, and not seen before (SignedRequest): what every
/// signed call checks, whichever list the key came from.
pub fn check_signature(s: &mut SyncStore, req: &Req, device: &str, at: &str, signature: &str, key: &[u8], body: &[u8]) -> Result<(), Resp> {
    let refused = |code: &str, message: &str| error(401, code, message, None);
    let millis: i64 = at.parse().map_err(|_| refused("signature_invalid", "X-Krowk-Timestamp is not Unix milliseconds"))?;
    if (Timestamp::now().as_millisecond() - millis).abs() > SIGNATURE_SKEW_MS {
        return Err(refused("signature_stale", "the request was signed more than 5 minutes from the registry's clock — check this machine's clock"));
    }
    let signature = unhex(signature).ok_or_else(|| refused("signature_invalid", "X-Krowk-Signature is not hex"))?;
    let target = if req.query.is_empty() { req.path.clone() } else { format!("{}?{}", req.path, req.query) };
    let message = format!("krowk/registry/v1\n{}\n{target}\n{at}\n{}", req.method, hex(&Sha256::digest(body)));
    if !krowk_client::e2e::verify_registry_request(key, &signature, message.as_bytes()) {
        return Err(refused("signature_invalid", &format!("the signature is not device {device}'s over this request")));
    }
    let now = Timestamp::now();
    s.signed.retain(|_, until| *until > now);
    let digest: [u8; 32] = Sha256::digest(format!("{device}\n{message}")).into();
    if s.signed.insert(digest, now + SignedDuration::from_millis(2 * SIGNATURE_SKEW_MS)).is_some() {
        return Err(refused("signature_replayed", "this signed request was already accepted once — sign it again to send it again"));
    }
    Ok(())
}

/// A call naming `device` and signed by another is refused: naming a
/// device is not a way to act as it.
fn signed_as(device: &str, signer: &str) -> Result<(), Resp> {
    if device == signer {
        Ok(())
    } else {
        Err(error(403, "device_mismatch", &format!("the request names device {device} but is signed by device {signer}"), None))
    }
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
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
pub fn gate(req: &Req) -> Result<String, Resp> {
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

/// Who a keyed request's burst is counted against: the key, as the registry
/// counts it (`by: Current.key.id`), not the workspace.
pub fn caller(req: &Req) -> String {
    crate::store::sha256_hex(req.header("Authorization").unwrap_or_default().as_bytes())
}

/// The keyed burst ceiling on a create: 429 with Retry-After past it.
pub fn burst(s: &mut SyncStore, key: &str, name: &'static str, now: Timestamp) -> Result<(), Resp> {
    let window = s.bursts.entry((key.to_owned(), name)).or_insert((now, 0));
    if now.duration_since(window.0) >= SignedDuration::from_mins(1) {
        *window = (now, 0);
    }
    window.1 += 1;
    if window.1 > KEYED_BURST {
        let mut r = error(429, "too_many_requests", &format!("Too many {} too quickly. Retry in 60 seconds.", name.replace('_', " ")), None);
        r.headers.push(("Retry-After", "60".to_owned()));
        return Err(r);
    }
    Ok(())
}

/// The person a call's key speaks for, whose devices it names.
fn person(req: &Req) -> String {
    crate::store::person_for(&crate::auth::token(req).unwrap_or_default())
}

/// A device a call names, as the registry finds it
/// (`sync_user.devices.named!`): one the person's list has named, removed
/// or not, else no such device.
fn named<'a>(s: &'a SyncStore, person: &str, device: &str) -> Result<&'a crate::devices::Listed, Resp> {
    s.people.get(person).and_then(|p| p.device(device)).ok_or_else(not_found)
}

/// The body's `resource` object, and a 400 for a missing one.
pub fn body(req: &mut Req, resource: &str) -> Result<Value, Resp> {
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

fn leased(s: &Session, now: Timestamp) -> bool {
    !s.holder.is_empty() && s.lease_expires_at.is_some_and(|e| e > now)
}

/// The stand-in's relay ticket key: a fixed test seed, published here on
/// purpose, like the conformance suite's. A relay under test trusts its
/// public half (`TICKET_KID`, `ticket_public_key`); nothing real does.
pub const TICKET_SEED: [u8; 32] = [0x5e; 32];
pub const TICKET_KID: [u8; 8] = *b"standin1";
const TICKET_TTL: i64 = 300;

pub fn ticket_public_key() -> [u8; 32] {
    krowk_client::e2e::SigningKey::from_secret(&TICKET_SEED).expect("a 32-byte seed").public().0
}

/// A relay ticket, in the registry's layout (relay.md → Tickets), with its
/// expiry.
fn relay_ticket(role: u8, env: u8, session: &str, device: &crate::devices::Listed, workspace: &str, fence: u64, now: Timestamp) -> (String, Timestamp) {
    let iat = now.as_second() as u64;
    let t = krowk_client::relay_ticket::Ticket {
        kid: TICKET_KID,
        role,
        env,
        session: unhex(&session.replace('-', "")).and_then(|b| b.try_into().ok()).unwrap_or_default(),
        device: unhex(&device.id).and_then(|b| b.try_into().ok()).unwrap_or_default(),
        signing_key: device.signing_key,
        fence,
        iat,
        exp: iat + TICKET_TTL as u64,
        workspace: workspace.to_string(),
    };
    (t.sign(&TICKET_SEED), now + SignedDuration::from_secs(TICKET_TTL))
}

/// `env` as a lease or ticket call names it: production when absent.
fn env_field(f: &Fields) -> Result<u8, Resp> {
    match f.get_value("env") {
        None | Some(Value::Null) => Ok(1),
        Some(Value::Str(e)) if e == "production" => Ok(1),
        Some(Value::Str(e)) if e == "development" => Ok(2),
        _ => Err(invalid("env", "must be production or development")),
    }
}

/// A lease call's answer, with the holder's host ticket when it has a
/// signing key. `token` only from the call that minted it.
fn serialize_lease_with(s: &Session, token: Option<&str>, ticket: Option<(String, Timestamp)>) -> Json {
    let mut pairs = lease_pairs(s, token);
    if let Some((t, exp)) = ticket {
        pairs.push(("relay_ticket".to_owned(), Json::str(&t)));
        pairs.push(("relay_ticket_expires_at".to_owned(), Json::str(rfc3339_nano(exp))));
    }
    Json::map_of(pairs)
}

fn lease_pairs(s: &Session, token: Option<&str>) -> Vec<(String, Json)> {
    let mut pairs = vec![
        ("session".to_owned(), Json::str(&s.id)),
        ("device".to_owned(), Json::str(&s.holder)),
        ("fence".to_owned(), Json::Int(s.fence as i64)),
    ];
    if let Some(t) = token {
        pairs.push(("token".to_owned(), Json::str(t)));
    }
    pairs.push(("expires_at".to_owned(), s.lease_expires_at.map_or(Json::Null, |e| Json::str(rfc3339_nano(e)))));
    pairs
}

fn nullable(s: &str) -> Json {
    if s.is_empty() { Json::Null } else { Json::str(s) }
}

/// A session: never the fence or the token, and in a listing not the
/// sealed index either.
fn serialize_session(s: &Session, now: Timestamp, listing: bool) -> Json {
    let lease = if leased(s, now) {
        Json::map([("device", Json::str(&s.holder)), ("expires_at", s.lease_expires_at.map_or(Json::Null, |e| Json::str(rfc3339_nano(e))))])
    } else {
        Json::Null
    };
    // Every session here is private, sealed under its owner's user key, as
    // the registry's `seal` says (engineering/devices.md → Keys). The
    // stand-in has no people, so it names no owner.
    let mut pairs = vec![("id".to_owned(), Json::str(&s.id)), ("wrapped_key".to_owned(), Json::str(&s.wrapped_key)), ("seal".to_owned(), Json::str("user")), ("record_signature".to_owned(), nullable(&s.record_signature)), ("signer".to_owned(), nullable(&s.signer))];
    if !listing {
        pairs.push(("sealed_index".to_owned(), if s.sealed_index.is_empty() { Json::Null } else { Json::str(&s.sealed_index) }));
    }
    pairs.extend([
        ("owner_user_id".to_owned(), Json::str(&s.owner)),
        // `user` until shared sessions are sealed to a workspace key.
        ("seal".to_owned(), Json::str("user")),
        ("sealed_index_size".to_owned(), Json::Int((s.sealed_index.len() / 2) as i64)),
        ("lease".to_owned(), lease),
        ("created_at".to_owned(), Json::str(rfc3339_nano(s.created_at))),
        ("updated_at".to_owned(), Json::str(rfc3339_nano(s.updated_at))),
        ("last_written_at".to_owned(), Json::str(rfc3339_nano(s.last_written_at))),
    ]);
    Json::map_of(pairs)
}

/// A session id as the registry routes it: a UUID, else no such session.
fn session_id(id: &str) -> Option<String> {
    let id = id.to_ascii_lowercase();
    let shape = id.len() == 36 && id.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() });
    shape.then_some(id)
}

/// Most recently written first, a page at a time, as the registry pages.
pub fn list_sessions(app: &App, req: &Req) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let limit = crate::artifacts::page_limit(req);
    let s = app.lock();
    let now = s.now();
    let mut mine: Vec<&Session> = s.sync.sessions.iter().filter(|((w, _), _)| *w == workspace).map(|(_, x)| x).collect();
    mine.sort_by_key(|x| (std::cmp::Reverse(x.last_written_at), std::cmp::Reverse(x.seq)));
    let before = req.query_get("before");
    if !before.is_empty() {
        let Some(cursor) = session_id(&before).and_then(|id| s.sync.sessions.get(&(workspace.clone(), id))) else { return not_found() };
        let at = (cursor.last_written_at, cursor.seq);
        mine.retain(|x| (x.last_written_at, x.seq) < at);
    }
    let (page, next) = crate::artifacts::paginate(mine, limit, |x| &x.id);
    Resp::json(200, &Json::map([("sessions", Json::Arr(page.into_iter().map(|x| serialize_session(x, now, true)).collect())), ("next", next)]))
}

pub fn show_session(app: &App, req: &Req, id: &str) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    let now = s.now();
    session_id(id).and_then(|id| s.sync.sessions.get(&(workspace, id))).map_or_else(not_found, |x| Resp::json(200, &serialize_session(x, now, false)))
}

fn lease_stale(s: &Session, now: Timestamp) -> Resp {
    let held = if leased(s, now) { "" } else { " (nobody holds it)" };
    error(
        409,
        "lease_stale",
        &format!("this is not the token of the session's live lease{held} — another device took the lease, it lapsed, or it was never this caller's; acquire it again"),
        None,
    )
}

/// Whether `token` is the live lease's, compared as digests.
/// The lease holder's token, presented by the holder itself: the request
/// is signed by the device holding the lease (`signed`), so a token copied
/// to another device writes nothing.
fn holder(s: &Session, token: &str, now: Timestamp, signer: &str) -> Result<(), Resp> {
    let presented = crate::store::sha256_hex(token.as_bytes());
    if leased(s, now) && !s.token_digest.is_empty() && presented == s.token_digest && s.holder == signer { Ok(()) } else { Err(lease_stale(s, now)) }
}

/// A lease call of the holder's is signed by the holder, whatever token it
/// presents (Api::Sync#refuse_unless_signed_by_holder!).
fn signed_by_holder(x: &Session, signer: &str) -> Result<(), Resp> {
    if x.holder.is_empty() || x.holder == signer {
        Ok(())
    } else {
        Err(error(409, "lease_stale", &format!("device {signer} does not hold this session's lease — only its holder writes"), None))
    }
}

/// A new lease token, and its digest to keep.
fn mint(s: &mut Session) -> String {
    let token = crate::store::random_token()[..32].to_owned();
    s.token_digest = crate::store::sha256_hex(token.as_bytes());
    token
}

/// Creates the session under its id, or writes its sealed index — the
/// latter the lease holder's, presenting its token. The wrapped key changes
/// only in a re-seal (`resealable`) or a key rotation (`rotatable`); a body
/// naming no `sealed_index` leaves it as it is.
pub fn put_session(app: &App, req: &mut Req, id: &str, signer: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let id = session_id(id).ok_or_else(not_found)?;
        let v = body(req, "session")?;
        let mut f = v.fields();
        let wrapped = hex(&wrapped_key(&required(&mut f, "wrapped_key")?)?);
        let named = f.raw("sealed_index").is_some();
        let sealed = f.string("sealed_index");
        let sealed = if sealed.is_empty() { String::new() } else { hex(&blob("sealed_index", &sealed, None, Some(MAX_SEALED_INDEX_BYTES))?) };
        let token = f.string("lease_token");
        let record_signature = f.string("record_signature");
        let record_signature = if record_signature.is_empty() { String::new() } else { hex(&blob("record_signature", &record_signature, Some(64), None)?) };
        let record_signer = f.string("signer");
        let mut s = app.lock();
        let now = s.now();
        burst(&mut s.sync, &caller(req), "session_writes", now)?;
        let seq = s.sync.seq + 1;
        let key = (workspace.clone(), id.clone());
        let holder_revoked = s.sync.sessions.get(&key).and_then(|x| revoked_holder(&s.sync, x));
        let cap = if s.sync.max_sessions == 0 { MAX_SESSIONS } else { s.sync.max_sessions };
        let held = s.sync.sessions.keys().filter(|(w, _)| *w == workspace).count();
        let reseal = s.sync.sessions.get(&key).is_some_and(|x| resealable(&s.sync.people, x, &wrapped, (&record_signature, &record_signer), signer));
        let rotate = !reseal && s.sync.sessions.get(&key).is_some_and(|x| rotatable(x, &wrapped, (&record_signature, &record_signer), signer));
        let current_epoch = s.sync.sessions.get(&key).map_or(0, |x| epoch_of(&s.sync.people, &x.owner));
        let Some(x) = s.sync.sessions.get_mut(&key) else {
            if held >= cap {
                return Err(error(422, "session_limit_reached", &format!("this workspace holds {cap} synced sessions, the most one may"), None));
            }
            let owner = crate::store::person_for(&crate::auth::token(req).unwrap_or_default());
            let sealed_epoch = epoch_of(&s.sync.people, &owner);
            let x = Session { id, wrapped_key: wrapped, record_signature, signer: record_signer, sealed_index: sealed, fence: 0, token_digest: String::new(), holder: String::new(), lease_expires_at: None, created_at: now, updated_at: now, last_written_at: now, seq, owner, sealed_epoch };
            let resp = Resp::json(201, &serialize_session(&x, now, false));
            s.sync.sessions.insert(key, x);
            s.sync.seq = seq;
            return Ok(resp);
        };
        if reseal {
            if token.is_empty() {
                return Err(parameter_missing("lease_token"));
            }
            holder(x, &token, now, signer)?;
            if let Some(refused) = &holder_revoked {
                return Err(crate::devices::revoked(refused));
            }
            (x.wrapped_key, x.record_signature, x.signer, x.sealed_epoch) = (wrapped.clone(), record_signature.clone(), record_signer.clone(), current_epoch);
            x.updated_at = now;
        }
        if rotate {
            if token.is_empty() {
                return Err(parameter_missing("lease_token"));
            }
            holder(x, &token, now, signer)?;
            if let Some(refused) = &holder_revoked {
                return Err(crate::devices::revoked(refused));
            }
            (x.wrapped_key, x.record_signature, x.signer) = (wrapped.clone(), record_signature.clone(), record_signer.clone());
            x.updated_at = now;
        }
        if x.wrapped_key != wrapped {
            return Err(invalid("wrapped_key", "is set when the session is created and cannot change"));
        }
        if named && x.sealed_index != sealed {
            if token.is_empty() {
                return Err(parameter_missing("lease_token"));
            }
            holder(x, &token, now, signer)?;
            if let Some(refused) = &holder_revoked {
                return Err(crate::devices::revoked(refused));
            }
            x.sealed_index = sealed;
            x.last_written_at = now;
            x.updated_at = now;
        }
        Ok(Resp::json(200, &serialize_session(x, now, false)))
    };
    run().unwrap_or_else(|r| r)
}

/// The lease length asked for: absent is the default, and anything else
/// that is not a whole number of seconds in the window is refused, as the
/// registry's `Integer(…)` refuses it — a number, or a string of one.
fn ttl(f: &Fields) -> Result<SignedDuration, Resp> {
    let secs = match f.get_value("ttl") {
        None => Some(DEFAULT_LEASE_TTL),
        Some(Value::Num(n)) => n.parse().ok(),
        Some(Value::Str(t)) => t.parse().ok(),
        Some(_) => None,
    };
    if let Some(secs) = secs.filter(|s| LEASE_TTL.contains(s)) {
        Ok(SignedDuration::from_secs(secs))
    } else {
        Err(invalid("ttl", &format!("must be a whole number of seconds from {} to {}", LEASE_TTL.start(), LEASE_TTL.end())))
    }
}

/// A lease call, once its session and device are known to be the
/// workspace's: the store still locked, the session's key, the device and
/// the body's `lease`.
type LeaseCall<'a> = (std::sync::MutexGuard<'a, crate::store::Store>, (String, String), String, Value);

/// The session a lease call is for, the workspace's, and the device it
/// names, which each call looks up in its own turn.
fn lease_call<'a>(app: &'a App, req: &mut Req, id: &str, needs_device: bool) -> Result<LeaseCall<'a>, Resp> {
    let workspace = gate(req)?;
    let id = session_id(id).ok_or_else(not_found)?;
    let v = body(req, "lease")?;
    let mut f = v.fields();
    let device = if needs_device { id_field("device", &required(&mut f, "device")?)? } else { String::new() };
    let s = app.lock();
    let key = (workspace.clone(), id);
    if !s.sync.sessions.contains_key(&key) {
        return Err(not_found());
    }
    Ok((s, key, device, v))
}

pub fn acquire_lease(app: &App, req: &mut Req, id: &str, signer: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let (mut s, key, device, v) = lease_call(app, req, id, true)?;
        // Only the device asking is checked: a lease whose holder was since
        // removed is `lease_held` until it lapses, and free after.
        named(&s.sync, &person(req), &device)?;
        signed_as(&device, signer)?;
        let ttl = ttl(&v.fields())?;
        let env = env_field(&v.fields())?;
        let now = s.now();
        let x = s.sync.sessions.get_mut(&key).unwrap();
        if leased(x, now) {
            let until = rfc3339_nano(x.lease_expires_at.unwrap());
            return Err(error(
                409,
                "lease_held",
                &format!("device {} holds this session's lease until {until} — send it commands instead, or wait for the lease to lapse", x.holder),
                Some(Json::map([("device", Json::str(&x.holder)), ("expires_at", Json::str(until))])),
            ));
        }
        x.fence += 1;
        x.holder = device.clone();
        x.lease_expires_at = Some(now + ttl);
        x.updated_at = now;
        let token = mint(x);
        let x = x.clone();
        let ticket = s.sync.people.get(&x.owner).and_then(|p| p.device(&device)).map(|d| relay_ticket(1, env, &key.1, d, &key.0, x.fence, now));
        let resp = Resp::json(201, &serialize_lease_with(&x, Some(&token), ticket));
        touch(&mut s.sync, &x.owner, &device, now);
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

pub fn renew_lease(app: &App, req: &mut Req, id: &str, signer: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let (mut s, key, device, v) = lease_call(app, req, id, true)?;
        signed_by_holder(&s.sync.sessions[&key], signer)?;
        let mut f = v.fields();
        let token = required(&mut f, "token")?;
        // The device it is kept by or handed to, which may not be a removed
        // or revoked one.
        crate::devices::refuse_if_revoked(named(&s.sync, &person(req), &device)?)?;
        let ttl = ttl(&f)?;
        let env = env_field(&f)?;
        let now = s.now();
        let refused = revoked_holder(&s.sync, &s.sync.sessions[&key]);
        let x = s.sync.sessions.get_mut(&key).unwrap();
        holder(x, &token, now, signer)?;
        if let Some(refused) = &refused {
            return Err(crate::devices::revoked(refused));
        }
        let minted = if x.holder != device {
            x.fence += 1;
            x.holder = device.clone();
            Some(mint(x))
        } else {
            None
        };
        x.lease_expires_at = Some(now + ttl);
        x.updated_at = now;
        let x = x.clone();
        let ticket = s.sync.people.get(&x.owner).and_then(|p| p.device(&device)).map(|d| relay_ticket(1, env, &key.1, d, &key.0, x.fence, now));
        let resp = Resp::json(200, &serialize_lease_with(&x, minted.as_deref(), ticket));
        touch(&mut s.sync, &x.owner, &device, now);
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

/// A viewer's relay ticket, for a device on the person's list asking for
/// itself.
pub fn viewer_ticket(app: &App, req: &mut Req, id: &str, signer: &str) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let id = session_id(id).ok_or_else(not_found)?;
        let q = |name: &str| req.query_get(name);
        let device = id_field("device", &q("device"))?;
        let env = match q("env").as_str() {
            "" | "production" => 1,
            "development" => 2,
            _ => return Err(invalid("env", "must be production or development")),
        };
        let s = app.lock();
        let now = s.now();
        if !s.sync.sessions.contains_key(&(workspace.clone(), id.clone())) {
            return Err(not_found());
        }
        // Every device a list names has a signing key: the entry adding it
        // carries one, so there is no `signing_key_missing` to answer.
        let d = named(&s.sync, &person(req), &device)?;
        signed_as(&device, signer)?;
        let (t, exp) = relay_ticket(2, env, &id, d, &workspace, 0, now);
        Ok(Resp::json(200, &Json::map([("relay_ticket", Json::str(&t)), ("expires_at", Json::str(rfc3339_nano(exp)))])))
    };
    run().unwrap_or_else(|r| r)
}

/// `GET /v1/relay/ticket_keys`: the stand-in's ticket public key, keyless,
/// as the registry publishes its own.
pub fn ticket_keys() -> Resp {
    Resp::json(200, &Json::map([("ticketKeys", Json::map_of(vec![(hex(&TICKET_KID), Json::str(hex(&ticket_public_key())))]))]))
}

pub fn release_lease(app: &App, req: &mut Req, id: &str, signer: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let (mut s, key, _, v) = lease_call(app, req, id, false)?;
        signed_by_holder(&s.sync.sessions[&key], signer)?;
        let token = required(&mut v.fields(), "token")?;
        let now = s.now();
        let refused = revoked_holder(&s.sync, &s.sync.sessions[&key]);
        let x = s.sync.sessions.get_mut(&key).unwrap();
        holder(x, &token, now, signer)?;
        if let Some(refused) = &refused {
            return Err(crate::devices::revoked(refused));
        }
        x.holder.clear();
        x.token_digest.clear();
        x.lease_expires_at = None;
        x.updated_at = now;
        Ok(Resp::empty(204))
    };
    run().unwrap_or_else(|r| r)
}

/// A lease call is the holder being seen, as the registry records it.
fn touch(s: &mut SyncStore, person: &str, device: &str, now: Timestamp) {
    if let Some(d) = s.people.get_mut(person).and_then(|p| p.devices.iter_mut().find(|d| d.id == device)) {
        d.last_seen_at = Some(now);
    }
}

/// The session's lease holder, when it is a device its owner's list
/// removed, or the dashboard revoked: refused once its token is checked
/// (SyncSession#refuse_unless_holder!), never on an acquire.
fn revoked_holder(s: &SyncStore, x: &Session) -> Option<crate::devices::Listed> {
    s.people.get(&x.owner).and_then(|p| p.device(&x.holder)).filter(|d| !d.active()).cloned()
}

/// The dashboard's Revoke, stood in for (Device#revoke!): the person's
/// device `id` refused from now on, and every key bound to it revoked with
/// it. The chain still lists it until one of their devices removes it.
/// Keyed by the bearer, the way `/_approve` stands in for a signed-in
/// person; a device of anyone else's, or the recovery kit, is a 404.
pub fn revoke(app: &App, req: &Req, id: &str) -> Resp {
    if let Err(r) = require_key(req) {
        return r;
    }
    let person = person(req);
    let mut s = app.lock();
    let now = s.now();
    let Some(d) = s.sync.people.get_mut(&person).and_then(|p| p.devices.iter_mut().find(|d| d.id.eq_ignore_ascii_case(id) && d.kind == "device" && d.removed_seq.is_none())) else {
        return not_found();
    };
    d.revoked_at = d.revoked_at.or(Some(now));
    let device = d.id.clone();
    let keys: Vec<String> = s.sync.bindings.iter().filter(|(_, bound)| **bound == device).map(|(key, _)| key.clone()).collect();
    s.sync.revoked_keys.extend(keys);
    Resp::json(200, &Json::map([("device", Json::str(device))]))
}

fn serialize_chunk(c: &Chunk) -> Vec<(String, Json)> {
    vec![
        ("index".to_owned(), Json::Int(c.index as i64)),
        ("slug".to_owned(), Json::str(&c.slug)),
        ("state".to_owned(), Json::str(if c.ready { "ready" } else { "pending" })),
        ("byte_size".to_owned(), Json::Int(c.byte_size)),
        ("checksum".to_owned(), Json::str(&c.checksum)),
        ("created_at".to_owned(), Json::str(rfc3339_nano(c.created_at))),
    ]
}

fn lease_token_missing() -> Resp {
    error(409, "lease_stale", "no lease token was presented — a chunk is the lease holder's to write; acquire the lease first", None)
}

/// A whole-number field that was sent, as the registry's `Integer(…)` reads
/// it: a number, or a string of one.
fn whole(f: &Fields, name: &str) -> Option<i64> {
    match f.get_value(name) {
        Some(Value::Num(n)) => n.parse().ok(),
        Some(Value::Str(t)) => t.parse().ok(),
        _ => None,
    }
}

/// The holder's declare of a chunk: the lease token, then an Idempotency-Key
/// replay or a new chunk with a presigned PUT under `/_storage`.
pub fn declare_chunk(app: &App, req: &mut Req, id: &str, site: &str, signer: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let id = session_id(id).ok_or_else(not_found)?;
        let attempt = crate::artifacts::idempotency_key(req)?;
        let v = body(req, "chunk")?;
        let mut f = v.fields();
        let index = whole(&f, "index").filter(|i| *i >= 0).ok_or_else(|| invalid("index", "must be a whole number from 0"))? as u64;
        let byte_size = whole(&f, "byte_size").ok_or_else(|| invalid("byte_size", "must be a whole number"))?;
        if byte_size <= 0 {
            return Err(invalid("byte_size", "must be greater than 0"));
        }
        if byte_size > MAX_CHUNK_BYTES {
            return Err(invalid("byte_size", &format!("must be at most {MAX_CHUNK_BYTES} bytes")));
        }
        let checksum = f.string("checksum").to_ascii_lowercase();
        if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("checksum", "must be a lowercase hex SHA-256"));
        }
        let token = f.string("lease_token");
        let mut s = app.lock();
        let now = s.now();
        let key = (workspace.clone(), id.clone());
        let x = s.sync.sessions.get(&key).ok_or_else(not_found)?;
        if token.is_empty() {
            return Err(lease_token_missing());
        }
        holder(x, &token, now, signer)?;
        if let Some(refused) = revoked_holder(&s.sync, x) {
            return Err(crate::devices::revoked(&refused));
        }
        burst(&mut s.sync, &caller(req), "chunk_declares", now)?;
        let hash = crate::store::sha256_hex(format!("{id}\n{index}\n{byte_size}\n{checksum}").as_bytes());
        let ck = (workspace.clone(), id.clone(), index);
        if let Some(attempt) = &attempt
            && let Some((found, matches)) = s.replay("chunk", &workspace, attempt, &hash)
        {
            let slug = found.artifact.clone();
            if !matches {
                return Err(crate::errors::key_reused(&slug));
            }
            let c = s.sync.chunks.get_mut(&ck).filter(|c| c.slug == slug).ok_or_else(not_found)?;
            if c.ready {
                return Err(crate::errors::already_finalized(&c.slug));
            }
            c.upload_tok = crate::store::random_token();
            c.upload_til = now + crate::store::UPLOAD_URL_LIFETIME;
            return Ok(Resp::json(201, &declared_chunk(c, site)));
        }
        // A ready chunk, or one this lease declared, is the log's; a pending
        // one an earlier lease declared is replaced, bytes and all.
        let fence = s.sync.sessions[&key].fence;
        if let Some(existing) = s.sync.chunks.get(&ck) {
            if existing.ready || existing.fence == fence {
                return Err(error(409, "chunk_exists", &format!("this session already has chunk {index} — its log is append-only"), None));
            }
            let displaced = s.sync.chunks.remove(&ck).unwrap();
            s.objects.remove(&displaced.storage_key);
        }
        if s.sync.chunks.keys().filter(|(w, sid, _)| *w == workspace && *sid == id).count() >= MAX_CHUNKS {
            return Err(error(422, "chunk_limit_reached", &format!("this session's log holds {MAX_CHUNKS} chunks, the most one may"), None));
        }
        let c = Chunk {
            slug: generate_slug("art"),
            index,
            fence,
            byte_size,
            checksum,
            storage_key: format!("{}/{}/chunk-{index}.bin", crate::store::ARTIFACT_REGION, crate::store::random_base36()),
            upload_tok: crate::store::random_token(),
            upload_til: now + crate::store::UPLOAD_URL_LIFETIME,
            uploaded_sum: None,
            ready: false,
            created_at: now,
        };
        let resp = Resp::json(201, &declared_chunk(&c, site));
        if let Some(attempt) = &attempt {
            s.remember("chunk", &workspace, attempt, crate::store::Answered { request_hash: hash, artifact: c.slug.clone(), run: String::new() });
        }
        s.sync.chunks.insert(ck, c);
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

fn declared_chunk(c: &Chunk, site: &str) -> Json {
    let mut out = serialize_chunk(c);
    let headers = Json::map([
        ("Content-Type", Json::str(CHUNK_CONTENT_TYPE)),
        ("Content-Length", Json::str(c.byte_size.to_string())),
        ("x-amz-checksum-sha256", Json::str(crate::store::base64_sum(&c.checksum))),
    ]);
    out.push((
        "upload".to_owned(),
        Json::map([
            ("method", Json::str("PUT")),
            ("url", Json::str(format!("{site}/_storage/{}?upload_token={}", c.storage_key, c.upload_tok))),
            ("headers", headers),
            ("expires_at", Json::str(rfc3339_nano(c.upload_til))),
        ]),
    ));
    Json::map_of(out)
}

/// Storage's PUT for a chunk's key, with a real signature's checks: the
/// token, the window, the type, the digest header, the length and the
/// digest. None when the key is no chunk's.
pub fn put_chunk_object(app: &App, req: &mut Req, key: &str) -> Option<Resp> {
    let found = {
        let s = app.lock();
        s.sync.chunks.iter().find(|(_, c)| c.storage_key == key).map(|(k, c)| (Ok(k.clone()), c.upload_tok.clone(), c.checksum.clone(), c.byte_size, c.upload_til, c.ready)).or_else(|| {
            s.sync.vintages.iter().find(|(_, (_, c))| c.storage_key == key).map(|(k, (_, c))| (Err(k.clone()), c.upload_tok.clone(), c.checksum.clone(), c.byte_size, c.upload_til, c.ready))
        })
    };
    let (ck, token, sum, size, until, ready) = found?;
    if ready || token.is_empty() || req.query_get("upload_token") != token {
        return Some(Resp::xml(403, "SignatureDoesNotMatch"));
    }
    if app.lock().now() > until {
        return Some(Resp::xml(403, "AccessDenied"));
    }
    if req.header("Content-Type").unwrap_or("") != CHUNK_CONTENT_TYPE || req.header("x-amz-checksum-sha256").unwrap_or("") != crate::store::base64_sum(&sum) {
        return Some(Resp::xml(403, "SignatureDoesNotMatch"));
    }
    let Ok(bytes) = req.read_body(size as u64 + 1) else { return Some(Resp::xml(400, "IncompleteBody")) };
    if bytes.len() as i64 != size {
        return Some(Resp::xml(400, "IncorrectContentLength"));
    }
    let got = crate::store::sha256_hex(&bytes);
    if got != sum {
        return Some(Resp::xml(400, "BadDigest"));
    }
    let mut s = app.lock();
    let c = match &ck {
        Ok(ck) => s.sync.chunks.get_mut(ck),
        Err(vk) => s.sync.vintages.get_mut(vk).map(|(_, c)| c),
    };
    if let Some(c) = c {
        c.uploaded_sum = Some(got);
    }
    s.objects.insert(key.to_owned(), bytes);
    Some(Resp::empty(200))
}

/// The holder's confirmation that a chunk landed: the token again, then
/// what storage holds checked against the declare. Idempotent.
pub fn finalize_chunk(app: &App, req: &mut Req, id: &str, index: &str, signer: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let id = session_id(id).ok_or_else(not_found)?;
        let index: u64 = index.parse().map_err(|_| not_found())?;
        let token = match decode(req, 1 << 16)? {
            Decoded::Value(v) => v.get("chunk").map(|m| m.value.fields().string("lease_token")).unwrap_or_default(),
            _ => String::new(),
        };
        let mut s = app.lock();
        let now = s.now();
        let x = s.sync.sessions.get(&(workspace.clone(), id.clone())).ok_or_else(not_found)?;
        if token.is_empty() {
            return Err(lease_token_missing());
        }
        holder(x, &token, now, signer)?;
        if let Some(refused) = revoked_holder(&s.sync, x) {
            return Err(crate::devices::revoked(&refused));
        }
        let fence = x.fence;
        let c = s.sync.chunks.get_mut(&(workspace.clone(), id.clone(), index)).ok_or_else(not_found)?;
        if c.fence != fence {
            return Err(error(
                409,
                "lease_stale",
                &format!("chunk {index} was declared under fence {}, not this lease's {fence} — declare it again under this lease", c.fence),
                None,
            ));
        }
        if !c.ready {
            match &c.uploaded_sum {
                None => return Err(error(409, "upload_missing", &format!("nothing uploaded for {} yet", c.slug), None)),
                Some(sum) if *sum != c.checksum => return Err(error(422, "checksum_mismatch", "what was uploaded does not match the declared checksum", None)),
                Some(_) => {}
            }
            c.ready = true;
            c.upload_tok.clear();
        }
        let resp = Resp::json(200, &Json::map_of(serialize_chunk(c)));
        if let Some(x) = s.sync.sessions.get_mut(&(workspace, id)) {
            x.last_written_at = now;
            x.updated_at = now;
        }
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

/// A session's ready chunks from after `after`, in log order, each with the
/// URL its bytes are read from.
pub fn list_chunks(app: &App, req: &Req, id: &str, site: &str) -> Resp {
    let workspace = match gate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let Some(id) = session_id(id) else { return not_found() };
    let limit = crate::artifacts::page_limit(req);
    let s = app.lock();
    if !s.sync.sessions.contains_key(&(workspace.clone(), id.clone())) {
        return not_found();
    }
    let after: Option<u64> = req.query_get("after").parse().ok();
    let mut ready: Vec<&Chunk> = s.sync.chunks.iter().filter(|((w, sid, i), c)| *w == workspace && *sid == id && c.ready && after.is_none_or(|a| *i > a)).map(|(_, c)| c).collect();
    ready.sort_by_key(|c| c.index);
    ready.truncate(limit);
    let next = if ready.len() == limit { ready.last().map_or(Json::Null, |c| Json::Int(c.index as i64)) } else { Json::Null };
    let page = ready
        .into_iter()
        .map(|c| {
            let mut j = serialize_chunk(c);
            j.push(("url".to_owned(), Json::str(format!("{site}/_storage/{}", c.storage_key))));
            Json::map_of(j)
        })
        .collect();
    Resp::json(200, &Json::map([("chunks", Json::Arr(page)), ("next", next)]))
}

fn serialize_vintage(week: &str, c: &Chunk) -> Vec<(String, Json)> {
    let mut out = serialize_chunk(c);
    out.retain(|(k, _)| k != "index");
    out.insert(1, ("week".to_owned(), Json::str(week)));
    out
}

/// The week's ready vintage in a workspace: its slug.
fn current_vintage(s: &SyncStore, workspace: &str, week: &str) -> Option<String> {
    s.vintages.iter().find(|(k, (wk, c))| k.0 == workspace && wk == week && c.ready && !s.vintages_replaced.contains(*k)).map(|((_, slug), _)| slug.clone())
}

fn vintage_conflict(week: &str) -> Resp {
    error(409, "vintage_conflict", &format!("the vintage for {week} was replaced since it was read — read it again and merge"), None)
}

/// A device's declare of a week's vintage (the registry's
/// VintagesController#create): an Idempotency-Key replay, or a new
/// vintage with a presigned PUT, refused as `vintage_conflict` unless
/// `replaces` names the week's ready vintage, or there is none and it
/// names nothing.
pub fn declare_vintage(app: &App, req: &mut Req, site: &str, signer: &str) -> Resp {
    let mut run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let attempt = crate::artifacts::idempotency_key(req)?;
        let v = body(req, "vintage")?;
        let mut f = v.fields();
        let week = f.string("week");
        let b = week.as_bytes();
        let valid = b.len() == 8 && b[..4].iter().all(u8::is_ascii_digit) && &b[4..6] == b"-W" && b[6..].iter().all(u8::is_ascii_digit) && (1..=53).contains(&week[6..].parse::<u32>().unwrap_or(0));
        if !valid {
            return Err(invalid("week", "must be an ISO week, like 2026-W38"));
        }
        let byte_size = whole(&f, "byte_size").ok_or_else(|| invalid("byte_size", "must be a whole number"))?;
        if byte_size <= 0 {
            return Err(invalid("byte_size", "must be greater than 0"));
        }
        if byte_size > MAX_VINTAGE_BYTES {
            return Err(invalid("byte_size", &format!("must be at most {MAX_VINTAGE_BYTES} bytes")));
        }
        let checksum = f.string("checksum").to_ascii_lowercase();
        if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("checksum", "must be a lowercase hex SHA-256"));
        }
        let replaces = f.string("replaces");
        let mut s = app.lock();
        let now = s.now();
        named(&s.sync, &person(req), signer)?;
        burst(&mut s.sync, &caller(req), "vintage_declares", now)?;
        let hash = crate::store::sha256_hex(format!("{week}\n{byte_size}\n{checksum}\n{replaces}").as_bytes());
        if let Some(attempt) = &attempt
            && let Some((found, matches)) = s.replay("vintage", &workspace, attempt, &hash)
        {
            let slug = found.artifact.clone();
            if !matches {
                return Err(crate::errors::key_reused(&slug));
            }
            let (wk, c) = s.sync.vintages.get_mut(&(workspace.clone(), slug)).ok_or_else(not_found)?;
            if c.ready {
                return Err(crate::errors::already_finalized(&c.slug));
            }
            c.upload_tok = crate::store::random_token();
            c.upload_til = now + crate::store::UPLOAD_URL_LIFETIME;
            return Ok(Resp::json(201, &declared_vintage(&wk.clone(), c, site)));
        }
        if current_vintage(&s.sync, &workspace, &week).unwrap_or_default() != replaces {
            return Err(vintage_conflict(&week));
        }
        let c = Chunk {
            slug: generate_slug("art"),
            index: 0,
            fence: 0,
            byte_size,
            checksum,
            storage_key: format!("{}/{}/vintage-{week}.bin", crate::store::ARTIFACT_REGION, crate::store::random_base36()),
            upload_tok: crate::store::random_token(),
            upload_til: now + crate::store::UPLOAD_URL_LIFETIME,
            uploaded_sum: None,
            ready: false,
            created_at: now,
        };
        let resp = Resp::json(201, &declared_vintage(&week, &c, site));
        if let Some(attempt) = &attempt {
            s.remember("vintage", &workspace, attempt, crate::store::Answered { request_hash: hash, artifact: c.slug.clone(), run: String::new() });
        }
        s.sync.vintage_replaces.insert((workspace.clone(), c.slug.clone()), replaces);
        s.sync.vintages.insert((workspace, c.slug.clone()), (week, c));
        Ok(resp)
    };
    run().unwrap_or_else(|r| r)
}

fn declared_vintage(week: &str, c: &Chunk, site: &str) -> Json {
    let mut out = serialize_vintage(week, c);
    let Json::Obj(upload) = declared_chunk(c, site) else { unreachable!("a declare is an object") };
    out.extend(upload.into_iter().filter(|(k, _)| k == "upload"));
    Json::map_of(out)
}

/// A vintage's bytes landed: checked against the declare, then it is the
/// week's, and the one it replaced is kept but no longer listed, as the
/// registry keeps it for its retention window. Idempotent.
pub fn finalize_vintage(app: &App, req: &mut Req, slug: &str, signer: &str) -> Resp {
    let run = || -> Result<Resp, Resp> {
        let workspace = gate(req)?;
        let mut s = app.lock();
        named(&s.sync, &person(req), signer)?;
        let (week, c) = s.sync.vintages.get(&(workspace.clone(), slug.to_owned())).ok_or_else(not_found)?;
        let week = week.clone();
        if c.ready {
            return Ok(Resp::json(200, &Json::map_of(serialize_vintage(&week, c))));
        }
        match &c.uploaded_sum {
            None => return Err(error(409, "upload_missing", &format!("nothing uploaded for {} yet", c.slug), None)),
            Some(sum) if *sum != c.checksum => return Err(error(422, "checksum_mismatch", "what was uploaded does not match the declared checksum", None)),
            Some(_) => {}
        }
        // The compare-and-swap again, as the registry runs it under its
        // lock: another machine may have replaced the week since the declare.
        let replaces = s.sync.vintage_replaces.get(&(workspace.clone(), slug.to_owned())).cloned().unwrap_or_default();
        if current_vintage(&s.sync, &workspace, &week).unwrap_or_default() != replaces {
            return Err(vintage_conflict(&week));
        }
        if let Some(old) = current_vintage(&s.sync, &workspace, &week) {
            s.sync.vintages_replaced.insert((workspace.clone(), old));
        }
        let (_, c) = s.sync.vintages.get_mut(&(workspace, slug.to_owned())).ok_or_else(not_found)?;
        c.ready = true;
        c.upload_tok.clear();
        Ok(Resp::json(200, &Json::map_of(serialize_vintage(&week, c))))
    };
    run().unwrap_or_else(|r| r)
}

/// The workspace's ready vintages, the week's alone when `week` is given,
/// each with the URL its bytes are read from.
pub fn list_vintages(app: &App, req: &Req, site: &str) -> Resp {
    let workspace = match active(app, req).and_then(|_| gate(req)) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let week = req.query_get("week");
    let s = app.lock();
    let mut ready: Vec<(&String, &Chunk)> =
        s.sync.vintages.iter().filter(|(k, (wk, c))| k.0 == workspace && c.ready && !s.sync.vintages_replaced.contains(*k) && (week.is_empty() || *wk == week)).map(|(_, (wk, c))| (wk, c)).collect();
    ready.sort_by(|a, b| a.0.cmp(b.0));
    let page = ready
        .into_iter()
        .map(|(wk, c)| {
            let mut j = serialize_vintage(wk, c);
            j.push(("url".to_owned(), Json::str(format!("{site}/_storage/{}", c.storage_key))));
            Json::map_of(j)
        })
        .collect();
    Resp::json(200, &Json::map([("vintages", Json::Arr(page))]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Person;

    fn session(sealed_epoch: u64) -> Session {
        let t = Timestamp::UNIX_EPOCH;
        Session { id: String::new(), wrapped_key: "old".into(), record_signature: "aa".into(), signer: "me".into(), sealed_index: String::new(), fence: 0, token_digest: String::new(), holder: String::new(), lease_expires_at: None, created_at: t, updated_at: t, last_written_at: t, seq: 0, owner: "p".into(), sealed_epoch }
    }

    /// The re-seal after a start-over, as the registry has it: a session
    /// sealed under an older epoch of the owner's chain is replaceable by
    /// its lease holder, with a new key and a record naming itself — even
    /// one the same device signed before, since a start-over keeps its key.
    /// One sealed under the current epoch, with no record signer, or with
    /// the key or record unchanged, never is.
    #[test]
    fn d6_a_session_sealed_under_an_older_epoch_is_resealable_by_its_holder() {
        let mut people = HashMap::new();
        people.insert("p".to_string(), Person { epoch: 2, ..Person::default() });
        assert!(resealable(&people, &session(1), "new", ("bb", "me"), "me"), "the same device's record, from the old list");
        assert!(!resealable(&people, &session(2), "new", ("bb", "me"), "me"), "sealed under the current epoch");
        let mut unsigned = session(1);
        unsigned.signer.clear();
        assert!(!resealable(&people, &unsigned, "new", ("bb", "me"), "me"), "no record signer");
        assert!(!resealable(&people, &session(1), "old", ("bb", "me"), "me"), "the key unchanged");
        assert!(!resealable(&people, &session(1), "new", ("aa", "me"), "me"), "the record unchanged");
        assert!(!resealable(&people, &session(1), "new", ("bb", "someone-else"), "me"), "the record names another device");
    }
}
