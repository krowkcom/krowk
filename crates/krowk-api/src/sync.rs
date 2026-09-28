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

/// A session's lease: its one writer, until when, and the fence every write
/// it makes names (R-SYNC-2).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    #[serde(default, deserialize_with = "nullable")]
    pub session: String,
    #[serde(default, deserialize_with = "nullable")]
    pub device: String,
    #[serde(default, deserialize_with = "nullable")]
    pub fence: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: String,
}

/// A synced session as the registry holds it: ciphertext, sizes, times and
/// the lease.
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
    pub fence: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub lease: Option<Lease>,
    #[serde(default, deserialize_with = "nullable")]
    pub created_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub updated_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub last_written_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SyncSessions {
    #[serde(default, deserialize_with = "nullable")]
    sessions: Vec<SyncSession>,
}

impl Client {
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
    /// `device`, this machine.
    pub fn approve_device(&self, slug: &str, device: &str, account_key_id: &str, wrapped_account_key: &str) -> Result<DeviceApproval, Error> {
        let body = json!({ "approval": { "device": device, "account_key_id": account_key_id, "wrapped_account_key": wrapped_account_key } });
        Ok(self.call("PUT", &format!("/device_approvals/{}/approval", slug_path(slug)), Some(body), ATTEMPTS, None)?.0)
    }

    pub fn list_sync_sessions(&self) -> Result<Vec<SyncSession>, Error> {
        Ok(self.get::<SyncSessions>("/sessions")?.sessions)
    }

    pub fn show_sync_session(&self, id: &str) -> Result<SyncSession, Error> {
        self.get(&format!("/sessions/{}", slug_path(id)))
    }

    /// Creates the session under its own id, or writes its sealed index;
    /// `fence` is the lease the writer holds, and a write after the first
    /// needs one.
    pub fn put_sync_session(&self, id: &str, wrapped_key: &str, sealed_index: &str, fence: Option<u64>) -> Result<SyncSession, Error> {
        let mut session = json!({ "wrapped_key": wrapped_key, "sealed_index": sealed_index });
        if let Some(fence) = fence {
            session["fence"] = json!(fence);
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

    /// The holder keeps the lease (`device` itself) or hands it to `device`.
    pub fn renew_lease(&self, id: &str, device: &str, fence: u64, ttl_seconds: u64) -> Result<Lease, Error> {
        let body = json!({ "lease": { "device": device, "fence": fence, "ttl": ttl_seconds } });
        Ok(self.call("PUT", &format!("/sessions/{}/lease", slug_path(id)), Some(body), ATTEMPTS, None)?.0)
    }

    pub fn release_lease(&self, id: &str, fence: u64) -> Result<(), Error> {
        let url = format!("{}/sessions/{}/lease", self.base_url, slug_path(id));
        self.request_raw("DELETE", &url, Some(json!({ "lease": { "fence": fence } })), ATTEMPTS, None).map(|_| ())
    }
}
