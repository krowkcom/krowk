//! R-E2E-3: the paired phone checks it is still on the person's device
//! list (`Account::check_device_list`), against the stand-in registry, as
//! `krowk sync status` checks it: a removed phone is told so, a list
//! started over is told apart, and a list that only grew is taken up with
//! its newer user key.

mod common;

use std::sync::Arc;

use common::{Bridge, Memory, World, key, signed_in, subject};
use krowk_api::sync::ListPost;
use krowk_client::device_chain::{Batch, Chain, Change, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceKey, DeviceSigner, SigningKey};
use krowk_client::keystore::Keystore;
use krowk_ffi::{Account, KeyStorage, KrowkError};

/// The list as the registry serves it, verified from entry 0.
fn served(w: &World) -> (Chain, Vec<SignedEntry>) {
    let list = w.host.device_list_all().unwrap();
    let entries: Vec<SignedEntry> = list.entries.iter().map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).unwrap(), &e2e::unhex(&e.signatures).unwrap()).unwrap()).collect();
    (Chain::verify(&entries, None).unwrap(), entries)
}

fn post_of(batch: &Batch, start_over: bool) -> ListPost {
    ListPost {
        entries: batch.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| e2e::hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over,
    }
}

/// The host signs `changes` onto the list, rotating the key for a removal,
/// as `krowk devices remove` does; the user key it leaves current.
fn host_posts(w: &World, held: &krowk_client::user_key::UserKey, changes: impl FnOnce(&Chain) -> Vec<Change<'static>>, time: u64) -> krowk_client::user_key::UserKey {
    let (chain, _) = served(w);
    let (_, batch) = chain.batch(held, changes(&chain), w.host_device.id(), &w.host_signing, time).unwrap();
    w.host.append_device_list(&post_of(&batch, false)).unwrap();
    batch.newest
}

fn removal_of(name: &'static str) -> impl FnOnce(&Chain) -> Vec<Change<'static>> {
    move |chain| {
        let d = chain.devices().iter().find(|d| d.name == name).unwrap();
        vec![Change::Remove(Subject::of(d))]
    }
}

/// The phone's storage with another key of the person in place of its
/// own: one bound to no device, which the registry lets read the list
/// even once the phone's own key is revoked, so the check reads the list
/// itself rather than the registry's refusal.
fn reading_with_an_unbound_key(w: &World) -> Arc<Account> {
    let copy = Memory::default();
    for (slot, bytes) in w.storage.0.lock().unwrap().iter() {
        copy.write(slot.clone(), bytes.clone()).unwrap();
    }
    let unbound = signed_in(&key("phone-reader"));
    copy.write("credentials.json".into(), unbound.read("credentials.json".into()).unwrap().unwrap()).unwrap();
    Account::open(w.api.clone(), Arc::new(copy)).unwrap()
}

fn refusal(r: Result<(), KrowkError>) -> (String, String) {
    match r {
        Err(KrowkError::Refused { code, detail }) => (code, detail),
        other => panic!("not a refusal: {other:?}"),
    }
}

fn kept_root(storage: &Arc<Memory>) -> [u8; 32] {
    Keystore::with(Arc::new(Bridge(storage.clone()))).device_list().unwrap().unwrap().root()
}

#[test]
fn r_e2e_3_a_phone_on_the_list_checks_ok() {
    let w = World::new();
    w.phone.check_device_list().unwrap();
    // Twice: nothing it kept stops the next check.
    w.phone.check_device_list().unwrap();
    assert!(!w.phone.has_recovery_kit().unwrap(), "the world's list was made with no kit");
}

#[test]
fn r_e2e_3_a_removed_phone_is_refused_device_removed_and_keeps_its_pin() {
    let w = World::new();
    let before = kept_root(&w.storage);
    host_posts(&w, &w.user, removal_of("phone"), common::T0 + 10);

    // Its key is revoked with it: the registry refuses the phone's every
    // call, and that is what says it was removed.
    let (code, detail) = refusal(w.phone.check_device_list());
    assert_eq!(code, "device_removed", "{detail}");
    assert!(detail.contains("reset it") && detail.contains("pair it again"), "{detail}");
    assert_eq!(kept_root(&w.storage), before);
    // Again, the same: it stops, it doesn't retry its way back.
    assert_eq!(refusal(w.phone.check_device_list()).0, "device_removed");

    // Read with a key the registry still takes, the verified list says it.
    let (code, detail) = refusal(reading_with_an_unbound_key(&w).check_device_list());
    assert_eq!(code, "device_removed", "{detail}");
    assert!(detail.contains("no longer on your device list"), "{detail}");
}

#[test]
fn r_e2e_3_a_list_started_over_is_device_list_reset_and_never_adopted() {
    let w = World::new();
    let before = kept_root(&w.storage);
    // The host starts over: a new first entry, a new epoch, the phone on
    // none of it.
    let (d, s) = (DeviceKey::generate(), SigningKey::generate());
    let at = 1_800_000_000;
    let (_, start) = Chain::start(subject("host again", &d, &s), &s, None, at).unwrap();
    let again = krowk_api::Client::new(&w.api, &key("host")).signed_by(DeviceSigner::new(d.id(), SigningKey::from_secret(&*s.secret_bytes()).unwrap()).shared());
    again.init_device_list(&post_of(&start, true)).unwrap();

    let (code, detail) = refusal(reading_with_an_unbound_key(&w).check_device_list());
    assert_eq!(code, "device_list_reset", "{detail}");
    assert!(detail.contains("started over on 2027-01-15 at 08:00 UTC"), "{detail}");
    assert_eq!(kept_root(&w.storage), before, "the new list is not adopted");
    // The phone's own key went with the old list: the registry refuses it.
    assert_eq!(refusal(w.phone.check_device_list()).0, "device_removed");
}

#[test]
fn r_e2e_3_a_rotation_the_phone_survives_is_taken_up() {
    let w = World::new();
    // Another device joins, then is removed: the key rotates to
    // generation 2, wrapped to the host and the phone.
    let (d, s) = (DeviceKey::generate(), SigningKey::generate());
    let other = subject("old laptop", &d, &s);
    let held = host_posts(&w, &w.user, move |_| vec![Change::Add(other)], common::T0 + 10);
    let newest = host_posts(&w, &held, removal_of("old laptop"), common::T0 + 20);
    assert_eq!(newest.generation(), 2);

    w.phone.check_device_list().unwrap();
    let ks = Keystore::with(Arc::new(Bridge(w.storage.clone())));
    let keys = ks.user_keys().unwrap().unwrap();
    assert_eq!(keys.newest().generation(), 2);
    assert_eq!(keys.newest().id(), newest.id());
    assert_eq!(ks.device_list().unwrap().unwrap().head(), served(&w).0.head(), "the grown list is the pin");
    // And the older generation still opens, down the chain.
    assert!(keys.verified_by(&served(&w).0).is_ok());
    w.phone.check_device_list().unwrap();
}

#[test]
fn r_e2e_3_an_unpaired_phone_is_not_paired() {
    let w = World::new();
    let fresh = Account::open(w.api.clone(), signed_in(&key("fresh"))).unwrap();
    assert_eq!(refusal(fresh.check_device_list()).0, "not_paired");
    assert!(!fresh.has_recovery_kit().unwrap());
}

/// The phone's Keychain, counting the writes of the device list.
struct Counting(Arc<Memory>, std::sync::atomic::AtomicUsize);

impl KeyStorage for Counting {
    fn read(&self, slot: String) -> Result<Option<Vec<u8>>, KrowkError> {
        self.0.read(slot)
    }
    fn write(&self, slot: String, bytes: Vec<u8>) -> Result<(), KrowkError> {
        if slot == "device-list.json" {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.0.write(slot, bytes)
    }
    fn remove(&self, slot: String) -> Result<(), KrowkError> {
        self.0.remove(slot)
    }
}

#[test]
fn r_e2e_3_a_list_the_same_as_the_pin_is_not_written_again_and_one_that_grew_is() {
    let w = World::new();
    let counting = Arc::new(Counting(w.storage.clone(), Default::default()));
    let phone = Account::open(w.api.clone(), counting.clone()).unwrap();
    let writes = || counting.1.load(std::sync::atomic::Ordering::SeqCst);
    phone.check_device_list().unwrap();
    phone.check_device_list().unwrap();
    assert_eq!(writes(), 0, "an unchanged list is not written again");
    let (d, s) = (DeviceKey::generate(), SigningKey::generate());
    let other = subject("new laptop", &d, &s);
    host_posts(&w, &w.user, move |_| vec![Change::Add(other)], common::T0 + 10);
    phone.check_device_list().unwrap();
    assert_eq!(writes(), 1, "a list that grew is the new pin");
    assert_eq!(kept_root(&w.storage), served(&w).0.root());
}

#[test]
fn r_e2e_3_a_registry_list_behind_or_beside_the_pin_is_refused_and_the_pin_kept() {
    let w = World::new();
    // The phone pinned an entry the registry never got: what it serves is
    // shorter than the pin (a rollback), then forked from it.
    let (chain, entries) = served(&w);
    let (d, s) = (DeviceKey::generate(), SigningKey::generate());
    let (_, batch) = chain.batch(&w.user, vec![Change::Add(subject("only pinned", &d, &s))], w.host_device.id(), &w.host_signing, common::T0 + 10).unwrap();
    let pinned: Vec<SignedEntry> = entries.iter().cloned().chain(batch.entries.iter().cloned()).collect();
    let ks = Keystore::with(Arc::new(Bridge(w.storage.clone())));
    ks.save_device_list(&pinned).unwrap();
    let head = || ks.device_list().unwrap().unwrap().head();
    let before = head();

    let (code, detail) = refusal(w.phone.check_device_list());
    assert_eq!(code, "device_list_refused", "shorter than the pin: {detail}");
    assert_eq!(head(), before);

    let (d2, s2) = (DeviceKey::generate(), SigningKey::generate());
    let other = subject("on the registry", &d2, &s2);
    host_posts(&w, &w.user, move |_| vec![Change::Add(other)], common::T0 + 11);
    let (code, detail) = refusal(w.phone.check_device_list());
    assert_eq!(code, "device_list_refused", "forked from the pin: {detail}");
    assert_eq!(head(), before);
}

#[test]
fn r_e2e_3_a_kept_list_with_a_recovery_kit_says_so() {
    let storage = signed_in(&key("kit"));
    let ks = Keystore::with(Arc::new(Bridge(storage.clone())));
    let (device, signing) = (ks.device_key().unwrap(), ks.signing_key().unwrap());
    let (kit_device, kit_signing) = (DeviceKey::generate(), SigningKey::generate());
    let kit = Subject { kind: krowk_client::device_chain::Kind::Recovery, ..subject("recovery kit", &kit_device, &kit_signing) };
    let (_, start) = Chain::start(subject("phone", &device, &signing), &signing, Some((kit, &kit_signing)), common::T0).unwrap();
    ks.save_device_list(&start.entries).unwrap();
    let phone = Account::open("http://127.0.0.1:9/v1".into(), storage).unwrap();
    assert!(phone.has_recovery_kit().unwrap());
}
