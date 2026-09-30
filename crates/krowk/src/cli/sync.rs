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
