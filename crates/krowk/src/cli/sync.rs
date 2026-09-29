//! `krowk sync`: this machine's end-to-end keys (R-E2E-3, R-E2E-4). The
//! keys, their files and the phrase are `krowk_client`; this is the command
//! line around them.
//!
//! - `init`: first sync setup. Makes this device's key when it has none and
//!   a new account key, shows the account key's recovery phrase, and keeps
//!   the key only once the phrase has been typed back — mandatory, so it
//!   needs a person at the terminal.
//! - `recover`: a fresh machine. The phrase, typed at a prompt that does not
//!   echo it (or piped from a file, never `echo`, which lands in the shell's
//!   history), restores the account key and wraps it to this device, and
//!   shows its key id to compare with the one `init` showed. Run again with
//!   the right phrase, it replaces a key an earlier `recover` put here.
//!
//! - `join`: a fresh machine, added by approving it from one that already
//!   syncs (`krowk devices approve`) rather than from the phrase. It shows
//!   this device's id, waits for the approval, and keeps the account key it
//!   carries only if it opens as the key id the person read off the
//!   approving device — typed at the prompt, or given as the argument — and
//!   they say yes to keeping it. A terminal is required: an agent told by a
//!   web page to join with some id must not be able to.
//! - `register`: says this machine holds its account key to the workspace,
//!   for a machine set up offline or before it had a key.
//!
//! With a key to a workspace, `init` and `recover` register this device
//! with the registry (a paid plan's, R-SYNC-1); without one, or with the
//! registry unreachable or failing, nothing leaves the machine, everything
//! local works the same, and `register` does it later.
//!
//! The phrase goes to the terminal (stderr) and nowhere else: never stdout,
//! never a file, never `--json`.

use super::Ctx;
use crate::output::Format;
use krowk_api::{fail, Client, Error};
use krowk_client::e2e::{self, KeyId};
use krowk_client::keystore::{Keystore, Setup};
use krowk_client::phrase;
use serde_json::json;
use std::time::Duration;

/// How many times `init` asks for the phrase back before giving up.
const TRIES: usize = 3;

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

/// Says this machine holds the account key, when there is a key to a
/// workspace to say it with. `None` when there is not: sync set up without
/// an account stays on the machine (R-SYNC-1).
fn register(ctx: &Ctx, s: &Setup) -> Result<Option<krowk_api::sync::Device>, Error> {
    let Ok(client) = super::agent::new_client(ctx) else { return Ok(None) };
    if !client.authenticated() {
        return Ok(None);
    }
    // A free workspace does not sync (R-SYNC-1), and a registry that cannot
    // be reached or fails on its side is not a reason for setup to fail:
    // the keys made here are complete without it. Set up, not registered,
    // the summary says so, and `krowk sync register` does it later.
    let registered = client.register_device(&e2e::hex(&s.device.public().0), &device_name(ctx), &s.account.id().to_string());
    if registered.as_ref().is_err_and(|e| e.code() == "sync_requires_paid_plan" || e.status == 0 || e.status >= 500) {
        return Ok(None);
    }
    registered.map(Some).map_err(|e| {
        let mut e = e;
        let kept = "the keys are kept on this machine all the same";
        let fix = match e.fix() {
            f if f.is_empty() => kept.to_string(),
            f => format!("{f} ({kept})"),
        };
        e.body.insert("fix".into(), json!(fix));
        e
    })
}

fn report(ctx: &mut Ctx, s: &Setup, how: &str, replaced: Option<KeyId>) -> Result<(), Error> {
    let recovered = how == "recover";
    let registered = if how == "join" { true } else { register(ctx, s)?.is_some() };
    let data = json!({
        "device": s.device.id().to_string(),
        "device_created": s.device_created,
        "account_key": s.account.id().to_string(),
        "recovered": recovered,
        "joined": how == "join",
        "registered": registered,
        "replaced": replaced.map(|id| id.to_string()),
    });
    let summary = match how {
        "join" => format!("account key {} approved and kept on this device ({})", s.account.id(), s.device.id()),
        // A mistyped word can make another valid phrase (1 in 256). The
        // registry catches it once this device registers there (it refuses
        // a key other than the workspace's); without an account the person
        // compares the id with the one `init` showed.
        "recover" => format!(
            "account key {} restored and wrapped to this device ({}){} — check it is the key id `krowk sync init` showed; if not, a word is wrong: run `krowk sync recover` again with the right phrase",
            s.account.id(),
            s.device.id(),
            replaced.map(|id| format!(", replacing {id}")).unwrap_or_default()
        ),
        _ => format!("account key {} made and wrapped to this device ({}); keep the recovery phrase", s.account.id(), s.device.id()),
    };
    let summary = match registered {
        true => summary,
        false => format!("{summary}. Not registered with a workspace — that takes a key to a Pro workspace (`krowk login`) and a registry that answers; run `krowk sync register` then. Nothing leaves this machine until it is"),
    };
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, data, summary)
}

pub(super) fn init(ctx: &mut Ctx) -> Result<(), Error> {
    if !ctx.io.stdin_tty || !ctx.io.err_tty {
        return Err(fail(
            "confirmation_required",
            "`krowk sync init` shows a recovery phrase and has it typed back, so it needs a person at a terminal — run it in one",
        ));
    }
    let store = keystore(ctx)?;
    let colour = ctx.colour;
    let stderr = &mut *ctx.io.stderr;
    let setup = store
        .init(|key| {
            let words = phrase::encode(key);
            let _ = writeln!(stderr, "Your recovery phrase — the only way back to your sessions if every device is lost:\n");
            // Four rows of six, numbered, so it is copied in order. Each
            // word goes straight to the terminal: no copy of it is built
            // here that the phrase's own wiping would miss.
            for (i, w) in words.split(' ').enumerate() {
                let (start, end) = (i % 6 == 0, i % 6 == 5);
                let _ = write!(stderr, "{}{:>2}. {w}", if start { "  " } else { "" }, i + 1);
                let _ = match end {
                    true => writeln!(stderr),
                    false => write!(stderr, "{:pad$}", "", pad = 10usize.saturating_sub(w.len())),
                };
            }
            let _ = writeln!(stderr, "\nKey id {} — `krowk sync recover` shows the same id when the phrase is right. Note it with the words.", key.id());
            let _ = writeln!(stderr, "{}", crate::output::paint(colour, crate::output::DIM, "Write it down and keep it offline. krowk never stores it and cannot show it again."));
            let _ = stderr.flush();
            for left in (0..TRIES).rev() {
                let typed = krowk_client::Zeroizing::new(
                    inquire::Password::new("Type the phrase back to confirm:").without_confirmation().prompt().map_err(|_| "nothing was confirmed, so no account key was kept".to_string())?,
                );
                match phrase::decode(&typed) {
                    Ok(k) if k == *key => return Ok(()),
                    Ok(_) => {
                        let _ = writeln!(stderr, "That is a valid phrase, but not this one.");
                    }
                    Err(why) => {
                        let _ = writeln!(stderr, "{why}.");
                    }
                }
                if left > 0 {
                    let _ = writeln!(stderr, "{left} more {}.", if left == 1 { "try" } else { "tries" });
                }
            }
            Err("the phrase was not typed back, so no account key was kept — run `krowk sync init` again for a new one".into())
        })
        .map_err(|e| fail("sync_setup_failed", e))?;
    report(ctx, &setup, "init", None)
}

pub(super) fn recover(ctx: &mut Ctx) -> Result<(), Error> {
    let store = keystore(ctx)?;
    let words = krowk_client::Zeroizing::new(if ctx.io.stdin_tty {
        inquire::Password::new("Recovery phrase (24 words):").without_confirmation().prompt().map_err(|_| fail("selection_cancelled", "no phrase was entered and nothing was changed"))?
    } else {
        use std::io::Read;
        // Room for the most it reads, so the phrase is never reallocated
        // (and an unwiped copy left behind).
        let mut raw = String::with_capacity(4097);
        std::io::stdin().take(4097).read_to_string(&mut raw).map_err(|e| fail("bad_recovery_phrase", format!("the phrase could not be read from stdin: {e}")))?;
        if raw.len() > 4096 {
            return Err(fail("bad_recovery_phrase", "more than 4 KiB was piped in, which is no recovery phrase"));
        }
        raw
    });
    let key = phrase::decode(&words).map_err(|e| fail("bad_recovery_phrase", e))?;
    drop(words);
    let (setup, replaced) = store.recover(key).map_err(|e| fail("sync_setup_failed", e))?;
    report(ctx, &setup, "recover", replaced)
}

/// `krowk sync register`: tells the workspace this machine holds its account
/// key — after an `init` or `recover` that ran offline, on a free plan since
/// upgraded, or before `krowk login`. The keys are not touched.
pub(super) fn register_now(ctx: &mut Ctx) -> Result<(), Error> {
    let store = keystore(ctx)?;
    let (Some(device), Some(account)) = (store.device().map_err(|e| fail("sync_setup_failed", e))?, store.account_id().map_err(|e| fail("sync_setup_failed", e))?) else {
        return Err(fail("no_account_key", "this machine holds no account key to register — set sync up first: `krowk sync init`, `recover` or `join`"));
    };
    let client = keyed_client(ctx, "`krowk sync register`")?;
    let d = client.register_device(&e2e::hex(&device.public().0), &device_name(ctx), &account.to_string())?;
    let summary = format!("this machine ({}) is registered as {}, holding account key {account}", device.id(), printable(&d.name));
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "device": device.id().to_string(), "name": printable(&d.name), "account_key": account.to_string(), "registered": true }), summary)
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
    let request = client.request_device_approval(&e2e::hex(&device.public().0), &device_name(ctx))?;
    let _ = writeln!(
        ctx.io.stderr,
        "This device's code:\n\n    {}\n\nOn a machine that already syncs, run `krowk devices approve` and type this code there. Waiting…",
        device.id().grouped()
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
    report(ctx, &setup, "join", None)
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
