//! Sync's records and calls (canon, engineering/crypto.md): the devices that
//! hold the account key, the mailbox a new device is approved through, and
//! sessions with their leases. Every call needs a key to a paid workspace
//! (R-SYNC-1), and nothing here carries plaintext: keys and sealed blobs are
//! hex, and a session's title and the rest of what a listing shows travel
//! inside `sealed_index`, sealed by the caller before it gets here.
//!
//! None of it takes an Idempotency-Key. A device is unique by its public key,
//! a session's id is the client's own and in the path, and a device approval
//! is the browser login's shape — a lost response costs asking again.
//!
//! This crate carries bytes and nothing else. What makes adding a device safe
//! against a hostile registry — comparing the new device's id on both
//! screens, and the account key's id on the new one — is krowk-client's and
//! the command line's to do with what these calls return.

use crate::client::{Client, slug_path};
use crate::types::Upload;
use crate::error::Error;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

const ATTEMPTS: u32 = 3;

fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A device of the workspace. `id` is the fingerprint of its public key;
/// the registry derived it, so a client that relies on it computes it again.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Device {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub public_key: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub created_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub last_seen_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub revoked_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Devices {
    #[serde(default, deserialize_with = "nullable")]
    devices: Vec<Device>,
}

/// A new device's request to be approved, and once approved the account key
/// wrapped to it with the account key's id.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceApproval {
    #[serde(default, deserialize_with = "nullable")]
    pub slug: String,
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub public_key: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub created_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub approved_by: String,
    #[serde(default, deserialize_with = "nullable")]
    pub account_key_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub wrapped_account_key: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DeviceApprovals {
    #[serde(default, deserialize_with = "nullable")]
    device_approvals: Vec<DeviceApproval>,
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

/// A synced session as the registry holds it: ciphertext, sizes, times and
/// who holds the lease. A listing leaves `sealed_index` empty; `show` has it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SyncSession {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub wrapped_key: String,
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
        let checksum = crate::client::sha256_hex(sealed);
        let body = json!({ "chunk": { "index": index, "byte_size": sealed.len(), "checksum": checksum, "lease_token": lease_token } });
        let declared: Chunk = self.call("POST", &format!("/sessions/{}/chunks", slug_path(session)), Some(body), ATTEMPTS, Some(crate::client::idempotency_key()?))?.0;
        let upload = declared.upload.as_ref().filter(|u| !u.url.is_empty()).ok_or_else(|| crate::fail("no_upload_url", "the registry declared the chunk but did not say where to put its bytes"))?;
        self.put_blob(upload, sealed)?;
        let fin = json!({ "chunk": { "lease_token": lease_token } });
        Ok(self.call("PUT", &format!("/sessions/{}/chunks/{index}/finalization", slug_path(session)), Some(fin), ATTEMPTS, None)?.0)
    }

    /// A page of a session's ready chunks from after `after`, with the URL
    /// each is read from.
    pub fn list_chunks(&self, session: &str, after: Option<u64>, limit: i64) -> Result<ChunkPage, Error> {
        let mut path = format!("/sessions/{}/chunks?limit={limit}", slug_path(session));
        if let Some(a) = after {
            path.push_str(&format!("&after={a}"));
        }
        self.get(&path)
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

    /// Says this machine holds the account key `account_key_id`. The same
    /// public key again is the same device, renamed; an account key other
    /// than the one the workspace's devices hold is `account_key_mismatch`.
    pub fn register_device(&self, public_key: &str, name: &str, account_key_id: &str) -> Result<Device, Error> {
        let body = json!({ "device": { "public_key": public_key, "name": name, "account_key_id": account_key_id } });
        Ok(self.call("POST", "/devices", Some(body), ATTEMPTS, None)?.0)
    }

    pub fn list_devices(&self) -> Result<Vec<Device>, Error> {
        Ok(self.get::<Devices>("/devices")?.devices)
    }

    /// A new device asks to be approved. Once: a retried create is a second
    /// request, and only the one whose id the person was shown is answered.
    pub fn request_device_approval(&self, public_key: &str, name: &str) -> Result<DeviceApproval, Error> {
        let body = json!({ "device_approval": { "public_key": public_key, "name": name } });
        Ok(self.call("POST", "/device_approvals", Some(body), 1, None)?.0)
    }

    /// What is waiting to be approved.
    pub fn list_device_approvals(&self) -> Result<Vec<DeviceApproval>, Error> {
        Ok(self.get::<DeviceApprovals>("/device_approvals")?.device_approvals)
    }

    pub fn show_device_approval(&self, slug: &str) -> Result<DeviceApproval, Error> {
        self.get(&format!("/device_approvals/{}", slug_path(slug)))
    }

    /// Answers a request with the account key wrapped to its public key, from
    /// `device`, this machine. Sent once: every wrap is a different blob, so
    /// a retry after a lost answer would be refused as already approved and
    /// read as a failure when it had worked.
    pub fn approve_device(&self, slug: &str, device: &str, account_key_id: &str, wrapped_account_key: &str) -> Result<DeviceApproval, Error> {
        let body = json!({ "approval": { "device": device, "account_key_id": account_key_id, "wrapped_account_key": wrapped_account_key } });
        Ok(self.call("PUT", &format!("/device_approvals/{}/approval", slug_path(slug)), Some(body), 1, None)?.0)
    }

    pub fn list_sync_sessions(&self, before: &str, limit: i64) -> Result<SyncSessionPage, Error> {
        self.get(&crate::client::paged("/sessions", before, limit))
    }

    pub fn show_sync_session(&self, id: &str) -> Result<SyncSession, Error> {
        self.get(&format!("/sessions/{}", slug_path(id)))
    }

    /// Creates the session under its own id, or writes its sealed index.
    /// `sealed_index` None leaves the stored one as it is; `lease_token` is
    /// the holder's, and a write after the first needs it.
    pub fn put_sync_session(&self, id: &str, wrapped_key: &str, sealed_index: Option<&str>, lease_token: Option<&str>) -> Result<SyncSession, Error> {
        let mut session = json!({ "wrapped_key": wrapped_key });
        if let Some(index) = sealed_index {
            session["sealed_index"] = json!(index);
        }
        if let Some(token) = lease_token {
            session["lease_token"] = json!(token);
        }
        Ok(self.call("PUT", &format!("/sessions/{}", slug_path(id)), Some(json!({ "session": session })), ATTEMPTS, None)?.0)
    }

    /// Takes a lease nobody holds. Once: acquiring moves the fence on, so an
    /// acquire retried after a lost response would be refused as held — by
    /// this device, under the fence it never heard.
    pub fn acquire_lease(&self, id: &str, device: &str, ttl_seconds: u64) -> Result<Lease, Error> {
        let body = json!({ "lease": { "device": device, "ttl": ttl_seconds } });
        Ok(self.call("POST", &format!("/sessions/{}/lease", slug_path(id)), Some(body), 1, None)?.0)
    }

    /// The holder, by its token, keeps the lease (`device` itself) or hands it
    /// to `device`, which mints the next holder's token. Sent once: a
    /// hand-over whose answer was lost has minted a token nobody holds, and a
    /// retry would present the old one and be refused as stale.
    pub fn renew_lease(&self, id: &str, device: &str, token: &str, ttl_seconds: u64) -> Result<Lease, Error> {
        let body = json!({ "lease": { "device": device, "token": token, "ttl": ttl_seconds } });
        Ok(self.call("PUT", &format!("/sessions/{}/lease", slug_path(id)), Some(body), 1, None)?.0)
    }

    pub fn release_lease(&self, id: &str, token: &str) -> Result<(), Error> {
        let url = format!("{}/sessions/{}/lease", self.base_url, slug_path(id));
        self.request_raw("DELETE", &url, Some(json!({ "lease": { "token": token } })), ATTEMPTS, None).map(|_| ())
    }
}
