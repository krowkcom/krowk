//! What the paired device (A) seals to the new one (B) once the person has
//! said yes, and B's checks on it (R-E2E-3; canon, engineering/devices.md
//! → Adding a device). The reply carries the whole signed device chain with
//! the new `add` entry on its end, the user key wrapped to B, and A's
//! signing key. `pairing` carries it sealed and checks its MAC; this is
//! what's inside.
//!
//! B trusts the chain because the pairing authenticated it, not because the
//! registry served it: a new machine has no pin, so this reply is where its
//! trust comes from. `check` keeps nothing until the chain verifies from
//! seq 0 and its last entry adds exactly B's keys, name and OS, signed by A
//! alone, whose signing key the reply names; then the user key opens as the
//! id the chain names.
//!
//! Pure: no IO, no clock. The format is JSON, version 1, and the field
//! names are the format.

use crate::device_chain::{Action, Chain, Entry, Kind, SignedEntry, Subject};
use crate::e2e::{self, DeviceId, DeviceKey, SigningPublic};
use crate::pairing::NewDevice;
use crate::user_key::{UserKey, UserKeys};
use crate::Zeroizing;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Payload {
    v: u8,
    /// Each entry as `(hex(bytes), hex(signatures))`, entry 0 first.
    entries: Vec<(String, String)>,
    /// Every older generation of the user key wrapped under the next, so
    /// the new machine opens sessions sealed before it joined.
    previous: Vec<String>,
    generation: u32,
    wrapped_key: String,
    signing_key: String,
}

/// What B keeps once the reply checks: the user keys, the chain's entries
/// as sent, and the chain they verify to.
pub struct Checked {
    pub keys: UserKeys,
    pub entries: Vec<SignedEntry>,
    pub chain: Chain,
}

/// A's half: `entries` is the whole chain with the new device's `add` on
/// its end, `generation` the one that chain leaves current, `wrapped_key`
/// the user key wrapped to the new device, and `signing` A's own key.
pub fn seal(entries: &[SignedEntry], keys: &UserKeys, generation: u32, wrapped_key: &[u8], signing: &SigningPublic) -> Zeroizing<Vec<u8>> {
    let payload = Payload {
        v: 1,
        entries: entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        previous: keys.wraps().map(|(_, w)| e2e::hex(w)).collect(),
        generation,
        wrapped_key: e2e::hex(wrapped_key),
        signing_key: e2e::hex(&signing.0),
    };
    Zeroizing::new(serde_json::to_vec(&payload).expect("the payload serializes"))
}

/// B's half, on what `pairing::Received::payload` opened: `a_device` is the
/// device the pairing bound, `me` this machine as it introduced itself,
/// and `device` its key. The message says which check failed; it is for a
/// test or a debugger, not the person, to whom every failure reads alike.
pub fn check(payload: &[u8], a_device: DeviceId, me: &NewDevice, device: &DeviceKey) -> Result<Checked, String> {
    let p: Payload = serde_json::from_slice(payload).map_err(|_| "the reply is not a payload krowk reads")?;
    if p.v != 1 {
        return Err("the reply is from a newer krowk".into());
    }
    let entries = p
        .entries
        .iter()
        .map(|(e, s)| SignedEntry::from_parts(e2e::unhex(e).ok_or("not hex")?, &e2e::unhex(s).ok_or("not hex")?).map_err(|e| e.0))
        .collect::<Result<Vec<_>, String>>()?;
    let chain = Chain::verify(&entries, None).map_err(|e| e.0)?;
    let last = Entry::decode(&entries.last().ok_or("the chain is empty")?.bytes).map_err(|e| e.0)?;
    let mine = Subject { kind: Kind::Device, name: me.name.clone(), os: me.os.clone(), device: me.device, signing: me.signing };
    if last.action != Action::Add || last.subjects != [mine] || last.signers != [a_device] || last.seq == 0 {
        return Err("the chain's last entry does not add exactly this device, signed by the device that paired it".into());
    }
    let signing = e2e::unhex(&p.signing_key).and_then(|b| <[u8; 32]>::try_from(b).ok()).map(SigningPublic).ok_or("no signing key")?;
    if !chain.devices().iter().any(|d| d.id() == a_device && d.kind == Kind::Device && d.signing == signing) {
        return Err("the device that paired this one is not on the chain with the signing key it sent".into());
    }
    if p.generation != chain.generation() {
        return Err("the user key sent is not the generation the chain leaves current".into());
    }
    let wrapped = e2e::unhex(&p.wrapped_key).ok_or("not hex")?;
    let newest = UserKey::unwrap(&wrapped, chain.generation(), chain.key_id(), device).map_err(|e| e.0)?;
    let previous = p.previous.iter().map(|w| e2e::unhex(w).ok_or("not hex")).collect::<Result<Vec<_>, _>>()?;
    let keys = UserKeys::new(newest, previous).and_then(|k| k.verified_by(&chain)).map_err(|e| e.0)?;
    Ok(Checked { keys, entries, chain })
}
