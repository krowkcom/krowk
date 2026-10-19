//! What the paired device seals to the new one, and the new device's
//! checks on it (R-E2E-3; engineering/devices.md in Canon → Adding a
//! device): the reply checks on the device it adds, and each way it must
//! not — another device added, another signer, another signing key, another
//! generation, another device's wrap, a newer format, not JSON at all.
//!
//! A's list has been through a removal before B joins, so the user key is
//! at generation 2 and the reply carries generation 1 wrapped under it.

use krowk_client::device_chain::{Batch, Change, Chain, Kind, SignedEntry, Subject};
use krowk_client::e2e::{DeviceKey, SigningKey};
use krowk_client::pairing::NewDevice;
use krowk_client::pairing_payload::{check, seal};
use krowk_client::user_key::UserKeys;

const TIME: u64 = 1_790_000_000;

struct Device {
    key: DeviceKey,
    signing: SigningKey,
    name: &'static str,
}

impl Device {
    fn new(name: &'static str) -> Device {
        Device { key: DeviceKey::generate(), signing: SigningKey::generate(), name }
    }

    fn subject(&self) -> Subject {
        Subject { kind: Kind::Device, name: self.name.into(), os: "linux".into(), device: self.key.public(), signing: self.signing.public() }
    }

    /// The device as it introduces itself to the pairing.
    fn me(&self) -> NewDevice {
        NewDevice { device: self.key.public(), signing: self.signing.public(), name: self.name.into(), os: "linux".into() }
    }
}

/// A's side before the add: its chain, its entries, its keys, and the
/// generation-2 wrap it holds for itself.
struct A {
    device: Device,
    chain: Chain,
    entries: Vec<SignedEntry>,
    keys: UserKeys,
    own_wrap: Vec<u8>,
}

/// A starts a list, adds and removes C (the removal makes generation 2),
/// and holds every generation.
fn a() -> A {
    let a = Device::new("a-desktop");
    let c = Device::new("c-laptop");
    let (chain, first) = Chain::start(a.subject(), &a.signing, None, TIME).unwrap();
    let (chain, added) = chain.batch(&first.newest, vec![Change::Add(c.subject())], a.key.id(), &a.signing, TIME).unwrap();
    let (chain, removed) = chain.batch(&added.newest, vec![Change::Remove(c.subject())], a.key.id(), &a.signing, TIME).unwrap();
    assert_eq!(chain.generation(), 2);
    let own_wrap = removed.wraps.iter().find(|(d, _)| *d == a.key.id()).unwrap().1.clone();
    let entries = [first.entries, added.entries, removed.entries.clone()].concat();
    let keys = UserKeys::new(removed.newest, removed.links).unwrap();
    A { device: a, chain, entries, keys, own_wrap }
}

impl A {
    /// `signer` adds `who`, as `seal_reply` does: the whole chain with the
    /// add on its end, and the wrap the batch made for `who`.
    fn add(&self, who: &Device, signer: &Device) -> (Chain, Vec<SignedEntry>, Batch) {
        let (next, batch) = self.chain.batch(self.keys.newest(), vec![Change::Add(who.subject())], signer.key.id(), &signer.signing, TIME).unwrap();
        let all = [self.entries.clone(), batch.entries.clone()].concat();
        (next, all, batch)
    }
}

fn wrap_for(batch: &Batch, who: &Device) -> Vec<u8> {
    batch.wraps.iter().find(|(d, _)| *d == who.key.id()).unwrap().1.clone()
}

fn refused(payload: &[u8], a: &A, b: &Device) -> String {
    match check(payload, a.device.key.id(), &b.me(), &b.key) {
        Ok(_) => panic!("the payload checked"),
        Err(e) => e,
    }
}

#[test]
fn r_e2e_3_a_sealed_payload_checks_on_the_device_it_adds() {
    let a = a();
    let b = Device::new("b-macbook");
    let (next, all, batch) = a.add(&b, &a.device);
    let payload = seal(&all, &a.keys, next.generation(), &wrap_for(&batch, &b), &a.device.signing.public());
    let checked = check(&payload, a.device.key.id(), &b.me(), &b.key).unwrap();
    assert_eq!(checked.chain, next);
    assert_eq!(checked.chain.head().seq, 3);
    assert_eq!(checked.entries.len(), all.len());
    assert!(checked.entries.iter().zip(&all).all(|(x, y)| x.bytes == y.bytes && x.signatures_bytes() == y.signatures_bytes()));
    assert_eq!(checked.keys.newest(), a.keys.newest());
    // Generation 1 opens too: the new machine reads sessions sealed before
    // it joined.
    assert_eq!(checked.keys.open(1).unwrap(), a.keys.open(1).unwrap());
}

#[test]
fn r_e2e_3_a_last_entry_adding_another_device_is_refused() {
    let a = a();
    let (b, other) = (Device::new("b-macbook"), Device::new("someone-else"));
    let (next, all, batch) = a.add(&other, &a.device);
    let payload = seal(&all, &a.keys, next.generation(), &wrap_for(&batch, &other), &a.device.signing.public());
    assert!(refused(&payload, &a, &b).contains("does not add exactly this device"));
}

#[test]
fn r_e2e_3_a_last_entry_signed_by_a_device_other_than_the_pairing_one_is_refused() {
    let mut a = a();
    let d = Device::new("d-server");
    // D is on the list, holds the key, and signs B's add; A paired.
    let (chain, batch) = a.chain.batch(a.keys.newest(), vec![Change::Add(d.subject())], a.device.key.id(), &a.device.signing, TIME).unwrap();
    a.chain = chain;
    a.entries.extend(batch.entries);
    let b = Device::new("b-macbook");
    let (next, all, batch) = a.add(&b, &d);
    let payload = seal(&all, &a.keys, next.generation(), &wrap_for(&batch, &b), &a.device.signing.public());
    assert!(refused(&payload, &a, &b).contains("signed by the device that paired it"));
}

#[test]
fn r_e2e_3_a_pairing_device_whose_signing_key_differs_from_the_one_sent_is_refused() {
    let a = a();
    let b = Device::new("b-macbook");
    let (next, all, batch) = a.add(&b, &a.device);
    let payload = seal(&all, &a.keys, next.generation(), &wrap_for(&batch, &b), &SigningKey::generate().public());
    assert!(refused(&payload, &a, &b).contains("not on the chain with the signing key it sent"));
    let payload = seal(&all, &a.keys, next.generation(), &wrap_for(&batch, &b), &b.signing.public());
    assert!(refused(&payload, &a, &b).contains("not on the chain with the signing key it sent"));
}

#[test]
fn r_e2e_3_a_generation_that_is_not_the_chains_is_refused() {
    let a = a();
    let b = Device::new("b-macbook");
    let (next, all, batch) = a.add(&b, &a.device);
    for generation in [next.generation() - 1, next.generation() + 1] {
        let payload = seal(&all, &a.keys, generation, &wrap_for(&batch, &b), &a.device.signing.public());
        assert!(refused(&payload, &a, &b).contains("not the generation the chain leaves current"));
    }
}

#[test]
fn r_e2e_3_a_wrap_for_another_device_is_refused() {
    let a = a();
    let b = Device::new("b-macbook");
    let (next, all, _) = a.add(&b, &a.device);
    let payload = seal(&all, &a.keys, next.generation(), &a.own_wrap, &a.device.signing.public());
    assert!(refused(&payload, &a, &b).contains("wrapped for another device"));
}

#[test]
fn r_e2e_3_a_version_2_payload_is_refused() {
    let a = a();
    let b = Device::new("b-macbook");
    let (next, all, batch) = a.add(&b, &a.device);
    let payload = seal(&all, &a.keys, next.generation(), &wrap_for(&batch, &b), &a.device.signing.public());
    let mut json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    json["v"] = 2.into();
    assert_eq!(refused(&serde_json::to_vec(&json).unwrap(), &a, &b), "the reply is from a newer krowk");
}

#[test]
fn r_e2e_3_bytes_that_are_not_json_are_refused() {
    let a = a();
    let b = Device::new("b-macbook");
    for bytes in [&b"\x00\x01not json"[..], b"", b"{\"v\":1}"] {
        assert_eq!(refused(bytes, &a, &b), "the reply is not a payload krowk reads");
    }
}
