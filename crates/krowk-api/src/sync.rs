//! Sync's records and calls (canon, engineering/crypto.md, devices.md): a
//! person's device list and user key, the mailbox a new device is paired
//! through, and sessions with their leases. Every call needs a key to a paid
//! workspace
//! (R-SYNC-1), and nothing here carries plaintext: keys and sealed blobs are
//! hex, and a session's title and the rest of what a listing shows travel
//! inside `sealed_index`, sealed by the caller before it gets here.
//!
//! None of it takes an Idempotency-Key. A device list entry names its seq,
//! a session's id is the client's own and in the path, and a pairing step is
//! sent once — a lost response costs reading again.
//!
//! This crate carries bytes and nothing else. What makes adding a device safe
//! against a hostile registry — pairing under a short code the registry
//! cannot test guesses against, and verifying the device list against its
//! pin — is krowk-client's and
//! the command line's to do with what these calls return.
//!
//! Every call that acts as a device — appending to the device list, a lease
//! call, reading or writing a session or its chunks, asking for a relay
//! ticket — is signed by this machine's device key (`Client::signed_by`), and
//! refused before it is sent when the client has none. Reading the device
//! list is the API key's.

use crate::client::{Client, slug_path};
use crate::types::Upload;
use crate::error::Error;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

const ATTEMPTS: u32 = 3;

fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A device as the registry indexes the person's device list: for showing,
/// and for a dashboard Revoke the chain has not caught up with
/// (`revoked_at` set, `removed_seq` not). What a client trusts is the chain
/// itself (`device_list`), verified against its pin; this is never a list
/// anything is wrapped to.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ListedDevice {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    /// `device` or `recovery`.
    #[serde(default, deserialize_with = "nullable")]
    pub kind: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub os: String,
    #[serde(default, deserialize_with = "nullable")]
    pub added_seq: Option<u64>,
    #[serde(default, deserialize_with = "nullable")]
    pub removed_seq: Option<u64>,
    #[serde(default, deserialize_with = "nullable")]
    pub last_seen_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub revoked_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ListedDevices {
    #[serde(default, deserialize_with = "nullable")]
    devices: Vec<ListedDevice>,
}

/// One entry of a person's device list, exactly as it was posted, with when
/// the registry received it (Unix seconds).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ListEntry {
    #[serde(default, deserialize_with = "nullable")]
    pub seq: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub entry: String,
    #[serde(default, deserialize_with = "nullable")]
    pub signatures: String,
    #[serde(default, deserialize_with = "nullable")]
    pub received_at: u64,
}

/// The head of the chain as the registry holds it: what a client checks
/// against its pin, never what it pins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ListHead {
    #[serde(default, deserialize_with = "nullable")]
    pub seq: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub hash: String,
}

/// A page of a person's device list from `after`, the epoch it belongs to
/// (a start-over is a new epoch, never a shorter chain) and the head.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeviceList {
    #[serde(default, deserialize_with = "nullable")]
    pub epoch: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub entries: Vec<ListEntry>,
    #[serde(default, deserialize_with = "nullable")]
    pub head: Option<ListHead>,
    #[serde(default, deserialize_with = "nullable")]
    pub next: Option<u64>,
}

/// What a device list post carries, hex throughout: the signed entries, each
/// generation it makes wrapped under the next (`links`, oldest first), and
/// the newest generation wrapped to each device it names.
#[derive(Debug, Clone, Default)]
pub struct ListPost {
    pub entries: Vec<(String, String)>,
    pub links: Vec<String>,
    pub wraps: Vec<(String, String)>,
    pub start_over: bool,
}

impl ListPost {
    fn body(&self) -> Value {
        let entries: Vec<Value> = self.entries.iter().map(|(e, s)| json!({ "entry": e, "signatures": s })).collect();
        let wraps: Vec<Value> = self.wraps.iter().map(|(d, w)| json!({ "device": d, "wrapped_key": w })).collect();
        let list = json!({ "entries": entries, "links": self.links, "wraps": wraps });
        json!({ "device_list": list })
    }
}

/// One generation of the user key: its id, and its wrap of the one before
/// (empty for generation 1).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KeyGeneration {
    #[serde(default, deserialize_with = "nullable")]
    pub generation: u32,
    #[serde(default, deserialize_with = "nullable")]
    pub key_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub wrapped_previous: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KeyWrap {
    #[serde(default, deserialize_with = "nullable")]
    pub generation: u32,
    #[serde(default, deserialize_with = "nullable")]
    pub wrapped_key: String,
}

/// The user key as the signing device can open it: every generation, and
/// the ones wrapped to this device.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserKeyWraps {
    #[serde(default, deserialize_with = "nullable")]
    pub generations: Vec<KeyGeneration>,
    #[serde(default, deserialize_with = "nullable")]
    pub wraps: Vec<KeyWrap>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PairingDevice {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
}

/// A pairing as the mailbox holds it: where it is, and whichever of its
/// messages have arrived, hex. Everything in it is opaque to the registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Pairing {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "nullable")]
    pub initiator_device: PairingDevice,
    #[serde(default, deserialize_with = "nullable")]
    pub initiator_message: String,
    #[serde(default, deserialize_with = "nullable")]
    pub joiner_message: String,
    #[serde(default, deserialize_with = "nullable")]
    pub joiner_confirmation: String,
    #[serde(default, deserialize_with = "nullable")]
    pub sealed_reply: String,
    #[serde(default, deserialize_with = "nullable")]
    pub joiner_ack: String,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub poll_interval: u64,
}

/// A step of a pairing, in the order they are taken after the open: B's
/// hello, A's SPAKE2 message, B's confirmation, A's sealed reply, B's ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStep {
    Join,
    Answer,
    Confirmation,
    Reply,
    Acknowledgement,
}

impl PairingStep {
    fn route(self) -> (&'static str, &'static str) {
        match self {
            PairingStep::Join => ("join", "joiner_message"),
            PairingStep::Answer => ("answer", "initiator_message"),
            PairingStep::Confirmation => ("confirmation", "joiner_confirmation"),
            PairingStep::Reply => ("reply", "sealed_reply"),
            PairingStep::Acknowledgement => ("acknowledgement", "joiner_ack"),
        }
    }

    /// The paired device's steps, which are signed; the new machine's are
    /// its key's alone, since it is not a device yet.
    fn signed(self) -> bool {
        matches!(self, PairingStep::Answer | PairingStep::Reply)
    }
}

/// A lease call's answer: the session's one writer, until when, the fence
/// that orders holders, and — from an acquire or a hand-over only — the
/// token every write the holder makes presents (R-SYNC-2). The token is the
/// holder's proof and nobody else ever sees it; the fence is no secret.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    #[serde(default, deserialize_with = "nullable")]
    pub session: String,
    #[serde(default, deserialize_with = "nullable")]
    pub device: String,
    #[serde(default, deserialize_with = "nullable")]
    pub fence: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub token: String,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: String,
    /// The holder's host ticket for the relay (relay.md → Tickets), at
    /// this lease's fence; empty when the holder has no signing key.
    #[serde(default, deserialize_with = "nullable")]
    pub relay_ticket: String,
    #[serde(default, deserialize_with = "nullable")]
    pub relay_ticket_expires_at: String,
}

/// Who holds a session's lease, as a session reports it: never the fence or
/// the token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LeaseHolder {
    #[serde(default, deserialize_with = "nullable")]
    pub device: String,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: String,
}

/// A relay ticket, hex, and when it lapses.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RelayTicket {
    #[serde(default, deserialize_with = "nullable")]
    pub relay_ticket: String,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: String,
}

/// A synced session as the registry holds it: ciphertext, sizes, times and
/// who holds the lease. A listing leaves `sealed_index` empty; `show` has it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SyncSession {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub wrapped_key: String,
    /// The person who owns it, whose user key its key is wrapped under.
    /// The registry sets it from the calling key; a client never sends it.
    #[serde(default, deserialize_with = "nullable")]
    pub owner_user_id: String,
    /// Which key wraps its session key: `user` today, `workspace` once
    /// shared sessions exist (engineering/devices.md → Keys).
    #[serde(default, deserialize_with = "nullable")]
    pub seal: String,
    /// The publishing device's Ed25519 signature over the record, hex, and
    /// that device's id: stored and returned as they came, and checked by
    /// every device against its verified device list.
    #[serde(default, deserialize_with = "nullable")]
    pub record_signature: String,
    #[serde(default, deserialize_with = "nullable")]
    pub signer: String,
    #[serde(default, deserialize_with = "nullable")]
    pub sealed_index: String,
    #[serde(default, deserialize_with = "nullable")]
    pub sealed_index_size: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub lease: Option<LeaseHolder>,
    #[serde(default, deserialize_with = "nullable")]
    pub created_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub updated_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub last_written_at: String,
}

/// One page of sessions, most recently written first; `next` is the id to
/// pass back as `before`, empty on the last page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SyncSessionPage {
    #[serde(default, deserialize_with = "nullable")]
    pub sessions: Vec<SyncSession>,
    #[serde(default, deserialize_with = "nullable")]
    pub next: String,
}

/// One chunk of a session's log, as the registry holds it: its place, size
/// and digest; `upload` from a declare, `url` from a listing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    #[serde(default, deserialize_with = "nullable")]
    pub index: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub slug: String,
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "nullable")]
    pub byte_size: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub checksum: String,
    #[serde(default, deserialize_with = "nullable")]
    pub upload: Option<Upload>,
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
}

/// A page of a session's ready chunks, in log order; `next` is the index to
/// pass back as `after`, absent on the last page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkPage {
    #[serde(default, deserialize_with = "nullable")]
    pub chunks: Vec<Chunk>,
    #[serde(default, deserialize_with = "nullable")]
    pub next: Option<u64>,
}

/// One ISO week of archived sessions, as the registry holds it: a durable
/// artifact of the `vintage` kind, sealed on the client (`e2e::seal_vintage`).
/// `upload` from a declare, `url` from a listing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Vintage {
    #[serde(default, deserialize_with = "nullable")]
    pub slug: String,
    #[serde(default, deserialize_with = "nullable")]
    pub week: String,
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "nullable")]
    pub byte_size: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub checksum: String,
    #[serde(default, deserialize_with = "nullable")]
    pub upload: Option<Upload>,
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VintagePage {
    #[serde(default, deserialize_with = "nullable")]
    pub vintages: Vec<Vintage>,
}

/// The most a vintage read back may be: the registry's own cap on one.
pub const MAX_VINTAGE_BYTES: u64 = 256 << 20;

/// The most a chunk read back may be: a chunk is a slice of a log, and a
/// registry listing a larger one is not handed the memory for it.
pub const MAX_CHUNK_BYTES: u64 = 64 << 20;

impl Client {
    /// The lease holder writes chunk `index` of a session's log: declared
    /// (with an Idempotency-Key, so a retry after a lost answer is the same
    /// chunk), put straight to storage, and finalized, each presenting the
    /// lease token (R-SYNC-2). `sealed` is what `e2e::ChunkSealer` made; the
    /// registry and storage see only it (R-E2E-1).
    pub fn put_chunk(&self, session: &str, index: u64, sealed: &[u8], lease_token: &str) -> Result<Chunk, Error> {
        self.put_chunk_keyed(session, index, sealed, lease_token, &crate::client::idempotency_key()?)
    }

    /// `put_chunk` under an Idempotency-Key the caller keeps: a writer that
    /// retries one sealed chunk until it lands presents the same key every
    /// time, so a declare whose answer was lost is the same chunk, not a
    /// second one the registry refuses as `chunk_exists`.
    pub fn put_chunk_keyed(&self, session: &str, index: u64, sealed: &[u8], lease_token: &str, key: &str) -> Result<Chunk, Error> {
        let checksum = crate::client::sha256_hex(sealed);
        let body = json!({ "chunk": { "index": index, "byte_size": sealed.len(), "checksum": checksum, "lease_token": lease_token } });
        let declared: Chunk = self.call_as_device("POST", &format!("/sessions/{}/chunks", slug_path(session)), Some(body), ATTEMPTS, Some(key.to_string()))?.0;
        let upload = declared.upload.as_ref().filter(|u| !u.url.is_empty()).ok_or_else(|| crate::fail("no_upload_url", "the registry declared the chunk but did not say where to put its bytes"))?;
        self.put_blob(upload, sealed)?;
        let fin = json!({ "chunk": { "lease_token": lease_token } });
        Ok(self.call_as_device("PUT", &format!("/sessions/{}/chunks/{index}/finalization", slug_path(session)), Some(fin), ATTEMPTS, None)?.0)
    }

    /// A page of a session's ready chunks from after `after`, with the URL
    /// each is read from. Signed, as every session read is: the key alone,
    /// without this machine's signing key, reads nothing.
    pub fn list_chunks(&self, session: &str, after: Option<u64>, limit: i64) -> Result<ChunkPage, Error> {
        let mut path = format!("/sessions/{}/chunks?limit={limit}", slug_path(session));
        if let Some(a) = after {
            path.push_str(&format!("&after={a}"));
        }
        self.get_as_device(&path)
    }

    /// A listed chunk's sealed bytes, checked against the digest the
    /// registry recorded before anything opens them.
    pub fn read_chunk(&self, chunk: &Chunk) -> Result<Vec<u8>, Error> {
        let bytes = self.get_blob(&chunk.url, MAX_CHUNK_BYTES)?;
        if crate::client::sha256_hex(&bytes) != chunk.checksum {
            return Err(crate::fail("checksum_mismatch", format!("chunk {} read back does not match its digest — read it again", chunk.index)));
        }
        Ok(bytes)
    }

    /// Stores a week's vintage: declared, put straight to storage and
    /// finalized. `replaces` names the week's vintage this one was merged
    /// from, and the registry refuses the write as `vintage_conflict` when
    /// that is no longer the week's — another machine replaced it first —
    /// so a vintage is never replaced by one that did not read it.
    pub fn put_vintage(&self, week: &str, sealed: &[u8], replaces: Option<&str>) -> Result<Vintage, Error> {
        let checksum = crate::client::sha256_hex(sealed);
        let body = json!({ "vintage": { "week": week, "byte_size": sealed.len(), "checksum": checksum, "replaces": replaces } });
        let key = crate::client::idempotency_key()?;
        let declared: Vintage = self.call_as_device("POST", "/vintages", Some(body), ATTEMPTS, Some(key))?.0;
        let upload = declared.upload.as_ref().filter(|u| !u.url.is_empty()).ok_or_else(|| crate::fail("no_upload_url", "the registry declared the vintage but did not say where to put its bytes"))?;
        self.put_blob(upload, sealed)?;
        Ok(self.call_as_device("PUT", &format!("/vintages/{}/finalization", slug_path(&declared.slug)), Some(json!({})), ATTEMPTS, None)?.0)
    }

    /// The week's ready vintage, if it has one.
    pub fn week_vintage(&self, week: &str) -> Result<Option<Vintage>, Error> {
        let page: VintagePage = self.get(&format!("/vintages?week={}", slug_path(week)))?;
        Ok(page.vintages.into_iter().find(|v| v.week == week))
    }

    /// A listed vintage's sealed bytes, checked against the digest the
    /// registry recorded before anything opens them.
    pub fn read_vintage(&self, v: &Vintage) -> Result<Vec<u8>, Error> {
        let bytes = self.get_blob(&v.url, MAX_VINTAGE_BYTES)?;
        if crate::client::sha256_hex(&bytes) != v.checksum {
            return Err(crate::fail("checksum_mismatch", format!("the vintage for {} read back does not match its digest — read it again", v.week)));
        }
        Ok(bytes)
    }

    /// A page of this person's device list from after `after`. Not signed: a
    /// machine recovering has no device to sign as yet. What it returns is
    /// the registry's word, and trusted only once krowk-client has verified
    /// it against the pin.
    pub fn device_list(&self, after: Option<u64>) -> Result<DeviceList, Error> {
        let devices = format!("{}/devices", self.user_path()?);
        self.get(&after.map_or_else(|| devices.clone(), |a| format!("{devices}?after={a}")))
    }

    /// Every device the person's list has named, removed ones too, with its
    /// revocation state: the same read as the list.
    pub fn listed_devices(&self) -> Result<Vec<ListedDevice>, Error> {
        Ok(self.get::<ListedDevices>(&format!("{}/devices", self.user_path()?))?.devices)
    }

    /// The whole device list, every page, from seq 0.
    pub fn device_list_all(&self) -> Result<DeviceList, Error> {
        let mut all = self.device_list(None)?;
        while let Some(after) = all.next.take() {
            let page = self.device_list(Some(after))?;
            if page.epoch != all.epoch {
                return Err(crate::fail("device_list_changed", "the device list was started over while it was read — read it again"));
            }
            all.entries.extend(page.entries);
            all.head = page.head;
            all.next = page.next;
        }
        Ok(all)
    }

    /// `krowk sync init`'s post: seq 0, signed by the first device. Once: an
    /// init that landed and is sent again is refused as `chain_exists`.
    /// A start-over (`post.start_over`) goes to `…/devices/reset`.
    pub fn init_device_list(&self, post: &ListPost) -> Result<Value, Error> {
        let path = format!("{}/devices{}", self.user_path()?, if post.start_over { "/reset" } else { "" });
        Ok(self.call_as_device("POST", &path, Some(post.body()), 1, None)?.0)
    }

    /// Appends to the device list, signed by the device that signed every
    /// entry. Once: an entry names its seq, so a post that landed and is
    /// sent again is refused as `device_list_stale` — read the list to see
    /// whether it did.
    pub fn append_device_list(&self, post: &ListPost) -> Result<Value, Error> {
        Ok(self.call_as_device("POST", &format!("{}/devices", self.user_path()?), Some(post.body()), 1, None)?.0)
    }

    /// This machine's key claims the device it speaks for, signed by it: the
    /// new machine after `join` or `recover`. Set once; the same device
    /// again is a no-op, so it is retried.
    pub fn claim_key_device(&self) -> Result<Value, Error> {
        Ok(self.call_as_device("PUT", "/key/device", Some(json!({})), ATTEMPTS, None)?.0)
    }

    /// The user key's generations, and those wrapped to this device.
    pub fn user_key(&self) -> Result<UserKeyWraps, Error> {
        let path = format!("{}/devices/{}/key", self.user_path()?, slug_path(&self.device_signer()?.device()));
        Ok(self.call_as_device("GET", &path, None, ATTEMPTS, None)?.0)
    }

    /// Opens a pairing from this device. Once: a pairing is one per person,
    /// so an open that landed and is sent again is `pairing_open`.
    pub fn open_pairing(&self) -> Result<Pairing, Error> {
        Ok(self.call_as_device("POST", &format!("{}/pairing", self.user_path()?), Some(json!({ "pairing": {} })), 1, None)?.0)
    }

    /// The one open pairing of the person this key speaks for, as a new
    /// machine finds it.
    pub fn find_open_pairing(&self) -> Result<Pairing, Error> {
        self.get(&format!("{}/pairing", self.user_path()?))
    }

    /// The person's one pairing, as either side reads it. `id` is the one
    /// the caller is in, checked against what comes back: a pairing that
    /// has since been replaced is not this one.
    pub fn show_pairing(&self, id: &str) -> Result<Pairing, Error> {
        let p: Pairing = self.get(&format!("{}/pairing", self.user_path()?))?;
        if p.id != id {
            return Err(crate::fail("pairing_gone", format!("{id} has ended — run `krowk devices add` again for a new code")));
        }
        Ok(p)
    }

    /// One step, sent once and never retried: a step sent twice is out of
    /// turn, and ends the pairing. A caller whose answer was lost reads the
    /// pairing (`show_pairing`) to see whether the step landed.
    pub fn pairing_step(&self, step: PairingStep, message: &[u8]) -> Result<Pairing, Error> {
        let (route, field) = step.route();
        let path = format!("{}/pairing/{route}", self.user_path()?);
        let body = Some(json!({ "pairing": { field: crate::client::hex(message) } }));
        Ok(if step.signed() { self.call_as_device("PUT", &path, body, 1, None)? } else { self.call("PUT", &path, body, 1, None)? }.0)
    }

    /// Ends a pairing for good: a check that failed, a no, a ^C.
    pub fn end_pairing(&self) -> Result<(), Error> {
        let url = format!("{}{}/pairing", self.base_url, self.user_path()?);
        self.request_raw("DELETE", &url, None, 1, None).map(|_| ())
    }

    pub fn list_sync_sessions(&self, before: &str, limit: i64) -> Result<SyncSessionPage, Error> {
        self.get_as_device(&crate::client::paged("/sessions", before, limit))
    }

    pub fn show_sync_session(&self, id: &str) -> Result<SyncSession, Error> {
        self.get_as_device(&format!("/sessions/{}", slug_path(id)))
    }

    /// Creates the session under its own id, or writes its sealed index.
    /// `record` is the publisher's signature over the record and its device
    /// id, hex (`krowk_client::session_record`), sent with the create; the
    /// registry keeps them as they are, and every device that opens the
    /// session checks them. `sealed_index` None leaves the stored one as it
    /// is; `lease_token` is the holder's, and a write after the first needs
    /// it.
    pub fn put_sync_session(&self, id: &str, wrapped_key: &str, record: Option<(&str, &str)>, sealed_index: Option<&str>, lease_token: Option<&str>) -> Result<SyncSession, Error> {
        let mut session = json!({ "wrapped_key": wrapped_key });
        if let Some((signature, signer)) = record {
            session["record_signature"] = json!(signature);
            session["signer"] = json!(signer);
        }
        if let Some(index) = sealed_index {
            session["sealed_index"] = json!(index);
        }
        if let Some(token) = lease_token {
            session["lease_token"] = json!(token);
        }
        Ok(self.call_as_device("PUT", &format!("/sessions/{}", slug_path(id)), Some(json!({ "session": session })), ATTEMPTS, None)?.0)
    }

    /// Takes a lease nobody holds. Once: acquiring moves the fence on, so an
    /// acquire retried after a lost response would be refused as held — by
    /// this device, under the fence it never heard.
    /// `env` is the relay env its host ticket is for (`crate::relay_env`).
    pub fn acquire_lease(&self, id: &str, device: &str, ttl_seconds: u64, env: &str) -> Result<Lease, Error> {
        let body = json!({ "lease": { "device": device, "ttl": ttl_seconds, "env": env } });
        Ok(self.call_as_device("POST", &format!("/sessions/{}/lease", slug_path(id)), Some(body), 1, None)?.0)
    }

    /// The holder, by its token, keeps the lease (`device` itself) or hands it
    /// to `device`, which mints the next holder's token. Sent once: a
    /// hand-over whose answer was lost has minted a token nobody holds, and a
    /// retry would present the old one and be refused as stale.
    pub fn renew_lease(&self, id: &str, device: &str, token: &str, ttl_seconds: u64, env: &str) -> Result<Lease, Error> {
        let body = json!({ "lease": { "device": device, "token": token, "ttl": ttl_seconds, "env": env } });
        Ok(self.call_as_device("PUT", &format!("/sessions/{}/lease", slug_path(id)), Some(body), 1, None)?.0)
    }

    /// A viewer's relay ticket for `device` on session `id`, in `env`: what
    /// a device joins a relay channel with (relay.md → Tickets). Good for
    /// five minutes; ask again for the next join after that.
    pub fn relay_ticket(&self, id: &str, device: &str, env: &str) -> Result<RelayTicket, Error> {
        Ok(self.call_as_device("GET", &format!("/sessions/{}/relay_ticket?device={}&env={}", slug_path(id), slug_path(device), slug_path(env)), None, ATTEMPTS, None)?.0)
    }

    pub fn release_lease(&self, id: &str, token: &str) -> Result<(), Error> {
        let url = format!("{}/sessions/{}/lease", self.base_url, slug_path(id));
        self.request_signed("DELETE", &url, Some(json!({ "lease": { "token": token } })), ATTEMPTS, None, Some(self.device_signer()?)).map(|_| ())
    }
}
