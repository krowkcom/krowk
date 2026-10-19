//! What the sync commands share: this machine's keystore, its name on the
//! device list, and the registry client they call. Setting sync up,
//! checking it and getting back in are `recovery`'s, pairing is
//! `pairing`'s, and the device list's plumbing is `chain`'s.

use super::Ctx;
use krowk_api::{fail, Client, Error};
use krowk_client::keystore::Keystore;

pub(super) fn keystore(ctx: &Ctx) -> Result<Keystore, Error> {
    Ok(Keystore::new(&krowk_api::home::dir(ctx.io.env)?))
}

/// The registry client vintages are written and read with, signed as this
/// device, and the keys they are sealed and opened with: the person's user
/// keys, as the device list — read from the registry, verified and brought
/// up to date first, as before anything else is sealed — leaves them, and
/// the generation it names current. A machine that has not set up sync has no vintages to write or read.
#[cfg(unix)]
pub(super) fn vintage_keys(ctx: &Ctx) -> Result<(Client, krowk_harness::vintage::Keys), Error> {
    let ks = keystore(ctx)?;
    let not_set_up = || fail("not_set_up", "archived sessions are sealed under your user key, which this machine does not hold yet — set sync up with `krowk sync init`, or add this machine from one that syncs with `krowk devices add` and `krowk sync join`");
    let device = ks.device().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(not_set_up)?.id();
    // The list first: a vintage is sealed only under the generation the
    // verified list leaves current, never under one a removed device holds.
    super::chain::before_sealing(ctx, &keyed_client(ctx, "archiving sessions")?, &super::chain::Me::load(ctx)?)?;
    let user = ks.user_keys().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(not_set_up)?;
    let chain = ks.device_list().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(not_set_up)?;
    let user = user.verified_by(&chain).map_err(|e| fail("keys_unreadable", e.0))?;
    let signing = ks.signing_key().map_err(|e| fail("keys_unreadable", e))?;
    let key = krowk_client::e2e::SigningKey::from_secret(&*signing.secret_bytes()).map_err(|e| fail("keys_unreadable", e.to_string()))?;
    let client = keyed_client(ctx, "archiving sessions")?.signed_by(krowk_client::e2e::DeviceSigner::new(device, key).shared());
    Ok((client, krowk_harness::vintage::Keys { user, generation: chain.generation() }))
}

/// The registry client for sync calls, which all need a key to a workspace.
pub(super) fn keyed_client(ctx: &Ctx, what: &str) -> Result<Client, Error> {
    let client = super::agent::new_client(ctx)?;
    if !client.authenticated() {
        return Err(fail("not_authenticated", format!("{what} needs an API key to the workspace this machine syncs — run `krowk login`, or set KROWK_TOKEN")));
    }
    Ok(client)
}

/// What this machine is called in the workspace's device list: the one
/// rule a browser login names it by too (`machine::name`).
pub(super) fn device_name(ctx: &Ctx) -> String {
    super::machine::name(ctx)
}

pub(super) use super::machine::printable;
