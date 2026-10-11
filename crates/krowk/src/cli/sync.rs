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

/// What this machine is called in the workspace's device list: `--name`,
/// else KROWK_DEVICE_NAME, else its host name, which is what a person
/// recognises it by — and which the registry sees in the clear, so either
/// of the first two keeps it from going there. Control characters are
/// dropped: the name is printed on other machines' terminals.
pub(super) fn device_name(ctx: &Ctx) -> String {
    let chosen = if ctx.f.name.is_empty() { ctx.env("KROWK_DEVICE_NAME") } else { ctx.f.name.clone() };
    let name = if chosen.trim().is_empty() { host_name() } else { chosen };
    printable(&name)
}

/// `s` without control characters: what a registry sends back is printed
/// here, and an escape sequence in a name must not reach the terminal.
pub(super) fn printable(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length, and gethostname
        // writes at most that many bytes.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "krowk device".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name the registry hands back is printed here; an escape sequence in
    /// one never reaches the terminal.
    #[test]
    fn names_are_printed_without_control_characters() {
        assert_eq!(printable("laptop\x1b[31m\u{7}\n"), "laptop[31m");
        assert_eq!(printable("  work laptop "), "work laptop");
    }
}
