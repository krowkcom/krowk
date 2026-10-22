//! The phone joining a pairing (R-E2E-3; canon, engineering/devices.md →
//! Adding a device): another of the person's devices shows a code (`krowk
//! devices add`), the phone takes it typed or scanned, and
//! [`Account::join_pairing`] (or [`Account::join_invite`], for a scanned
//! invite) runs the joining side — krowk-client's `pairing_join::join`,
//! the one every client runs (R-CLIENT-1) — over [`RegistryMailbox`], the
//! registry's mailbox as this crate reaches it. What the join keeps, the
//! user keys and the device list, goes to the app's `KeyStorage` through
//! the account's keystore; nothing of it, and no part of the pairing's
//! secrets, is handed to Swift.
//!
//! The rules are `pairing_join`'s: the code starts one pairing once,
//! nothing is retried, and any failure ends the pairing and forgets what
//! the join kept. A cancel is one more failure, up to the moment the other
//! device's sealed reply is seen: it lists this one once its reply is out,
//! acked or not, so from then on the join is seen through rather than
//! leave the phone listed without the keys.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use krowk_api::Client;
use krowk_api::sync::{Pairing, PairingStep};
use krowk_client::Zeroizing;
use krowk_client::device_chain::{self, Action, Entry};
use krowk_client::e2e::{self, DeviceId};
use krowk_client::pairing::{self, Binding, NewDevice, PairingCode, PeerKind, invite};
use krowk_client::pairing_join::{self, Field, JoinFailed, Mailbox, Timing};

use crate::KrowkError;
use crate::account::{Account, internal};

/// The longest a pause goes without looking at the cancel flag.
const SLICE: Duration = Duration::from_millis(250);

/// What the phone calls its OS on the other device's prompt when the app
/// names none: M1's phone is the iPhone; Android names its own.
const OS: &str = "iOS";

/// The registry's pairing mailbox as the phone reaches it: `client` the
/// account's (keyed, signed by this device for the claim; the joining
/// side's steps go with the key alone), `id` the pairing it is in.
///
/// Once `cancel` is set, a pause returns within a quarter of a second, the
/// steps and the claim fail at once with status 499 and code `cancelled`
/// — never 0, which `join` counts as an ack that may have landed — and so
/// does a read until the confirmation is out; after it, a read looks once
/// more for the other device's sealed reply before it fails. `end` still
/// ends the pairing. That device posts this one to the list once its
/// reply is out, acked or not, so once a reply that can be checked has
/// been seen after the confirmation, the check, the ack and the claims go
/// ahead, cancel or no cancel; what the last look cannot see is a reply
/// that lands between it and the `end`. Nothing the registry serves before
/// the confirmation, or that is not hex, stops a cancel.
pub struct RegistryMailbox {
    client: Arc<Client>,
    id: String,
    cancel: Arc<AtomicBool>,
    /// The confirmation step was sent: a reply may come.
    confirmed: AtomicBool,
    /// The sealed reply was seen, or the ack reached: too late to cancel.
    committed: AtomicBool,
}

impl RegistryMailbox {
    pub fn new(client: Arc<Client>, id: String, cancel: Arc<AtomicBool>) -> RegistryMailbox {
        RegistryMailbox { client, id, cancel, confirmed: AtomicBool::new(false), committed: AtomicBool::new(false) }
    }

    /// Whether a cancel stopped this mailbox: one set before the other
    /// device's reply was seen.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst) && !self.committed.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), krowk_api::Error> {
        if self.cancelled() {
            return Err(cancelled_error());
        }
        Ok(())
    }
}

/// A mailbox call refused because the join was cancelled.
fn cancelled_error() -> krowk_api::Error {
    let mut body = BTreeMap::new();
    body.insert("error".into(), serde_json::Value::String("cancelled".into()));
    body.insert("fix".into(), serde_json::Value::String("the pairing was cancelled on this device".into()));
    body.insert("retryable".into(), serde_json::Value::Bool(false));
    krowk_api::Error { status: 499, body }
}

impl Mailbox for RegistryMailbox {
    fn step(&self, step: PairingStep, message: &[u8]) -> Result<(), krowk_api::Error> {
        if step == PairingStep::Acknowledgement {
            self.committed.store(true, Ordering::SeqCst);
        }
        self.check()?;
        if step == PairingStep::Confirmation {
            self.confirmed.store(true, Ordering::SeqCst);
        }
        self.client.pairing_step(step, message).map(|_| ())
    }

    fn read(&self) -> Result<Pairing, krowk_api::Error> {
        if self.cancelled() && !self.confirmed.load(Ordering::SeqCst) {
            return Err(cancelled_error());
        }
        let p = self.client.show_pairing(&self.id)?;
        if self.confirmed.load(Ordering::SeqCst) && Field::SealedReply.blob(&p).is_some() {
            self.committed.store(true, Ordering::SeqCst);
        }
        // Cancelled with no reply yet: the one last look is over.
        self.check()?;
        Ok(p)
    }

    /// Ends the pairing whether or not the join was cancelled: a cancel is
    /// what asks for it.
    fn end(&self) {
        let _ = self.client.end_pairing();
    }

    fn claim_device(&self) -> Result<(), krowk_api::Error> {
        self.check()?;
        self.client.claim_key_device().map(|_| ())
    }

    fn pause(&self, d: Duration) {
        let until = Instant::now() + d;
        while !self.cancelled() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::sleep(left.min(SLICE));
        }
    }
}

/// A finished join: the device-list head the phone now pins, and the name
/// it joined under (as it was sent, cleaned and cut to 64 bytes).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Joined {
    pub seq: u64,
    pub device_name: String,
}

/// A scanned invite (`krowk://pair?v=1&code=…&user=…&device=…`), as
/// `krowk devices add` draws it under the code. `code` is the pairing code
/// as `XXXX-XXXX`, `user_id` the person it is for and `device` the id of
/// the device showing it, 32 lowercase hex: [`Account::join_invite`]
/// checks both before it spends the code.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct PairingInvite {
    pub code: String,
    pub user_id: String,
    pub device: String,
}

/// Never the code, as `PairingCode`'s own `Debug`.
impl std::fmt::Debug for PairingInvite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingInvite").field("code", &"…").field("user_id", &self.user_id).field("device", &self.device).finish()
    }
}

/// The pairing open now on the person's account: the device showing the
/// code, by id (32 lowercase hex) and by the name it is listed under, for
/// the phone to say "Pairing with <name>" before it spends the code.
/// `device_name` is the registry's word, cleaned as a device name is
/// (the characters a device list refuses dropped, cut to 64 bytes); empty
/// when nothing is left.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OpenPairing {
    pub device: String,
    pub device_name: String,
}

/// The invite in a scanned QR code, read strictly by krowk-client's
/// `pairing::invite`; None for anything that is not one, so the scanner
/// keeps looking.
#[uniffi::export]
pub fn parse_pairing_invite(uri: String) -> Option<PairingInvite> {
    let uri = Zeroizing::new(uri);
    let invite = invite::parse(&uri)?;
    Some(PairingInvite { code: invite.code.to_string(), user_id: invite.user_id.clone(), device: invite.a_device.to_string() })
}

#[uniffi::export]
impl Account {
    /// Joins the person's open pairing with `code`, the eight characters
    /// another of their devices shows (`krowk devices add`), as `name`
    /// (the characters a device list refuses dropped, trimmed, cut to 64
    /// bytes). Blocks until the other device has added this one, up to
    /// ten minutes while the person approves there: call it off the main
    /// thread. On success the user keys and the device list are in the
    /// app's storage.
    ///
    /// Refused with `pairing_failed` (the code is used up: ask for a new
    /// one), `not_added` (the keys came but the other device did not add
    /// this one in time; they were forgotten), `cancelled`, `no_pairing`
    /// (no device has one open), `already_joined`, `device_removed` or
    /// `pairing_busy` (a join is under way on this account already);
    /// `InvalidInput` for a code or a name that cannot be sent, before
    /// anything is; `Network` when the registry could not be reached
    /// before the code was spent.
    ///
    /// `os` is what the other device's prompt and the device list say this
    /// one runs, like `iOS 26.1`: cleaned as the name is, cut to 32 bytes,
    /// and `iOS` when nothing is left.
    pub fn join_pairing(&self, code: String, name: String, os: String) -> Result<Joined, KrowkError> {
        self.join(code_of(code)?, &name, &os, None)
    }

    /// As [`Account::join_pairing`], with a scanned invite: before the code
    /// is spent, the phone checks that it is signed in as the invite's
    /// person and that the open pairing is the one on the screen it was
    /// scanned from; refused with `invite_mismatch` otherwise, nothing
    /// sent. A mismatch costs no guess.
    pub fn join_invite(&self, invite: PairingInvite, name: String, os: String) -> Result<Joined, KrowkError> {
        let PairingInvite { code, user_id, device } = invite;
        self.join(code_of(code)?, &name, &os, Some(Expected { user_id, device }))
    }

    /// The pairing open now, without joining it: which device shows the
    /// code. Nothing is sent to the pairing, so asking costs no code.
    /// Refused with `no_pairing` when no device has one open; `Network`
    /// when the registry could not be reached. The join looks again
    /// itself: this is for the screen, and a scanned invite's device is
    /// checked by [`Account::join_invite`] all the same.
    pub fn open_pairing(&self) -> Result<OpenPairing, KrowkError> {
        let open = self.client.find_open_pairing().map_err(no_pairing)?;
        let device = DeviceId::parse(&open.initiator_device.id).ok_or_else(|| refused("malformed_response", "the registry's pairing names no device id — try again"))?;
        Ok(OpenPairing { device: device.to_string(), device_name: cleaned(&open.initiator_device.name, pairing::MAX_NAME) })
    }

    /// Ends the join under way, from any thread: within a second, unless a
    /// registry call is out, which is answered or times out first. The
    /// join is refused with `cancelled`, and once its hello has gone out
    /// the pairing is ended on the registry. Too late once the other
    /// device's sealed reply has been seen — that device lists this one
    /// by then — so a join that got that far finishes as it would have:
    /// `Joined`, `pairing_failed` or `not_added`. Does nothing when no
    /// join is under way: a cancel is not kept for the next one.
    pub fn cancel_pairing(&self) {
        if let Some(cancel) = self.pairing_slot().as_ref() {
            cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// What a scanned invite says the pairing is: checked against the account
/// and the open pairing before the code is spent.
struct Expected {
    user_id: String,
    device: String,
}

/// `text` as a code: the string wiped once parsed.
fn code_of(text: String) -> Result<PairingCode, KrowkError> {
    let text = Zeroizing::new(text);
    PairingCode::parse(&text).ok_or_else(|| KrowkError::InvalidInput {
        detail: "that is not a pairing code — it is the eight characters `krowk devices add` shows, like K7QF-9M3X; nothing was sent".into(),
    })
}

/// A join under way on an account: frees the account's one pairing slot
/// when dropped.
struct Running<'a> {
    account: &'a Account,
    cancel: Arc<AtomicBool>,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.account.pairing_slot().take();
    }
}

impl Account {
    fn pairing_slot(&self) -> MutexGuard<'_, Option<Arc<AtomicBool>>> {
        self.pairing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the account's pairing slot: one join at a time, since the
    /// registry holds one pairing per person and a second join's step
    /// would end the first's.
    fn begin_pairing(&self) -> Result<Running<'_>, KrowkError> {
        let mut slot = self.pairing_slot();
        if slot.is_some() {
            return Err(refused("pairing_busy", "this device is already joining a pairing — finish or cancel that one first"));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *slot = Some(Arc::clone(&cancel));
        Ok(Running { account: self, cancel })
    }

    fn join(&self, code: PairingCode, name: &str, os: &str, expected: Option<Expected>) -> Result<Joined, KrowkError> {
        let name = device_name(name)?;
        let os = match cleaned(os, pairing::MAX_OS) {
            os if os.is_empty() => OS.to_string(),
            os => os,
        };
        let run = self.begin_pairing()?;
        let cancel = &run.cancel;
        // Before the code is spent, a cancel wins over whatever a call
        // under way said meanwhile.
        let before = |e: KrowkError| if cancel.load(Ordering::SeqCst) { cancelled() } else { e };

        let device = self.keystore.device_key().map_err(internal)?;
        let signing = self.keystore.signing_key().map_err(internal)?;
        match listed(&self.client, &device.id()).map_err(before)? {
            Listing::Active if self.keystore.user_keys().map_err(internal)?.is_some() => {
                return Err(refused("already_joined", "this device is already on your device list — there is nothing to join"));
            }
            Listing::Removed => {
                return Err(refused("device_removed", "this device was removed from your device list, and a removed device's keys can never be added again — it can join only with new device keys"));
            }
            _ => {}
        }
        let open = self.client.find_open_pairing().map_err(no_pairing).map_err(before)?;
        let a_device = DeviceId::parse(&open.initiator_device.id).ok_or_else(|| refused("malformed_response", "the registry's pairing names no device id — try again")).map_err(before)?;
        let user_id = self.client.verify_key().map_err(KrowkError::from).map_err(before)?.user_id;
        if let Some(expected) = expected {
            if expected.user_id != user_id {
                return Err(refused("invite_mismatch", "that code is for another person's devices — sign in as the person it was shown to; nothing was sent"));
            }
            if expected.device != a_device.to_string() {
                return Err(refused("invite_mismatch", "the pairing open now is not the one that code was shown for — scan the code on the screen again; nothing was sent"));
            }
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(cancelled());
        }

        let me = NewDevice { device: device.public(), signing: signing.public(), name, os };
        drop(signing);
        let binding = Binding { kind: PeerKind::SamePersonDevice, user_id, a_device, b_device: device.id() };
        let mailbox = RegistryMailbox::new(Arc::clone(&self.client), open.id, Arc::clone(cancel));
        let joined = pairing_join::join(&mailbox, &self.keystore, &device, binding, code, me, &Timing::default());
        match joined {
            Ok(pairing_join::Joined { seq, name }) => Ok(Joined { seq, device_name: name }),
            Err(_) if mailbox.cancelled() => Err(cancelled()),
            Err(JoinFailed::CodeUsedUp) => Err(refused(
                "pairing_failed",
                "the pairing failed, and a code works once — get a new one: run `krowk devices add` again on the other device, then enter its code here. Nothing was kept",
            )),
            Err(JoinFailed::NotAdded) => Err(refused(
                "not_added",
                "the other device did not add this one to your device list, or the registry could not be reached to see it — the keys were forgotten and the code is used up; run `krowk devices add` again there for a new code",
            )),
        }
    }
}

fn refused(code: &str, detail: &str) -> KrowkError {
    KrowkError::Refused { code: code.into(), detail: detail.into() }
}

/// The registry's `no_pairing`, in the phone's words; anything else as it came.
fn no_pairing(e: krowk_api::Error) -> KrowkError {
    if e.code() == "no_pairing" {
        refused("no_pairing", "none of your devices has a pairing open — run `krowk devices add` on a device already set up")
    } else {
        e.into()
    }
}

fn cancelled() -> KrowkError {
    refused("cancelled", "the pairing was cancelled on this device")
}

/// The phone's name as the other device will be asked about it: without
/// the characters the device list refuses in a name (the control, format
/// and separator characters), trimmed, then cut to its 64 bytes on a
/// character boundary.
fn device_name(name: &str) -> Result<String, KrowkError> {
    let name = cleaned(name, pairing::MAX_NAME);
    if name.is_empty() {
        return Err(KrowkError::InvalidInput { detail: "the device needs a name for your other device to show — nothing was sent".into() });
    }
    Ok(name)
}

/// `text` without the characters the device list refuses in a name or an
/// os, trimmed, then cut to `max` bytes on a character boundary; empty
/// when nothing is left.
fn cleaned(text: &str, max: usize) -> String {
    let kept: String = text.chars().filter(|&c| !device_chain::refused_in_name(c)).collect();
    let kept = kept.trim();
    let mut end = kept.len().min(max);
    while !kept.is_char_boundary(end) {
        end -= 1;
    }
    kept[..end].trim_end().to_string()
}

#[derive(PartialEq, Eq)]
enum Listing {
    Never,
    Active,
    Removed,
}

/// Where `device` stands on the person's device list as the registry serves
/// it, replaying its adds and removes, as `krowk sync join` checks: only
/// ever a reason to refuse, so the registry's word is enough, and it never
/// makes the phone forget anything.
fn listed(client: &Client, device: &DeviceId) -> Result<Listing, KrowkError> {
    let list = client.device_list_all()?;
    let mut at = Listing::Never;
    for e in &list.entries {
        let bytes = e2e::unhex(&e.entry).ok_or_else(|| refused("device_list_invalid", "the registry served a device list entry that is not hex — nothing was changed"))?;
        let Ok(entry) = Entry::decode(&bytes) else { continue };
        if entry.subjects.iter().any(|s| s.id() == *device) {
            at = match entry.action {
                Action::Add => Listing::Active,
                Action::Remove => Listing::Removed,
                _ => at,
            };
        }
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_e2e_3_a_name_loses_what_the_list_refuses_and_is_cut_on_a_character() {
        assert_eq!(device_name("  Elvinas\u{2019}s iPhone\u{202E}\n ").unwrap(), "Elvinas\u{2019}s iPhone");
        let long = "é".repeat(40);
        let cut = device_name(&long).unwrap();
        assert!(cut.len() <= pairing::MAX_NAME && cut.chars().all(|c| c == 'é'), "{cut}");
        // Leading spaces don't eat into the 64 bytes.
        let padded = format!("{}{}", " ".repeat(10), "a".repeat(64));
        assert_eq!(device_name(&padded).unwrap(), "a".repeat(64));
        assert!(matches!(device_name(" \u{200B}\t"), Err(KrowkError::InvalidInput { .. })));
    }

    #[test]
    fn r_e2e_3_an_os_is_cleaned_and_cut_to_32_bytes() {
        assert_eq!(cleaned("iOS 26.1\u{202E}", pairing::MAX_OS), "iOS 26.1");
        assert!(cleaned(&"9".repeat(40), pairing::MAX_OS).len() == pairing::MAX_OS);
        assert_eq!(cleaned(" \u{200B} ", pairing::MAX_OS), "");
    }

    #[test]
    fn r_e2e_3_a_cancelled_mailbox_call_is_never_status_0() {
        let e = cancelled_error();
        assert_eq!((e.status, e.code().as_str()), (499, "cancelled"));
    }

    #[test]
    fn r_e2e_3_an_invite_never_shows_its_code_in_debug() {
        let invite = PairingInvite { code: "K7QF-9M3X".into(), user_id: "u".into(), device: "d".into() };
        assert!(!format!("{invite:?}").contains("K7QF"), "{invite:?}");
    }
}
