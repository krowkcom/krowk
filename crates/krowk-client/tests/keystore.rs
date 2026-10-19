//! The device key and the wrapped account key in a krowk home (R-E2E-3). Each test gets homes of its own under a scratch directory.
//!
//! The rules hold on any storage (R-CLIENT-4): the policy tests run once on
//! files in a scratch home and once in memory, through the one `Keystore`.

use krowk_client::e2e::{self, AccountKey, SessionKey};
use krowk_client::keystore::{FileStorage, KeyStorage, Keystore, MemoryStorage, Slot};
use krowk_client::user_key::{UserKey, UserKeys};
use std::path::PathBuf;
use std::sync::Arc;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("krowk-client-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Where a policy test makes its devices: homes under a scratch directory,
/// or memory.
enum Rig {
    Files(PathBuf),
    Memory,
}

impl Rig {
    /// The same test's two rigs, files first.
    fn both(name: &str) -> [(&'static str, Rig); 2] {
        [("files", Rig::Files(scratch(name))), ("memory", Rig::Memory)]
    }

    /// A device, its keystore and the storage under it.
    fn device(&self, name: &str) -> (Keystore, Arc<dyn KeyStorage>) {
        let storage: Arc<dyn KeyStorage> = match self {
            Rig::Files(root) => Arc::new(FileStorage::new(&root.join(name))),
            Rig::Memory => Arc::new(MemoryStorage::new()),
        };
        (Keystore::with(storage.clone()), storage)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Rig::Files(root) = self {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// The slot's bytes copied from one storage into another, as a file copied
/// between homes.
fn copy(slot: Slot, from: &dyn KeyStorage, to: &dyn KeyStorage) {
    to.write(slot, &from.read(slot).unwrap().unwrap()).unwrap();
}

/// The slot's JSON, edited.
fn edit(storage: &dyn KeyStorage, slot: Slot, change: impl FnOnce(&mut serde_json::Value)) {
    let mut v: serde_json::Value = serde_json::from_slice(&storage.read(slot).unwrap().unwrap()).unwrap();
    change(&mut v);
    storage.write(slot, v.to_string().as_bytes()).unwrap();
}

#[test]
fn r_e2e_3_r_client_4_each_device_opens_its_wrapped_account_key_with_its_own_device_key_only_on_any_storage() {
    for (label, rig) in Rig::both("recover") {
        let ((first, first_storage), (second, second_storage)) = (rig.device("laptop"), rig.device("desktop"));
        let account = AccountKey::generate();
        let (setup, _) = first.recover(account.clone()).unwrap();
        assert!(setup.device_created, "{label}");
        assert_eq!(first.account().unwrap().unwrap(), setup.account, "{label}");
        let (restored, _) = second.recover(account.clone()).unwrap();
        assert_ne!(restored.device.id(), setup.device.id(), "{label}: the second home is its own device");
        assert_eq!(second.account().unwrap().unwrap(), setup.account, "{label}");

        copy(Slot::Account, &*first_storage, &*second_storage);
        assert!(second.account().unwrap_err().contains("was wrapped for device"), "{label}");
        // And with the stored device id edited to match, the HPKE binding
        // still refuses it.
        edit(&*second_storage, Slot::Account, |v| v["device_id"] = restored.device.id().to_string().into());
        assert!(second.account().unwrap_err().contains("does not open"), "{label}");
    }
}

/// A key `recover` put here is replaced by the next `recover`.
#[test]
fn r_e2e_3_r_client_4_a_recovered_account_key_is_replaced_by_the_next_on_any_storage() {
    for (label, rig) in Rig::both("retry") {
        let (store, _) = rig.device("home");
        let (wrong, right) = (AccountKey::generate(), AccountKey::generate());
        let (first, _) = store.recover(wrong.clone()).unwrap();
        let (second, replaced) = store.recover(right.clone()).unwrap();
        assert_eq!(replaced, Some(wrong.id()), "{label}");
        assert_eq!(second.device.id(), first.device.id(), "{label}");
        assert_eq!(store.account().unwrap().unwrap(), right, "{label}");
    }
}

#[cfg(unix)]
#[test]
fn r_e2e_3_the_key_files_are_0600_and_hold_no_plain_account_key() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("mode");
    let files = FileStorage::new(&root.join("home"));
    let store = Keystore::new(&root.join("home"));
    let (setup, _) = store.recover(AccountKey::generate()).unwrap();
    for path in [files.path(Slot::Device), files.path(Slot::Account)] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
    }
    let account = std::fs::read_to_string(files.path(Slot::Account)).unwrap();
    assert!(!account.contains(&e2e::hex(setup.account.as_bytes())));
    let _ = std::fs::remove_dir_all(&root);
}

/// R-RELAY-1: the relay signing key is made once, `0600`, beside the
/// device key, and read back as the same key.
#[cfg(unix)]
#[test]
fn r_relay_1_the_signing_key_is_made_once_and_kept_0600() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("signing");
    let signing = root.join("home").join(Slot::Signing.file_name());
    let store = Keystore::new(&root.join("home"));
    let first = store.signing_key().unwrap();
    assert_eq!(std::fs::metadata(&signing).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(store.signing_key().unwrap().public(), first.public());
    std::fs::write(&signing, "{\"version\":2,\"secret\":\"00\"}").unwrap();
    assert!(store.signing_key().unwrap_err().contains("move it aside"));
    let _ = std::fs::remove_dir_all(&root);
}

/// D8: the user keys a device holds are kept `0600`, wrapped to its device
/// key, and read back as the same generations — so a session sealed under
/// an older generation opens after the device has moved on.
#[cfg(unix)]
#[test]
fn d8_user_keys_are_kept_0600_wrapped_to_the_device_and_read_back() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("user-keys");
    let files = FileStorage::new(&root.join("home"));
    let store = Keystore::new(&root.join("home"));
    assert!(store.user_keys().unwrap().is_none());
    let g1 = UserKey::first();
    store.save_user_keys(&UserKeys::new(g1.clone(), []).unwrap()).unwrap();
    assert_eq!(std::fs::metadata(files.path(Slot::UserKeys)).unwrap().permissions().mode() & 0o777, 0o600);
    let text = std::fs::read_to_string(files.path(Slot::UserKeys)).unwrap();
    assert!(!text.contains(&e2e::hex(g1.as_bytes())), "the user key is stored in the clear");
    assert_eq!(store.user_keys().unwrap().unwrap().newest(), &g1);

    let session = *b"session-00000001";
    let key = SessionKey::generate();
    let sealed = e2e::wrap_session_key(&key, &session, &g1);
    let g2 = g1.next().unwrap();
    store.save_user_keys(&UserKeys::new(g2.clone(), [g2.wrap_previous(&g1).unwrap()]).unwrap()).unwrap();
    let held = store.user_keys().unwrap().unwrap();
    assert_eq!(held.newest(), &g2);
    assert_eq!(e2e::unwrap_session_key(&sealed, &session, &held).unwrap().as_bytes(), key.as_bytes());

    // Copied to another device's home, it does not open there.
    let other = Keystore::new(&root.join("other"));
    other.device_key().unwrap();
    std::fs::copy(files.path(Slot::UserKeys), root.join("other").join(Slot::UserKeys.file_name())).unwrap();
    assert!(other.user_keys().unwrap_err().contains("was wrapped for device"));
    let _ = std::fs::remove_dir_all(&root);
}

/// D8: the store never goes back a generation, and never takes keys that
/// do not lead down to the one held — a registry offering its own key as
/// "newer", or an older copy, loses nothing.
#[test]
fn d8_r_client_4_the_user_keys_held_are_never_lost_or_swapped_on_any_storage() {
    for (label, rig) in Rig::both("user-keys-kept") {
        let (store, _) = rig.device("home");
        let g1 = UserKey::first();
        let g2 = g1.next().unwrap();
        store.save_user_keys(&UserKeys::new(g2.clone(), [g2.wrap_previous(&g1).unwrap()]).unwrap()).unwrap();
        assert!(store.save_user_keys(&UserKeys::new(g1.clone(), []).unwrap()).unwrap_err().contains("newer than generation 1"), "{label}");
        let forged2 = UserKey::from_bytes(2, [7; 32]).unwrap();
        assert!(store.save_user_keys(&UserKeys::new(forged2.clone(), []).unwrap()).unwrap_err().contains("do not lead back"), "{label}");
        let forged3 = forged2.next().unwrap();
        assert!(store.save_user_keys(&UserKeys::new(forged3.clone(), [forged3.wrap_previous(&forged2).unwrap()]).unwrap()).is_err(), "{label}");
        assert_eq!(store.user_keys().unwrap().unwrap().newest(), &g2, "{label}: what was held is still held");
        // The same generation again is kept, as is a real rotation.
        store.save_user_keys(&UserKeys::new(g2.clone(), [g2.wrap_previous(&g1).unwrap()]).unwrap()).unwrap();
        let g3 = g2.next().unwrap();
        store.save_user_keys(&UserKeys::new(g3.clone(), [g3.wrap_previous(&g2).unwrap(), g2.wrap_previous(&g1).unwrap()]).unwrap()).unwrap();
        assert_eq!(store.user_keys().unwrap().unwrap().open(1).unwrap(), g1, "{label}");
    }
}

/// D8 (m1 of #200's review): a save that omits or garbles an older wrap —
/// as a hostile registry's set could — keeps the ones already held.
#[test]
fn d8_r_client_4_a_save_never_drops_the_older_wraps_held_on_any_storage() {
    for (label, rig) in Rig::both("user-keys-wraps") {
        let (store, _) = rig.device("home");
        let g1 = UserKey::first();
        let g2 = g1.next().unwrap();
        let g3 = g2.next().unwrap();
        store.save_user_keys(&UserKeys::new(g2.clone(), [g2.wrap_previous(&g1).unwrap()]).unwrap()).unwrap();
        store.save_user_keys(&UserKeys::new(g2.clone(), []).unwrap()).unwrap();
        assert_eq!(store.user_keys().unwrap().unwrap().open(1).unwrap(), g1, "{label}: the same generation without its wraps");
        let mut bad = g2.wrap_previous(&g1).unwrap();
        let n = bad.len();
        bad[n - 1] ^= 1;
        store.save_user_keys(&UserKeys::new(g3.clone(), [g3.wrap_previous(&g2).unwrap(), bad]).unwrap()).unwrap();
        let held = store.user_keys().unwrap().unwrap();
        assert_eq!(held.newest(), &g3, "{label}");
        assert_eq!(held.open(1).unwrap(), g1, "{label}: a rotation with a garbled older wrap");
    }
}

/// D8b: the device list a device verified is kept, entry 0 to its head
/// (`0600` in a home), and read back by verifying it again. A save verifies
/// the new list against the kept head: a shorter one, or another list at the
/// same seq — another person's — is refused (#206's review, m1).
#[test]
fn d8b_r_client_4_the_device_list_only_grows_on_any_storage() {
    use krowk_client::device_chain::{Chain, Change, Kind, Subject};
    for (label, rig) in Rig::both("device-list") {
        let (store, storage) = rig.device("home");
        assert!(store.device_list().unwrap().is_none(), "{label}");
        let subject = |name: &str, d: &e2e::DeviceKey, s: &e2e::SigningKey| Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: d.public(), signing: s.public() };
        let (laptop, laptop_signing) = (e2e::DeviceKey::generate(), e2e::SigningKey::generate());
        let (phone, phone_signing) = (e2e::DeviceKey::generate(), e2e::SigningKey::generate());
        let t = 1_790_000_000;
        let (first, start) = Chain::start(subject("laptop", &laptop, &laptop_signing), &laptop_signing, None, t).unwrap();
        assert_eq!(store.save_device_list(&start.entries).unwrap().head(), first.head(), "{label}");
        #[cfg(unix)]
        if let Rig::Files(root) = &rig {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(root.join("home").join(Slot::DeviceList.file_name())).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(store.device_list().unwrap().unwrap().head(), first.head(), "{label}");
        let (second, add) = first.batch(&start.newest, vec![Change::Add(subject("phone", &phone, &phone_signing))], laptop.id(), &laptop_signing, t + 1).unwrap();
        let entries = [start.entries.clone(), add.entries].concat();
        store.save_device_list(&entries).unwrap();
        let read = store.device_list().unwrap().unwrap();
        assert_eq!((read.head(), read.devices().len()), (second.head(), 2), "{label}");
        assert!(store.save_device_list(&start.entries).is_err(), "{label}: a shorter list");
        // Another person's list, as long: forked at the kept head.
        let (_, theirs) = Chain::start(subject("other", &phone, &phone_signing), &phone_signing, None, t).unwrap();
        let (other, more) = Chain::verify(&theirs.entries, None).unwrap().batch(&theirs.newest, vec![Change::Add(subject("laptop", &laptop, &laptop_signing))], phone.id(), &phone_signing, t + 1).unwrap();
        assert_eq!(other.head().seq, second.head().seq, "{label}");
        assert!(store.save_device_list(&[theirs.entries, more.entries].concat()).is_err(), "{label}: another list at the same seq");
        assert_eq!(store.device_list().unwrap().unwrap().head(), second.head(), "{label}: the kept list is untouched");
        // A list edited in storage does not verify.
        edit(&*storage, Slot::DeviceList, |v| v["entries"][1][1] = e2e::hex(&[0; 80]).into());
        assert!(store.device_list().is_err(), "{label}");
    }
}

/// R-CLIENT-4: `MemoryStorage`'s lock excludes, so two threads asking one
/// keystore for its device key at once agree on one key — the one kept.
#[test]
fn r_client_4_two_threads_on_one_memory_keystore_agree_on_one_device_key() {
    for _ in 0..50 {
        let store = Keystore::with(Arc::new(MemoryStorage::new()));
        let start = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (store, start) = (store.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    store.device_key().unwrap().id()
                })
            })
            .collect();
        let ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(ids[0], ids[1]);
        assert_eq!(store.device().unwrap().unwrap().id(), ids[0], "the key both were given is the one kept");
    }
}

/// R-CLIENT-4: a `MemoryStorage` slot's lock is held until its guard is
/// dropped, and only that slot's.
#[test]
fn r_client_4_a_memory_lock_holds_its_slot_until_dropped() {
    let storage = Arc::new(MemoryStorage::new());
    let held = storage.lock(Slot::Account).unwrap();
    drop(storage.lock(Slot::DeviceList).unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let other = storage.clone();
    let waiter = std::thread::spawn(move || {
        let _second = other.lock(Slot::Account).unwrap();
        tx.send(()).unwrap();
    });
    assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(), "taken while held");
    drop(held);
    rx.recv_timeout(std::time::Duration::from_secs(10)).expect("taken once dropped");
    waiter.join().unwrap();
}

/// R-CLIENT-4: storage that is not a key file krowk reads is named by the
/// storage's own words, with a line and column and never the value — and
/// `FileStorage` keeps today's file names, so a home's files do not move.
#[test]
fn r_client_4_a_slot_is_named_by_its_storage_and_files_keep_their_names() {
    let memory = Arc::new(MemoryStorage::new());
    // serde's own words would quote it: `invalid type: string "1234abcd"`.
    memory.write(Slot::Device, b"{\"version\": \"1234abcd\"}").unwrap();
    let err = Keystore::with(memory.clone()).device().unwrap_err();
    assert!(err.starts_with("the in-memory device key slot is not valid (line 1, column "), "{err}");
    assert!(!err.contains("1234abcd"), "{err}");
    memory.remove(Slot::Device).unwrap();
    memory.remove(Slot::Device).unwrap();
    assert!(Keystore::with(memory).device().unwrap().is_none());

    let names = [Slot::Device, Slot::Signing, Slot::Account, Slot::UserKeys, Slot::DeviceList].map(Slot::file_name);
    assert_eq!(names, ["device.json", "signing.json", "account-key.json", "user-keys.json", "device-list.json"]);
    let root = scratch("files");
    let files = FileStorage::new(&root);
    assert_eq!(files.path(Slot::Account), root.join("account-key.json"));
    assert_eq!(files.describe(Slot::Account), root.join("account-key.json").display().to_string());
    let store = Keystore::new(&root);
    assert_eq!(store.describe(Slot::Account), files.describe(Slot::Account));
    store.device_key().unwrap();
    assert!(root.join("device.json").is_file() && root.join("account-key.lock").is_file());
    let _ = std::fs::remove_dir_all(&root);
}

/// R-CLIENT-4, R-CLIENT-1: a keystore names a slot in its errors as its
/// storage does, so one with no files (the iPhone's Keychain, here memory)
/// never names a path — not for its own slots, not for another storage's
/// it reads, and not in what `holds` and `describe` say.
#[test]
fn r_client_4_r_client_1_a_memory_keystores_errors_name_the_slot_not_a_path() {
    let memory = Arc::new(MemoryStorage::new());
    let store = Keystore::with(memory.clone());
    store.device_key().unwrap();
    assert!(!store.holds(Slot::UserKeys).unwrap() && !store.holds(Slot::DeviceList).unwrap());
    memory.write(Slot::UserKeys, b"{\"version\": 2}").unwrap();
    memory.write(Slot::DeviceList, b"{\"version\": 1, \"entries\": []}").unwrap();
    assert!(store.holds(Slot::UserKeys).unwrap() && store.holds(Slot::DeviceList).unwrap());

    let aside = MemoryStorage::new();
    aside.write(Slot::UserKeys, b"{\"version\": 2}").unwrap();
    aside.write(Slot::DeviceList, b"not json").unwrap();
    for (err, slot) in [
        (store.user_keys().unwrap_err(), "the in-memory user keys slot"),
        (store.device_list().unwrap_err(), "the in-memory device list slot"),
        (store.user_keys_in(&aside).unwrap_err(), "the in-memory user keys slot"),
        (store.device_list_in(&aside).unwrap_err(), "the in-memory device list slot"),
    ] {
        assert!(err.starts_with(slot), "{err}");
        assert!(!err.contains('/'), "{err}");
    }
    for slot in [Slot::Device, Slot::Signing, Slot::Account, Slot::UserKeys, Slot::DeviceList] {
        assert!(!store.describe(slot).contains('/'), "{slot:?}");
    }
    assert!(store.user_keys_in(&MemoryStorage::new()).unwrap().is_none());
}

/// R-CLIENT-4: the same errors from a keystore over files name each file
/// by its full path — the terminal's next action is to move that file —
/// and a slot read from another home is named by that home's path, while
/// the device key that opens it is still this one's.
#[test]
fn r_client_4_a_file_keystores_errors_name_the_full_path() {
    let root = scratch("named");
    let (home, aside) = (root.join("home"), root.join("aside"));
    let store = Keystore::new(&home);
    store.device_key().unwrap();
    let g1 = UserKey::first();
    store.save_user_keys(&UserKeys::new(g1.clone(), []).unwrap()).unwrap();
    std::fs::create_dir_all(&aside).unwrap();
    std::fs::copy(home.join(Slot::UserKeys.file_name()), aside.join(Slot::UserKeys.file_name())).unwrap();
    let away = FileStorage::new(&aside);
    assert_eq!(store.user_keys_in(&away).unwrap().unwrap().newest(), &g1, "opened with this keystore's device key");
    let other = Keystore::new(&root.join("other"));
    other.device_key().unwrap();
    assert!(other.user_keys_in(&away).unwrap_err().contains("was wrapped for device"));

    std::fs::write(home.join(Slot::UserKeys.file_name()), "{\"version\": 2}").unwrap();
    std::fs::write(home.join(Slot::DeviceList.file_name()), "{\"version\": 1, \"entries\": []}").unwrap();
    std::fs::write(aside.join(Slot::UserKeys.file_name()), "{\"version\": 2}").unwrap();
    std::fs::write(aside.join(Slot::DeviceList.file_name()), "not json").unwrap();
    for (err, path) in [
        (store.user_keys().unwrap_err(), home.join("user-keys.json")),
        (store.device_list().unwrap_err(), home.join("device-list.json")),
        (store.user_keys_in(&away).unwrap_err(), aside.join("user-keys.json")),
        (store.device_list_in(&away).unwrap_err(), aside.join("device-list.json")),
    ] {
        assert!(path.is_absolute());
        assert!(err.starts_with(&path.display().to_string()), "{err}");
    }
    assert_eq!(store.describe(Slot::Device), home.join("device.json").display().to_string());
    assert!(store.holds(Slot::UserKeys).unwrap() && !Keystore::new(&root.join("none")).holds(Slot::UserKeys).unwrap());
    // A slot that cannot be read is an error naming its file, not "empty".
    std::fs::create_dir_all(root.join("dir").join(Slot::UserKeys.file_name())).unwrap();
    let err = Keystore::new(&root.join("dir")).holds(Slot::UserKeys).unwrap_err();
    assert!(err.starts_with(&root.join("dir").join("user-keys.json").display().to_string()), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}
