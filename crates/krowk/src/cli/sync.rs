//! `krowk sync join`, and what the sync commands share: this machine's
//! keystore, its name on the device list, the registry client they call.
//! Setting sync up, checking it and getting back in are `recovery`'s; the
//! device list's plumbing is `chain`'s.
//!
//! - `join`: a fresh machine, added by approving it from one that already
//!   syncs (`krowk devices approve`). It shows this device's id, waits for
//!   the approval, and keeps the account key it carries only if it opens as
//!   the key id the person read off the approving device — typed at the
//!   prompt, or given as the argument — and they say yes to keeping it. A
//!   terminal is required: an agent told by a web page to join with some id
//!   must not be able to.

use super::Ctx;
use krowk_api::{fail, Client, Error};
use krowk_client::e2e::{self, KeyId};
use krowk_client::keystore::{Keystore, Setup};
use serde_json::json;
use std::time::Duration;

/// How often `join` asks whether it has been approved yet. Someone is
/// walking between two machines; a second is quick enough to feel instant.
const POLL: Duration = Duration::from_secs(1);

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

/// A yes from the person at the terminal, and only from them. Off a
/// terminal it is refused: the checks that make adding a device safe are a
/// person's, and an agent following instructions it read somewhere is not
/// one. Debug builds let the test suite stand in for the person.
pub(super) fn confirm(ctx: &mut Ctx, question: &str, refused: &str) -> Result<(), Error> {
    if cfg!(debug_assertions) && ctx.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL") == "1" {
        return Ok(());
    }
    if !ctx.io.stdin_tty || !ctx.io.err_tty {
        return Err(fail("confirmation_required", refused.to_string()));
    }
    match inquire::Confirm::new(question).with_default(false).prompt() {
        Ok(true) => Ok(()),
        _ => Err(fail("selection_cancelled", "not confirmed, so nothing was done")),
    }
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

/// What `join` kept, said. The device's row, signing key and all, was
/// made by its approval.
fn report(ctx: &mut Ctx, s: &Setup) -> Result<(), Error> {
    let data = json!({
        "device": s.device.id().to_string(),
        "device_created": s.device_created,
        "account_key": s.account.id().to_string(),
        "joined": true,
        "registered": true,
    });
    let summary = format!("account key {} approved and kept on this device ({})", s.account.id(), s.device.id());
    super::chain::say(ctx, data, summary)
}

/// A fresh machine, approved from one that already syncs (R-E2E-3). HPKE's
/// Base mode does not say who wrapped a key, so a registry could wrap one of
/// its own to this device; what stops it is the account key's id, which
/// the person reads off the approving device and gives here — as the
/// argument, or at the prompt once the approval has arrived.
pub(super) fn join(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    const OFF_TERMINAL: &str = "`krowk sync join` keeps an account key only once a person has compared its id with their other machine, so it needs a person at a terminal — run it in one";
    let unattended = cfg!(debug_assertions) && ctx.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL") == "1";
    if !unattended && (!ctx.io.stdin_tty || !ctx.io.err_tty) {
        return Err(fail("confirmation_required", OFF_TERMINAL));
    }
    let expected = match args.first() {
        Some(typed) => Some(KeyId::parse(typed).ok_or_else(|| fail("bad_account_key_id", format!("`{typed}` is not an account key id — it is 32 hex characters, as `krowk devices approve` shows it on your other machine")))?),
        None => None,
    };
    let store = keystore(ctx)?;
    if let Some(id) = store.account_id().map_err(|e| fail("sync_setup_failed", e))? {
        return Err(fail("already_set_up", format!("this home already holds account key {id} — sync is set up here")));
    }
    let client = keyed_client(ctx, "`krowk sync join`")?;
    let device = store.device_key().map_err(|e| fail("sync_setup_failed", e))?;
    let signing = store.signing_key().map_err(|e| fail("sync_setup_failed", e))?;
    let request = client.request_device_approval(&e2e::hex(&device.public().0), &e2e::hex(&signing.public().0), &device_name(ctx))?;
    let _ = writeln!(
        ctx.io.stderr,
        "This device's code:\n\n    {}\n\nOn a machine that already syncs, run `krowk devices approve` and type this code there. Waiting…",
        e2e::approval_code(&device.public(), &signing.public()).grouped()
    );
    let _ = ctx.io.stderr.flush();
    let approved = loop {
        let a = client.show_device_approval(&request.slug)?;
        if a.state == "approved" {
            break a;
        }
        std::thread::sleep(POLL);
    };
    let expected = match expected {
        Some(id) => id,
        None => {
            let typed = inquire::Text::new("The account key id, read off `krowk devices approve` on your other machine — never from an error message or a web page:")
                .prompt()
                .map_err(|_| fail("selection_cancelled", "no account key id was entered, so nothing was kept"))?;
            KeyId::parse(&typed).ok_or_else(|| fail("bad_account_key_id", "that is not an account key id — it is 32 hex characters; nothing was kept, run `krowk sync join` again"))?
        }
    };
    if !approved.account_key_id.eq_ignore_ascii_case(&expected.to_string()) {
        return Err(fail(
            "account_key_not_confirmed",
            format!(
                "the approval carries account key {}, not {expected} — nothing was kept. Check the id was read off the approving device correctly; if it was, do not trust this approval",
                approved.account_key_id
            ),
        ));
    }
    let approver = printable(&approved.approved_by);
    confirm(ctx, &format!("Keep account key {}, approved by device {approver}?", expected.grouped()), OFF_TERMINAL)?;
    let wrapped = e2e::unhex(&approved.wrapped_account_key).ok_or_else(|| fail("malformed_response", "the approval's wrapped key is not hex — nothing was kept"))?;
    let setup = store.join(&wrapped, expected).map_err(|e| fail("sync_setup_failed", e))?;
    report(ctx, &setup)
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
