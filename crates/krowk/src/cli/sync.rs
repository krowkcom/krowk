//! `krowk sync`: this machine's end-to-end keys (R-E2E-3, R-E2E-4). The
//! keys, their files and the phrase are `krowk_client`; this is the command
//! line around them. Named now for the sync that uses them (tickets 16 and
//! 19): until then the wrapped account key is kept in this home only.
//!
//! - `init`: first sync setup. Makes this device's key when it has none and
//!   a new account key, shows the account key's recovery phrase, and keeps
//!   the key only once the phrase has been typed back — mandatory, so it
//!   needs a person at the terminal.
//! - `recover`: a fresh machine. The phrase, typed at a prompt that does not
//!   echo it (or piped in), restores the account key and wraps it to this
//!   device.
//!
//! The phrase goes to the terminal (stderr) and nowhere else: never stdout,
//! never a file, never `--json`.

use super::Ctx;
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_client::keystore::{Keystore, Setup};
use krowk_client::phrase;
use serde_json::json;

/// How many times `init` asks for the phrase back before giving up.
const TRIES: usize = 3;

fn keystore(ctx: &Ctx) -> Result<Keystore, Error> {
    Ok(Keystore::new(&krowk_api::home::dir(ctx.io.env)?))
}

fn report(ctx: &mut Ctx, s: &Setup, recovered: bool) -> Result<(), Error> {
    let data = json!({
        "device": s.device.id().to_string(),
        "device_created": s.device_created,
        "account_key": s.account.id().to_string(),
        "recovered": recovered,
    });
    let summary = match recovered {
        true => format!("account key {} restored and wrapped to this device ({})", s.account.id(), s.device.id()),
        false => format!("account key {} made and wrapped to this device ({}); keep the recovery phrase", s.account.id(), s.device.id()),
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
            // Four rows of six, numbered, so it is copied in order.
            for (row, chunk) in words.split(' ').collect::<Vec<_>>().chunks(6).enumerate() {
                let line: Vec<String> = chunk.iter().enumerate().map(|(i, w)| format!("{:>2}. {w:<9}", row * 6 + i + 1)).collect();
                let _ = writeln!(stderr, "  {}", line.join(" ").trim_end());
            }
            let _ = writeln!(stderr, "\n{}", crate::output::paint(colour, crate::output::DIM, "Write it down and keep it offline. krowk never stores it and cannot show it again."));
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
    report(ctx, &setup, false)
}

pub(super) fn recover(ctx: &mut Ctx) -> Result<(), Error> {
    let store = keystore(ctx)?;
    let words = krowk_client::Zeroizing::new(if ctx.io.stdin_tty {
        inquire::Password::new("Recovery phrase (24 words):").without_confirmation().prompt().map_err(|_| fail("selection_cancelled", "no phrase was entered and nothing was changed"))?
    } else {
        use std::io::Read;
        let mut raw = String::new();
        std::io::stdin().take(4097).read_to_string(&mut raw).map_err(|e| fail("bad_recovery_phrase", format!("the phrase could not be read from stdin: {e}")))?;
        if raw.len() > 4096 {
            return Err(fail("bad_recovery_phrase", "more than 4 KiB was piped in, which is no recovery phrase"));
        }
        raw
    });
    let key = phrase::decode(&words).map_err(|e| fail("bad_recovery_phrase", e))?;
    drop(words);
    let setup = store.recover(key).map_err(|e| fail("sync_setup_failed", e))?;
    report(ctx, &setup, true)
}
