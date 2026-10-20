//! The joining side of a pairing (B, the device that types the code; R-E2E-3,
//! canon engineering/devices.md → Adding a device), run the same way on
//! every client (R-CLIENT-1). Each platform supplies only the registry's
//! mailbox as a `Mailbox`; the sequence, and its rules, are here.
//!
//! B sends its hello, waits for A's SPAKE2 message, confirms, waits for the
//! sealed reply, checks it (`pairing_payload::check`), keeps the keys, acks,
//! and then waits for A to post the `add` entry.
//!
//! **After any failure, the code is gone.** A step that fails, a refusal, a
//! timeout, a read the network lost — B ends the pairing and never starts
//! again with the typed code: `join` takes it by value and calls
//! `PairB::start` once, since each start gives a registry posing as A one
//! more guess. Nothing is retried, a step or a read: a step sent twice is
//! out of turn and ends the pairing anyway. Keys kept before the failure
//! are forgotten again.
//!
//! The claim, not the ack, says whether the join finished. A posts the
//! entry once its reply is out, acked or not; this device then claims its
//! key's device, which the registry allows only once the entry is on the
//! list. An ack whose answer was lost may well have landed.

use crate::e2e::{self, DeviceKey};
use crate::keystore::Keystore;
use crate::pairing::{Binding, NewDevice, PairB, PairingCode};
use crate::pairing_payload;
use krowk_api::sync::{Pairing, PairingStep};
use krowk_api::Error;
use std::time::{Duration, Instant};

/// The registry's pairing mailbox as the joining device reaches it.
pub trait Mailbox {
    /// One of B's steps (Join, Confirmation, Acknowledgement), sent once.
    fn step(&self, step: PairingStep, message: &[u8]) -> Result<(), Error>;
    /// The pairing this device is in (the CLI: `show_pairing(id)`). An
    /// `Err` is final: `join` reads no more, whatever the transport retried
    /// before giving it up.
    fn read(&self) -> Result<Pairing, Error>;
    /// Ends the pairing for good; best effort.
    fn end(&self);
    /// Claims the key's device (the CLI: device-signed `claim_key_device`),
    /// which the registry allows only once A has posted the `add` entry.
    fn claim_device(&self) -> Result<(), Error>;
    /// Waits between reads (the CLI: `std::thread::sleep`).
    fn pause(&self, d: Duration);
}

/// How long B waits, and how often it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Between reads of the pairing, and between claims.
    pub poll: Duration,
    /// For each of A's messages.
    pub wait: Duration,
    /// Once the ack is sent, for A to post the `add` entry.
    pub posted: Duration,
}

impl Default for Timing {
    /// A person walking between two machines, within the registry's ten
    /// minutes (its `poll_interval` is not used).
    fn default() -> Timing {
        Timing { poll: Duration::from_millis(500), wait: Duration::from_secs(10 * 60), posted: Duration::from_secs(60) }
    }
}

/// A join that finished: the device list's head this device now pins, and
/// the name it joined under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    pub seq: u64,
    pub name: String,
}

/// Why a join did not finish. Which check failed is nothing the person or
/// an attacker needs: either way the pairing is ended, the code is used up,
/// and nothing is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinFailed {
    /// Before A could post the entry: a step, a read, a check or the wait
    /// failed.
    CodeUsedUp,
    /// The keys were kept and the ack sent, but A did not post the entry
    /// (or the registry could not be reached to see it) in time.
    NotAdded,
}

/// A message field of the pairing, as `Mailbox::read` serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    JoinerMessage,
    InitiatorMessage,
    JoinerConfirmation,
    SealedReply,
    JoinerAck,
}

impl Field {
    /// The field's bytes: None while it is empty, or not hex.
    pub fn blob(self, p: &Pairing) -> Option<Vec<u8>> {
        let v = match self {
            Field::JoinerMessage => &p.joiner_message,
            Field::InitiatorMessage => &p.initiator_message,
            Field::JoinerConfirmation => &p.joiner_confirmation,
            Field::SealedReply => &p.sealed_reply,
            Field::JoinerAck => &p.joiner_ack,
        };
        if v.is_empty() { None } else { e2e::unhex(v) }
    }
}

/// B's side from the hello to the claim. `code` is consumed by the one
/// pairing it starts; `binding.a_device` is the device the reply must come
/// from, and `me` this device as it introduces itself, its key `device`.
pub fn join(mailbox: &dyn Mailbox, store: &Keystore, device: &DeviceKey, binding: Binding, code: PairingCode, me: NewDevice, timing: &Timing) -> Result<Joined, JoinFailed> {
    let seq = match steps(mailbox, store, device, binding, code, &me, timing) {
        Ok(seq) => seq,
        Err(saved) => {
            mailbox.end();
            if saved {
                let _ = store.forget_user_keys();
            }
            return Err(JoinFailed::CodeUsedUp);
        }
    };
    let deadline = Instant::now() + timing.posted;
    while mailbox.claim_device().is_err() {
        if Instant::now() > deadline {
            mailbox.end();
            let _ = store.forget_user_keys();
            return Err(JoinFailed::NotAdded);
        }
        mailbox.pause(timing.poll);
    }
    Ok(Joined { seq, name: me.name })
}

/// From the hello to the ack: the chain head on success. Every failure is
/// `Err(saved)`, whether this run kept keys the caller must forget.
fn steps(mailbox: &dyn Mailbox, store: &Keystore, device: &DeviceKey, binding: Binding, code: PairingCode, me: &NewDevice, timing: &Timing) -> Result<u64, bool> {
    let a_device = binding.a_device;
    let (b, hello) = PairB::start(binding, code, me.clone()).map_err(|_| false)?;
    mailbox.step(PairingStep::Join, &hello).map_err(|_| false)?;
    let spake = wait_for(mailbox, Field::InitiatorMessage, timing).ok_or(false)?;
    let (await_reply, confirm) = b.receive_spake(&spake).map_err(|_| false)?;
    mailbox.step(PairingStep::Confirmation, &confirm).map_err(|_| false)?;
    let reply = wait_for(mailbox, Field::SealedReply, timing).ok_or(false)?;
    let received = await_reply.receive_reply(&reply).map_err(|_| false)?;
    let checked = pairing_payload::check(received.payload(), a_device, me, device).map_err(|_| false)?;
    // Kept before the ack, so an ack that lands always leaves the keys
    // here; a failure after this forgets them again. A user key an earlier
    // pairing left goes only now, on A's authenticated word that this
    // device is new to the list.
    store.forget_user_keys().map_err(|_| false)?;
    store.save_user_keys(&checked.keys).map_err(|_| true)?;
    store.save_device_list(&checked.entries).map_err(|_| true)?;
    let (_, ack) = received.acknowledge();
    match mailbox.step(PairingStep::Acknowledgement, &ack) {
        // Lost on the way back, it may have landed: the claim says which.
        // Nothing is sent again either way.
        Ok(()) => Ok(checked.chain.head().seq),
        Err(e) if e.status == 0 => Ok(checked.chain.head().seq),
        Err(_) => Err(true),
    }
}

/// Reads the pairing until `field` has arrived. A read that fails, a
/// pairing that died, or `timing.wait` passing is None: B never asks again.
fn wait_for(mailbox: &dyn Mailbox, field: Field, timing: &Timing) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timing.wait;
    loop {
        let p = mailbox.read().ok()?;
        if let Some(b) = field.blob(&p) {
            return Some(b);
        }
        if p.state == "dead" || Instant::now() > deadline {
            return None;
        }
        mailbox.pause(timing.poll);
    }
}
