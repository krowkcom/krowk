//! `krowk devices`: the machines that sync this workspace's sessions, and
//! adding one (R-E2E-3; canon, engineering/crypto.md → Adding a device).
//!
//! - `list`: the devices on the person's device list, verified against the
//!   one this machine keeps, this one marked, and any revoked on the
//!   dashboard that the list still names.
//! - `add`: pairs a new machine by a short code (`pairing.rs`), the other
//!   end of its `krowk sync join`.
//!
//! Both need a key to a paid workspace (R-SYNC-1).

use super::sync::{keyed_client, printable};
use super::Ctx;
use krowk_api::Error;
use krowk_client::device_chain::Kind;
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
