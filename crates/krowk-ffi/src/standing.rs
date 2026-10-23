//! Whether the phone is still one of the person's devices (R-E2E-3; canon
//! devices.md → Removing a device, Starting over): the device list the
//! registry serves now, checked against the one the phone pinned, as the
//! terminal's `krowk sync status` checks it. A removed phone, or one left
//! off a list that was started over, stops syncing and says so; it never
//! adopts a new list by itself. A list that only grew is kept as the new
//! pin, and a newer user key generation taken up, so the phone goes on
//! opening what the person's hosts seal after a removal.
//!
//! What the registry can do with this: refuse the phone's key, or serve a
//! list with another first entry. Either stops the phone, and the person
//! resets it and pairs it again — the denial of service every registry
//! can already do. It can't serve a removal of the phone the phone takes
//! for real: a `remove` entry verifies only signed by a device on the
//! pinned list.

use krowk_client::device_chain::{Chain, Entry, SignedEntry};
use krowk_client::e2e;
use krowk_client::user_key::UserKeys;

use crate::KrowkError;
use crate::account::{Account, internal};

/// The phone is not on the person's device list any more, or the registry
/// no longer takes its key.
const DEVICE_REMOVED: &str = "device_removed";
/// The person's device list was started over: a new chain, under a new
/// epoch, that this phone is not on.
const DEVICE_LIST_RESET: &str = "device_list_reset";

#[uniffi::export]
impl Account {
    /// Checks this phone against the person's device list as the registry
    /// serves it now. Ok when it is still on it: the list, verified to
    /// extend the one kept, is kept as the new pin, and a newer user key
    /// generation is taken up (only as the id the list names, and only
    /// when it opens back down to the one held).
    ///
    /// Refused with:
    /// - `device_removed`: the verified list no longer names this phone
    ///   (the list, which only grew, is kept as the pin), or the registry
    ///   refuses its key (`unauthorized`,
    ///   `device_revoked`): removed with `krowk devices remove`, revoked
    ///   on the dashboard, or left behind by a start-over, which revokes
    ///   the keys of every device it leaves off;
    /// - `device_list_reset`: the list's first entry is not the one kept,
    ///   and the list verifies from its own: it was started over, under a
    ///   new chain epoch, and the detail says when, as the new list's first
    ///   entry dates it. Never adopted;
    /// - `device_list_refused`: a list that doesn't extend the one kept
    ///   (shorter, forked), one that starts elsewhere and doesn't verify,
    ///   or none at all: this phone won't trust it, and keeps its pin;
    /// - `user_key_refused` or `user_key_missing`: a newer generation that
    ///   doesn't open, or isn't wrapped to this phone yet;
    /// - `not_paired`: the phone holds no device list.
    ///
    /// A registry that can't be reached is `Network`. Blocking: a registry
    /// call per page of the list, and one more to take up a newer key;
    /// call it off the main thread.
    pub fn check_device_list(&self) -> Result<(), KrowkError> {
        let kept = self.keystore.device_list().map_err(internal)?.ok_or_else(not_paired)?;
        let list = match self.client.device_list_all() {
            Ok(list) => list,
            Err(e) if matches!(e.code().as_str(), "unauthorized" | "device_revoked") => return Err(refused(DEVICE_REMOVED, KEY_REFUSED.into())),
            Err(e) => return Err(e.into()),
        };
        let entries = decode(&list)?;
        let untrusted = |e: String| refused("device_list_refused", format!("{e} — this iPhone does not trust it, and nothing was changed"));
        let Some(first) = entries.first() else {
            return Err(untrusted("the registry served an empty device list".into()));
        };
        if first.hash() != kept.root() {
            // A list started over is a whole chain from its own first entry.
            // That proves only that it is well formed — anyone signed in as
            // the person can start one — so it is never adopted, only said.
            Chain::verify(&entries, None).map_err(|e| untrusted(format!("the registry served a device list that starts elsewhere and does not verify ({})", e.0)))?;
            return Err(refused(DEVICE_LIST_RESET, reset(first)));
        }
        // The list as kept, entry for entry (the head's hash covers every
        // entry before it): nothing to verify or write again.
        let unchanged = entries.len() as u64 == kept.head().seq + 1 && entries.last().map(SignedEntry::hash) == Some(kept.head().hash);
        let chain = if unchanged {
            kept
        } else {
            // Verified against the pin first, so a list this phone won't
            // trust is told apart from a Keychain that won't keep one.
            Chain::verify(&entries, Some(kept.head())).map_err(|e| untrusted(e.0))?;
            self.keystore.save_device_list(&entries).map_err(internal)?
        };
        if !chain.devices().iter().any(|d| d.id() == self.device) {
            return Err(refused(DEVICE_REMOVED, REMOVED.into()));
        }
        self.adopt(&chain)
    }

    /// Whether the person's device list, as this phone keeps it, holds a
    /// recovery kit: what the reminder after pairing says. False before
    /// the phone is paired. No registry call.
    pub fn has_recovery_kit(&self) -> Result<bool, KrowkError> {
        Ok(self.keystore.device_list().map_err(internal)?.is_some_and(|c| c.recovery().is_some()))
    }
}

impl Account {
    /// The user keys `chain` leaves current, held here: kept as they are
    /// when they are its generation, a newer one taken up from the
    /// registry's wrap to this device, never an older one.
    fn adopt(&self, chain: &Chain) -> Result<(), KrowkError> {
        let held = self.keystore.user_keys().map_err(internal)?.ok_or_else(not_paired)?;
        let (have, named) = (held.newest().generation(), chain.generation());
        if have == named {
            return held.verified_by(chain).map(|_| ()).map_err(key_refused);
        }
        if have > named {
            // Another check took a newer list up meanwhile: the list kept now
            // names the generation held.
            if self.keystore.device_list().map_err(internal)?.is_some_and(|c| c.generation() >= have) {
                return Ok(());
            }
            return Err(refused("user_key_refused", format!("this iPhone holds user key generation {have}, newer than your device list names ({named}) — it will not go back")));
        }
        let wraps = self.client.user_key()?;
        let blob = wraps.wraps.iter().find(|w| w.generation == named).and_then(|w| e2e::unhex(&w.wrapped_key)).ok_or_else(|| {
            refused("user_key_missing", format!("the registry holds no user key generation {named} wrapped to this iPhone yet — it keeps the key it holds and asks again later"))
        })?;
        let links = wraps.generations.iter().filter(|g| g.generation > 1).filter_map(|g| e2e::unhex(&g.wrapped_previous));
        let device = self.keystore.device_key().map_err(internal)?;
        let keys = UserKeys::adopt(held.newest(), &blob, named, chain.key_id(), &device, links).and_then(|k| k.verified_by(chain)).map_err(key_refused)?;
        self.keystore.save_user_keys(&keys).map_err(internal)
    }
}

const KEY_REFUSED: &str = "krowk's registry no longer takes this iPhone's key: it was removed from your devices, revoked on the dashboard, or left off your device list when it was started over. It has stopped syncing — reset it, then sign in and pair it again from a device on your list";

const REMOVED: &str = "this iPhone is no longer on your device list — it was removed, and syncs nothing more. Reset it, then sign in and pair it again from a device on your list";

/// The list was started over: when, as its new first entry dates it, if
/// there is one to say it by.
fn reset(first: &SignedEntry) -> String {
    let when = Entry::decode(&first.bytes)
        .ok()
        .and_then(|e| jiff::Timestamp::from_second(i64::try_from(e.time).ok()?).ok())
        .map(|t| format!(" on {}", t.strftime("%Y-%m-%d at %H:%M UTC")))
        .unwrap_or_default();
    format!(
        "your device list was started over{when}, and this iPhone is not on the new one. It has stopped syncing. If you started over, reset it and pair it again; if you did not, someone with your password did, so change it"
    )
}

/// The list's entries as the verifier takes them.
fn decode(list: &krowk_api::sync::DeviceList) -> Result<Vec<SignedEntry>, KrowkError> {
    let bad = |seq: u64| refused("malformed_response", format!("the registry served entry {seq} of your device list in a shape krowk does not read"));
    list.entries
        .iter()
        .map(|e| {
            let bytes = e2e::unhex(&e.entry).ok_or_else(|| bad(e.seq))?;
            let sigs = e2e::unhex(&e.signatures).ok_or_else(|| bad(e.seq))?;
            SignedEntry::from_parts(bytes, &sigs).map_err(|_| bad(e.seq))
        })
        .collect()
}

fn key_refused(e: krowk_client::e2e::Error) -> KrowkError {
    refused("user_key_refused", format!("{} — the registry's wrap is not one your devices made, and this iPhone keeps the key it holds", e.0))
}

fn not_paired() -> KrowkError {
    refused("not_paired", "this phone is not paired yet — add it from a computer with `krowk devices add`".into())
}

fn refused(code: &str, detail: String) -> KrowkError {
    KrowkError::Refused { code: code.into(), detail }
}
