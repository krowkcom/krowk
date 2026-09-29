//! Who the reference relay trusts: devices by their signing keys and
//! workspaces, and each session's workspace and lease. The hosted relay
//! (ticket 18) reads the same facts from the registry — a device record's
//! `signing_key`, a session's lease holder and fence — so this file is
//! the registry's shape, not a second model of it:
//!
//! ```json
//! {"devices": [{"id": "<32 hex>", "signingKey": "<64 hex>", "workspace": "ws_…", "revoked": false}],
//!  "sessions": [{"id": "<uuid>", "workspace": "ws_…", "holder": "<device id>", "fence": 3}]}
//! ```

use krowk_client::e2e::{self, DeviceId, SigningPublic};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Keyring {
    devices: HashMap<[u8; 16], Device>,
    sessions: HashMap<[u8; 16], Session>,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub signing: SigningPublic,
    pub workspace: String,
    pub revoked: bool,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub workspace: String,
    pub holder: [u8; 16],
    /// The lease's fence: it moves on whenever the lease changes hands.
    pub fence: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    #[serde(default)]
    devices: Vec<DeviceEntry>,
    #[serde(default)]
    sessions: Vec<SessionEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceEntry {
    id: String,
    signing_key: String,
    workspace: String,
    #[serde(default)]
    revoked: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionEntry {
    id: String,
    workspace: String,
    holder: String,
    fence: u64,
}

impl Keyring {
    /// Reads the JSON above; any entry krowk cannot read is refused by
    /// name, rather than a device silently left out.
    pub fn parse(text: &str) -> Result<Keyring, String> {
        let f: File = serde_json::from_str(text).map_err(|e| format!("the keyring is not the JSON relay.md lays out: {e}"))?;
        let mut ring = Keyring::default();
        for d in f.devices {
            let id = DeviceId::parse(&d.id).ok_or_else(|| format!("device {:?} is not a device id (32 hex characters)", d.id))?;
            let key: [u8; 32] = e2e::unhex(&d.signing_key).and_then(|k| k.try_into().ok()).ok_or_else(|| format!("device {}'s signingKey is not 64 hex characters", d.id))?;
            ring.devices.insert(id.0, Device { signing: SigningPublic(key), workspace: d.workspace, revoked: d.revoked });
        }
        for s in f.sessions {
            let id = super::parse_uuid(&s.id).ok_or_else(|| format!("session {:?} is not a UUID", s.id))?;
            let holder = DeviceId::parse(&s.holder).ok_or_else(|| format!("session {}'s holder is not a device id", s.id))?;
            ring.sessions.insert(id, Session { workspace: s.workspace, holder: holder.0, fence: s.fence });
        }
        Ok(ring)
    }

    pub fn device(&self, id: &DeviceId) -> Option<&Device> {
        self.devices.get(&id.0)
    }

    pub fn session(&self, id: &[u8; 16]) -> Option<&Session> {
        self.sessions.get(id)
    }
}
