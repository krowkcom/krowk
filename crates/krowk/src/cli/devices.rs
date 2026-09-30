//! `krowk devices`: the machines that sync this workspace's sessions, and
//! adding one (R-E2E-3; canon, engineering/crypto.md → Adding a device).
//!
//! - `list`: the workspace's devices, this one marked, and the account key
//!   id this machine holds — what a new device's `krowk sync join` checks.
//! - `add`: pairs a new machine by a short code (`pairing.rs`), the other
//!   end of its `krowk sync join`.
//!
//! Both need a key to a paid workspace (R-SYNC-1).

use super::sync::{keyed_client, keystore, printable};
use super::Ctx;
use crate::output::Format;
use krowk_api::{fail, Error};
use serde_json::json;

pub(super) fn list(ctx: &mut Ctx) -> Result<(), Error> {
    let store = keystore(ctx)?;
    let mine = store.device().map_err(|e| fail("sync_setup_failed", e))?.map(|d| d.id().to_string());
    let account = store.account_id().map_err(|e| fail("sync_setup_failed", e))?;
    let client = keyed_client(ctx, "`krowk devices list`")?;
    let devices = client.list_devices()?;
    let rows: Vec<_> = devices
        .iter()
        .map(|d| json!({ "id": d.id, "name": printable(&d.name), "this_device": mine.as_deref() == Some(d.id.as_str()), "created_at": d.created_at, "last_seen_at": d.last_seen_at, "revoked_at": if d.revoked_at.is_empty() { None } else { Some(&d.revoked_at) } }))
        .collect();
    let summary = match &account {
        Some(id) => format!("{} devices; this machine holds account key {}", devices.len(), id.grouped()),
        None => format!("{} devices; this machine holds no account key — `krowk sync join` adds it", devices.len()),
    };
    if ctx.format == Format::Human {
        for d in &devices {
            let this = if mine.as_deref() == Some(d.id.as_str()) { "  (this device)" } else { "" };
            let _ = writeln!(ctx.io.stdout, "{}  {}{this}", d.id, printable(&d.name));
        }
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    let data = json!({ "devices": rows, "account_key": account.map(|id| id.to_string()) });
    super::sessions::emit_data(ctx, data, summary)
}
