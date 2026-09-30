//! `krowk devices`: the machines that sync this workspace's sessions, and
//! adding one (R-E2E-3; canon, engineering/crypto.md → Adding a device).
//!
//! - `list`: the workspace's devices, this one marked, and the account key
//!   id this machine holds — what a new device's `krowk sync join` checks.
//! - `approve [CODE]`: answers a new device's `krowk sync join`. The code is
//!   the id the new device shows; this machine computes each pending
//!   request's id from the public key the registry hands it and wraps the
//!   account key only to the one whose id matches, once the person has said
//!   yes to its name and id. A registry that swapped in a key of its own
//!   would have to show a different code, and the person comparing is what
//!   catches it. A terminal is required, code given or not: a prompt
//!   injection that has an agent run `krowk devices approve <code>` must not
//!   hand the attacker's machine every session.
//!
//! Both need a key to a paid workspace (R-SYNC-1).

use super::sync::{confirm, device_name, keyed_client, keystore, printable};
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

const OFF_TERMINAL: &str = "`krowk devices approve` hands this workspace's account key to another machine, so it needs a person at a terminal to compare the new device's code and say yes — run it in one";

pub(super) fn approve(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let unattended = cfg!(debug_assertions) && ctx.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL") == "1";
    if !unattended && (!ctx.io.stdin_tty || !ctx.io.err_tty) {
        return Err(fail("confirmation_required", OFF_TERMINAL));
    }
    // Joined, so a code pasted unquoted in its groups of four is one code;
    // parsing drops the spaces and dashes.
    let joined = args.join(" ");
    let typed = match (!joined.trim().is_empty()).then_some(joined.as_str()) {
        Some(t) => Some(DeviceId::parse(t).ok_or_else(|| fail("bad_device_code", format!("`{t}` is not a device code — it is the 32 hex characters `krowk sync join` shows on the new device")))?),
        None => None,
    };
    let store = keystore(ctx)?;
    let device = store.device().map_err(|e| fail("sync_setup_failed", e))?;
    let account = store.account().map_err(|e| fail("sync_setup_failed", e))?;
    let (Some(device), Some(account)) = (device, account) else {
        return Err(fail("no_account_key", "this machine holds no account key to approve with — set sync up here first: `krowk sync init`, `krowk sync recover` or `krowk sync join`"));
    };
    // Registered first, so the registry knows the device the answer is
    // from; the same key again is the same row. Both calls act as this
    // device, and are signed by its key.
    let signing = store.signing_key().map_err(|e| fail("sync_setup_failed", e))?;
    let signing_public = e2e::hex(&signing.public().0);
    let client = keyed_client(ctx, "`krowk devices approve`")?.signed_by(e2e::DeviceSigner::new(device.id(), signing).shared());
    client.register_device(&e2e::hex(&device.public().0), &signing_public, &device_name(ctx), &account.id().to_string())?;
    let pending = client.list_device_approvals()?;
    // Each request's code computed here, from both keys it carries — the
    // X25519 key this machine would wrap to and the signing key the approval
    // registers — never taken from the registry's own `id`.
    let candidates: Vec<_> = pending
        .iter()
        .filter_map(|a| {
            let key = public_key(&a.public_key)?;
            let signing: [u8; 32] = e2e::unhex(&a.signing_key).and_then(|b| b.try_into().ok())?;
            Some((e2e::approval_code(&key, &e2e::SigningPublic(signing)), key, a))
        })
        .collect();
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
                let _ = writeln!(ctx.io.stderr, "  {}  (asked at {})", printable(&a.name), printable(&a.created_at));
            }
            let t = inquire::Text::new("Type the code the new device shows:").prompt().map_err(|_| fail("selection_cancelled", "no code was entered and nothing was approved"))?;
            DeviceId::parse(&t).ok_or_else(|| fail("bad_device_code", "that is not a device code — it is 32 hex characters; nothing was approved"))?
        }
    };
    // Exactly one request with exactly this code, and the account key goes
    // to the X25519 key of that request: never the first of several, and
    // never matched by key alone.
    let mut matched = candidates.into_iter().filter(|(c, _, _)| *c == code);
    let (Some((id, key, request)), None) = (matched.next(), matched.next()) else {
        return Err(fail(
            "no_such_device_code",
            format!("no single waiting device has code {} — check it against the new device's screen; nothing was approved", code.grouped()),
        ));
    };
    let name = printable(&request.name);
    confirm(ctx, &format!("Give this workspace's account key to {name} ({}), asked at {}?", id.grouped(), printable(&request.created_at)), OFF_TERMINAL)?;
    let wrapped = e2e::wrap_account_key(&account, &key).map_err(|e| fail("sync_setup_failed", e.0))?;
    client.approve_device(&request.slug, &device.id().to_string(), &account.id().to_string(), &e2e::hex(&wrapped))?;
    let summary = format!(
        "approved {} ({}) — on it, `krowk sync join` asks for account key id {}",
        name,
        id.grouped(),
        account.id().grouped()
    );
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    let data = json!({ "device": key.id().to_string(), "code": id.to_string(), "name": name, "account_key": account.id().to_string() });
    super::sessions::emit_data(ctx, data, summary)
}
