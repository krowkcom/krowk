//! `krowk devices`: the machines that sync this workspace's sessions, and
//! adding one (R-E2E-3; canon, engineering/crypto.md → Adding a device).
//!
//! - `list`: the devices on the person's device list, verified against the
//!   one this machine keeps, this one marked, and any revoked on the
//!   dashboard that the list still names.
//! - `add`: pairs a new machine by a short code (`pairing.rs`), the other
//!   end of its `krowk sync join`.
//!
//! - `remove NAME`: takes a device off the person's device list and rotates
//!   the user key away from it (`remove`).
//!
//! All need a key to a paid workspace (R-SYNC-1).

use super::sync::{keyed_client, printable};
use super::Ctx;
use krowk_api::{fail, Error};
use krowk_client::device_chain::{Change, Device as ChainDevice, Kind, Subject};
use krowk_client::e2e::DeviceId;
use serde_json::json;

pub(super) fn list(ctx: &mut Ctx) -> Result<(), Error> {
    use super::chain::{self, describe, Me};
    let me = Me::load(ctx)?;
    let client = keyed_client(ctx, "`krowk devices list`")?;
    let v = chain::verified(ctx, &client, &me)?;
    let listed = chain::listed_devices(ctx, &client);
    let revoked = chain::revoked_on_dashboard(&v.chain, &listed);
    let rows: Vec<_> = v
        .chain
        .devices()
        .iter()
        .map(|d| json!({ "id": d.id().to_string(), "kind": if d.kind == Kind::Recovery { "recovery" } else { "device" }, "name": printable(&d.name), "os": printable(&d.os), "this_device": d.id() == me.device.id(), "revoked": revoked.iter().any(|r| r.id() == d.id()) }))
        .collect();
    let lines: Vec<String> = v
        .chain
        .devices()
        .iter()
        .map(|d| {
            let this = if d.id() == me.device.id() { "  (this device)" } else { "" };
            let gone = if revoked.iter().any(|r| r.id() == d.id()) { "  (revoked on the dashboard — `krowk devices remove` finishes it)" } else { "" };
            format!("{}{this}{gone}", describe(d, &v.entries))
        })
        .collect();
    let summary = format!("{}\n{} on your device list, user key generation {}", lines.join("\n"), rows.len(), v.chain.generation());
    chain::say(ctx, json!({ "devices": rows, "generation": v.chain.generation() }), summary)
}

/// `krowk devices remove NAME` (canon, engineering/devices.md → Removing a
/// device): the device is taken off the person's list and the user key
/// rotated away from it, in one post. Before anything is asked of the
/// person, the prompt names every device the new key goes to; then a fresh
/// sign-in, then the removal entry signed by this device, generation g+1
/// wrapped to every device left and the kit, and g wrapped under g+1. The
/// other devices take the new generation at their next sync, verifying it
/// against their pins.
///
/// A device revoked on the dashboard is refused by the registry already,
/// but only a removal takes it off the list: until then a rotation would
/// wrap to it, so every such device is removed in the same post.
pub(super) fn remove(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    use super::chain::{self, need_person, Me};
    need_person(ctx, "`krowk devices remove` takes a device off your list and rotates your key", false)?;
    let wanted = args.first().map(|a| a.trim()).filter(|a| !a.is_empty()).ok_or_else(|| fail("missing_argument", "name the device to remove: `krowk devices remove NAME` — `krowk sync status` lists them"))?.to_string();
    let me = Me::load(ctx)?;
    let client = keyed_client(ctx, "`krowk devices remove`")?;
    let v = chain::verified(ctx, &client, &me)?;
    let target = v.chain.devices().iter().find(|d| krowk_client::device_chain::same_name(&d.name, &wanted)).ok_or_else(|| {
        let names: Vec<String> = v.chain.devices().iter().filter(|d| d.kind == Kind::Device).map(|d| format!("'{}'", printable(&d.name))).collect();
        fail("no_such_device", format!("no device on your list is called '{}' — it has {}", printable(&wanted), names.join(", ")))
    })?;
    if target.kind == Kind::Recovery {
        return Err(fail("recovery_not_removable", "the recovery kit can't be removed, only replaced — `krowk sync recovery new`"));
    }
    if target.id() == me.device.id() {
        return Err(fail("removing_this_device", "that is the device you're on — remove it from another of your devices, or with the recovery kit (`krowk sync recover` on a new machine)"));
    }
    let listed = chain::listed_devices(ctx, &client);
    let targets = removals(target, &chain::revoked_on_dashboard(&v.chain, &listed), me.device.id());
    finish(ctx, &me, &v, &targets)
}

/// The devices a removal of `target` takes off: it, and every other device
/// revoked on the dashboard and still on the list, but never this one.
fn removals<'a>(target: &'a ChainDevice, revoked: &[&'a ChainDevice], me: DeviceId) -> Vec<&'a ChainDevice> {
    let mut out = vec![target];
    out.extend(revoked.iter().copied().filter(|d| d.id() != target.id() && d.id() != me));
    out
}

/// Asks, signs in again, and posts the removal of `targets` with the key
/// rotated to everything else on the list. `krowk sync status` comes here
/// too, to finish a dashboard Revoke.
pub(super) fn finish(ctx: &mut Ctx, me: &super::chain::Me, v: &super::chain::Verified, targets: &[&ChainDevice]) -> Result<(), Error> {
    use super::chain::{self, ask, describe, fresh_sign_in, now, post_of};
    let gone = |d: &ChainDevice| targets.iter().any(|t| t.id() == d.id());
    let names = targets.iter().map(|t| format!("'{}'", printable(&t.name))).collect::<Vec<_>>().join(" and ");
    let kept: Vec<String> = v.chain.devices().iter().filter(|d| !gone(d)).map(|d| describe(d, &v.entries)).collect();
    let _ = writeln!(ctx.io.stderr, "Remove {names}? {} refused at once, and your key is rotated to:\n  {}", if targets.len() == 1 { "It is" } else { "They are" }, kept.join(", "));
    if targets.len() > 1 {
        let _ = writeln!(ctx.io.stderr, "(Every device revoked on the dashboard is removed with it: a new key must not reach one.)");
    }
    if !ask(ctx, "Remove?")? {
        return Err(fail("selection_cancelled", "not confirmed, so nothing was removed"));
    }
    let fresh = fresh_sign_in(ctx, "Removing a device changes which devices can read your sessions", krowk_api::LoginAction::RemoveDevice)?;
    // The new key speaks for this device, as the one it replaces did.
    chain::claim(&fresh, me)?;
    let keys = chain::held_keys(ctx)?.ok_or_else(chain::not_set_up)?;
    let changes = targets.iter().map(|d| Change::Remove(Subject::of(d))).collect();
    let (next, batch) = v.chain.batch(keys.newest(), changes, me.device.id(), &me.signing, now()).map_err(|e| fail("sync_setup_failed", e.0))?;
    me.sign(&fresh).append_device_list(&post_of(&batch)).map_err(|e| match e.code().as_str() {
        "device_list_stale" => fail("device_list_stale", "your device list changed while this ran — nothing was removed; run `krowk devices remove` again"),
        _ => e,
    })?;
    chain::keep(ctx, &v.entries, &next, &batch, me, chain::older_of(&keys))?;
    let summary = format!("Removed {names}. New sessions are sealed under key generation {}; what a removed device already copied, it may keep", next.generation());
    let data = json!({ "removed": targets.iter().map(|t| json!({ "id": t.id().to_string(), "name": printable(&t.name) })).collect::<Vec<_>>(), "generation": next.generation() });
    chain::say(ctx, data, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use krowk_client::device_chain::Chain;
    use krowk_client::e2e::{DeviceKey, SigningKey};

    /// D7: removing one device also removes every other device revoked on
    /// the dashboard, so no rotation wraps the new key to one — and never
    /// this device, nor the named one twice.
    #[test]
    fn d7_a_removal_sweeps_in_the_dashboard_revoked_devices() {
        let subject = |name: &str| Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: DeviceKey::generate().public(), signing: SigningKey::generate().public() };
        let (me, key) = (subject("laptop"), SigningKey::generate());
        let me = Subject { signing: key.public(), ..me };
        let (chain, batch) = Chain::start(me.clone(), &key, None, 1_000).unwrap();
        let (a, b) = (subject("old"), subject("stolen"));
        let (chain, _) = chain.batch(&batch.newest, vec![Change::Add(a), Change::Add(b)], me.id(), &key, 1_001).unwrap();
        let find = |n: &str| chain.devices().iter().find(|d| d.name == n).unwrap();
        let (old, stolen, mine) = (find("old"), find("stolen"), find("laptop"));
        let got: Vec<_> = removals(old, &[stolen, old, mine], me.id()).into_iter().map(|d| d.name.clone()).collect();
        assert_eq!(got, ["old", "stolen"]);
        // And the batch that carries them rotates once per removal, wrapped
        // only to what is left.
        let (next, posted) = chain.batch(&batch.newest, vec![Change::Remove(Subject::of(old)), Change::Remove(Subject::of(stolen))], me.id(), &key, 1_002).unwrap();
        assert_eq!(next.generation(), 3);
        assert_eq!(posted.links.len(), 2);
        assert_eq!(posted.wraps.iter().map(|(d, _)| *d).collect::<Vec<_>>(), [me.id()]);
    }
}
