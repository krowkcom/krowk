//! A synced session's record, signed by the device that published it
//! (engineering/devices.md → Keys). The registry stores the session's
//! wrapped key; nothing about that blob says who made it. So the
//! publishing device signs the record with its Ed25519 key, and every
//! device that opens the session checks the signature against the verified
//! device list before it unwraps anything.
//!
//! The signed message is `"krowk/session-record/v1"` ‖ session id (16) ‖
//! user key generation (4, BE) ‖ that generation's user key id (16) ‖ seal
//! (1: `user`) ‖ the wrapped session key (74). The key id is the verifier's
//! own list's for the generation, which is what binds the record to the
//! person; the registry's `owner_user_id` is not signed, since a value the
//! registry supplies proves nothing against the registry.
//!
//! A record is accepted only when its seal is one this krowk knows
//! (`user`), checked first; the generation it names is one the list
//! commits to; and its signer is a device the list has ever held
//! (`Chain::signer`) — the Ed25519 key checked is the list's, never one the
//! record carries. A host taking a session up to write under it asks more:
//! the signer must be listed now (`Signer::Listed`), so a removed device's
//! record, planted under a generation it held, is never written under.

use crate::device_chain::Chain;
use crate::e2e::{self, DeviceId, Error, SigningKey};
use crate::user_key::UserKey;

const LABEL: &[u8] = b"krowk/session-record/v1";
/// An Ed25519 signature.
pub const SIGNATURE: usize = 64;
/// The seal of a private session: its key is wrapped under its owner's
/// user key.
pub const SEAL_USER: &str = "user";

/// Which signers a record is accepted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signer {
    /// Any device the list has ever held: reading a session.
    EverHeld,
    /// A device on the list now: taking a session up to write under it.
    Listed,
}

fn seal_byte(seal: &str) -> Result<u8, Error> {
    match seal {
        SEAL_USER => Ok(1),
        _ => Err(Error("the session is sealed some way other than to your user key, which this krowk does not open".into())),
    }
}

fn message(session: &[u8; 16], generation: u32, key: [u8; 16], seal: u8, wrapped: &[u8]) -> Vec<u8> {
    [LABEL, session, &generation.to_be_bytes(), &key, &[seal], wrapped].concat()
}

/// Signs a record this device publishes: `wrapped` is the session key
/// sealed under `user` (`e2e::seal_session_key`).
pub fn sign(session: &[u8; 16], wrapped: &[u8], seal: &str, user: &UserKey, key: &SigningKey) -> Result<[u8; SIGNATURE], Error> {
    let generation = e2e::session_key_generation(wrapped)?;
    if generation != user.generation() {
        return Err(Error(format!("the session key is sealed under generation {generation}, not generation {} this record names", user.generation())));
    }
    Ok(key.sign(&message(session, generation, user.id().0, seal_byte(seal)?, wrapped)))
}

/// Checks a record against the verified device list. Every refusal leaves
/// the session unopened.
pub fn verify(session: &[u8; 16], wrapped: &[u8], seal: &str, signer: DeviceId, signature: &[u8], chain: &Chain, from: Signer) -> Result<(), Error> {
    let seal = seal_byte(seal)?;
    let generation = e2e::session_key_generation(wrapped)?;
    let key = chain.key_id_at(generation).ok_or_else(|| Error(format!("the session names user key generation {generation}, which your device list does not reach — catch this device up with it, then try again")))?;
    let signing = chain.signer(signer).ok_or_else(|| Error(format!("the session's record is signed by device {signer}, which your device list has never held — refused")))?;
    if from == Signer::Listed && !chain.devices().iter().any(|d| d.id() == signer) {
        return Err(Error(format!("the session's record is signed by device {signer}, which is no longer on your device list — nothing is written under it; start a new session and host that")));
    }
    let signature: [u8; SIGNATURE] = signature.try_into().map_err(|_| Error("the session's record carries no signature krowk reads — refused".into()))?;
    signing.verify(&message(session, generation, key.0, seal, wrapped), &signature).map_err(|_| Error("the session's record signature does not verify — refused".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_chain::{Change, Kind, Subject};
    use crate::e2e::{DeviceKey, SessionKey};
    use crate::user_key::UserKeys;

    const SESSION: [u8; 16] = *b"0123456789abcdef";
    const T: u64 = 1_790_000_000;

    struct Dev {
        key: DeviceKey,
        signing: SigningKey,
        name: &'static str,
    }

    impl Dev {
        fn new(name: &'static str) -> Dev {
            Dev { key: DeviceKey::generate(), signing: SigningKey::generate(), name }
        }
        fn subject(&self) -> Subject {
            Subject { kind: Kind::Device, name: self.name.into(), os: "linux".into(), device: self.key.public(), signing: self.signing.public() }
        }
        fn id(&self) -> DeviceId {
            self.key.id()
        }
    }

    /// A record `by` publishes under `user`.
    fn record(by: &Dev, user: &UserKey) -> (Vec<u8>, [u8; SIGNATURE]) {
        let keys = UserKeys::new(user.clone(), []).unwrap();
        let wrapped = e2e::seal_session_key(&SessionKey::generate(), &SESSION, &keys, user.generation()).unwrap();
        let sig = sign(&SESSION, &wrapped, SEAL_USER, user, &by.signing).unwrap();
        (wrapped, sig)
    }

    #[test]
    fn d8b_a_record_signed_by_a_listed_device_verifies_and_nothing_else_does() {
        let (laptop, phone) = (Dev::new("laptop"), Dev::new("phone"));
        let (chain, start) = Chain::start(laptop.subject(), &laptop.signing, None, T).unwrap();
        let (chain, _) = chain.batch(&start.newest, vec![Change::Add(phone.subject())], laptop.id(), &laptop.signing, T + 1).unwrap();
        let (wrapped, sig) = record(&phone, &start.newest);
        let check = |session: &[u8; 16], wrapped: &[u8], seal: &str, signer: DeviceId, sig: &[u8], chain: &Chain| verify(session, wrapped, seal, signer, sig, chain, Signer::EverHeld);
        check(&SESSION, &wrapped, SEAL_USER, phone.id(), &sig, &chain).unwrap();
        verify(&SESSION, &wrapped, SEAL_USER, phone.id(), &sig, &chain, Signer::Listed).unwrap();
        assert!(check(&SESSION, &wrapped, SEAL_USER, laptop.id(), &sig, &chain).is_err(), "named as another signer");
        assert!(check(b"another-session!", &wrapped, SEAL_USER, phone.id(), &sig, &chain).is_err(), "moved to another session");
        assert!(check(&SESSION, &wrapped, "workspace", phone.id(), &sig, &chain).unwrap_err().0.contains("some way other"));
        let mut other = wrapped.clone();
        other[40] ^= 1;
        assert!(check(&SESSION, &other, SEAL_USER, phone.id(), &sig, &chain).is_err(), "another wrapped key");
        assert!(check(&SESSION, &wrapped, SEAL_USER, phone.id(), &[0; 63], &chain).is_err(), "no signature");
        let stranger = Dev::new("stranger");
        let (w, s) = record(&stranger, &start.newest);
        assert!(check(&SESSION, &w, SEAL_USER, stranger.id(), &s, &chain).unwrap_err().0.contains("never held"));
        // Another person's list: its generation 1 is another key id.
        let (theirs, _) = Chain::start(phone.subject(), &phone.signing, None, T).unwrap();
        assert!(check(&SESSION, &wrapped, SEAL_USER, phone.id(), &sig, &theirs).is_err(), "another person's list");
        let g2 = start.newest.next().unwrap();
        let (w, s) = record(&laptop, &g2);
        assert!(check(&SESSION, &w, SEAL_USER, laptop.id(), &s, &chain).unwrap_err().0.contains("does not reach"));
    }

    /// D8b (M1 of #206's review): a removed device's record still reads —
    /// it held the generation — but is never taken up to write under.
    #[test]
    fn d8b_a_removed_devices_record_is_read_but_never_written_under() {
        let (laptop, thief) = (Dev::new("laptop"), Dev::new("thief"));
        let (chain, start) = Chain::start(laptop.subject(), &laptop.signing, None, T).unwrap();
        let (chain, _) = chain.batch(&start.newest, vec![Change::Add(thief.subject())], laptop.id(), &laptop.signing, T + 1).unwrap();
        let (chain, _) = chain.batch(&start.newest, vec![Change::Remove(thief.subject())], laptop.id(), &laptop.signing, T + 2).unwrap();
        let (w, s) = record(&thief, &start.newest);
        verify(&SESSION, &w, SEAL_USER, thief.id(), &s, &chain, Signer::EverHeld).unwrap();
        assert!(verify(&SESSION, &w, SEAL_USER, thief.id(), &s, &chain, Signer::Listed).unwrap_err().0.contains("no longer on your device list"));
    }

    #[test]
    fn d8b_signing_refuses_a_record_whose_key_is_not_the_one_named() {
        let laptop = Dev::new("laptop");
        let user = UserKey::first();
        let (wrapped, _) = record(&laptop, &user);
        assert!(sign(&SESSION, &wrapped, SEAL_USER, &user.next().unwrap(), &laptop.signing).is_err());
        assert!(sign(&SESSION, &wrapped, "workspace", &user, &laptop.signing).is_err());
    }

    /// Frozen: a change to the signed message makes this stop verifying.
    #[test]
    fn d8b_a_frozen_session_record_still_verifies() {
        let signing = SigningKey::from_secret(&[0x51; 32]).unwrap();
        let user = UserKey::from_bytes(1, [0x61; 32]).unwrap();
        let wrapped = e2e::unhex(FROZEN_WRAPPED).unwrap();
        let expected = e2e::unhex(FROZEN_SIGNATURE).unwrap();
        signing.public().verify(&message(&SESSION, 1, user.id().0, 1, &wrapped), &expected.try_into().unwrap()).unwrap();
    }

    const FROZEN_WRAPPED: &str = "02020000000137ce51273d5a4c1d782c55449f0695d10971f42123e9627d4f3fb91d84a4e30d7f2d338f065cdeebc7616fc12977df8a4a47d22d9c5b7b4c7dd0e89be39db088019e3513";
    const FROZEN_SIGNATURE: &str = "8a43674ccef8b057629f766186ea91de0aab76b16a5cefbf948597beb0797fb3fbe48d2fb43c3cdf5312971315cf621071b4ade5709f40518353034b04539305";

    /// Prints a fresh signature for the frozen test above: run once with
    /// `--ignored --nocapture` when the format is meant to change.
    #[test]
    #[ignore]
    fn print_frozen_session_record() {
        let signing = SigningKey::from_secret(&[0x51; 32]).unwrap();
        let user = UserKey::from_bytes(1, [0x61; 32]).unwrap();
        let wrapped = e2e::unhex(FROZEN_WRAPPED).unwrap();
        println!("signature {}", e2e::hex(&sign(&SESSION, &wrapped, SEAL_USER, &user, &signing).unwrap()));
    }
}
