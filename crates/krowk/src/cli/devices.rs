//! `krowk devices`: the machines that sync this workspace's sessions, and
//! adding one (R-E2E-3; canon, engineering/crypto.md → Adding a device).
//!
//! - `list`: the workspace's devices, this one marked, and the account key
//!   id this machine holds — what a new device's `krowk sync join` checks.
//! - `approve [CODE]`: answers a new device's `krowk sync join`. The code is
//!   the id the new device shows; this machine computes each pending
//!   request's id from the public key the registry hands it and wraps the
//!   account key only to the one whose id matches. A registry that swapped
//!   in a key of its own would have to show a different code, and the
//!   person comparing is what catches it.
//!
//! Both need a key to a paid workspace (R-SYNC-1).

use super::sync::{device_name, keyed_client, keystore};
use super::Ctx;
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_client::e2e::{self, DeviceId, DevicePublic};
use serde_json::json;

fn public_key(hex: &str) -> Option<DevicePublic> {
    e2e::unhex(hex).and_then(|b| <[u8; 32]>::try_from(b).ok()).map(DevicePublic)
}

pub(super) fn list(ctx: &mut Ctx) -> Result<(), Error> {
    let store = keystore(ctx)?;
    let mine = store.device().map_err(|e| fail("sync_setup_failed", e))?.map(|d| d.id().to_string());
    let account = store.account_id().map_err(|e| fail("sync_setup_failed", e))?;
    let client = keyed_client(ctx, "`krowk devices list`")?;
    let devices = client.list_devices()?;
    let rows: Vec<_> = devices
        .iter()
        .map(|d| json!({ "id": d.id, "name": d.name, "this_device": mine.as_deref() == Some(d.id.as_str()), "created_at": d.created_at, "last_seen_at": d.last_seen_at, "revoked_at": if d.revoked_at.is_empty() { None } else { Some(&d.revoked_at) } }))
        .collect();
    let summary = match &account {
        Some(id) => format!("{} devices; this machine holds account key {}", devices.len(), id.grouped()),
        None => format!("{} devices; this machine holds no account key — `krowk sync join` adds it", devices.len()),
    };
    if ctx.format == Format::Human {
        for d in &devices {
            let this = if mine.as_deref() == Some(d.id.as_str()) { "  (this device)" } else { "" };
            let _ = writeln!(ctx.io.stdout, "{}  {}{this}", d.id, d.name);
        }
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    let data = json!({ "devices": rows, "account_key": account.map(|id| id.to_string()) });
    super::sessions::emit_data(ctx, data, summary)
}

pub(super) fn approve(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let typed = match args.first() {
        Some(t) => Some(DeviceId::parse(t).ok_or_else(|| fail("bad_device_code", format!("`{t}` is not a device code — it is the 32 hex characters `krowk sync join` shows on the new device")))?),
        None if ctx.io.stdin_tty => None,
        None => return Err(fail("confirmation_required", "pass the code the new device shows: `krowk devices approve <code>`, or run it at a terminal")),
    };
    let store = keystore(ctx)?;
    let device = store.device().map_err(|e| fail("sync_setup_failed", e))?;
    let account = store.account().map_err(|e| fail("sync_setup_failed", e))?;
    let (Some(device), Some(account)) = (device, account) else {
        return Err(fail("no_account_key", "this machine holds no account key to approve with — set sync up here first: `krowk sync init`, `krowk sync recover` or `krowk sync join`"));
    };
    let client = keyed_client(ctx, "`krowk devices approve`")?;
    // Registered first, so the registry knows the device the answer is
    // from; the same key again is the same row.
    client.register_device(&e2e::hex(&device.public().0), &device_name(), &account.id().to_string())?;
    let pending = client.list_device_approvals()?;
    // Each request's id computed here, from the key this machine would wrap
    // to, never taken from the registry's own `id`.
    let candidates: Vec<_> = pending.iter().filter_map(|a| public_key(&a.public_key).map(|k| (k.id(), k, a))).collect();
    if candidates.is_empty() {
        return Err(fail("no_pending_devices", "no device is waiting to be approved — run `krowk sync join` on the new machine first, with a key to this workspace"));
    }
    let code = match typed {
        Some(code) => code,
        None => {
            // Names only: the code is typed from the new device's screen,
            // not picked from a list this registry supplied.
            let _ = writeln!(ctx.io.stderr, "Waiting to be approved:");
            for (_, _, a) in &candidates {
                let _ = writeln!(ctx.io.stderr, "  {}  (asked at {})", a.name, a.created_at);
            }
            let t = inquire::Text::new("Type the code the new device shows:").prompt().map_err(|_| fail("selection_cancelled", "no code was entered and nothing was approved"))?;
            DeviceId::parse(&t).ok_or_else(|| fail("bad_device_code", "that is not a device code — it is 32 hex characters; nothing was approved"))?
        }
    };
    let Some((id, key, request)) = candidates.into_iter().find(|(id, _, _)| *id == code) else {
        return Err(fail(
            "no_such_device_code",
            format!("no waiting device has code {} — check it against the new device's screen; nothing was approved", code.grouped()),
        ));
    };
    let wrapped = e2e::wrap_account_key(&account, &key).map_err(|e| fail("sync_setup_failed", e.0))?;
    client.approve_device(&request.slug, &device.id().to_string(), &account.id().to_string(), &e2e::hex(&wrapped))?;
    let summary = format!(
        "approved {} ({}) — on it, `krowk sync join` asks for account key id {}",
        request.name,
        id.grouped(),
        account.id().grouped()
    );
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    let data = json!({ "device": id.to_string(), "name": request.name, "account_key": account.id().to_string() });
    super::sessions::emit_data(ctx, data, summary)
}
