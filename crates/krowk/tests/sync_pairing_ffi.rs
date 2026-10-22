//! The phone joins a pairing the terminal shows, end to end against the
//! stand-in registry (R-E2E-3, R-CLIENT-1; canon, engineering/devices.md →
//! Adding a device): the laptop, already on the person's device list, runs
//! the real `krowk devices add`; the phone is krowk-ffi's `Account`, as the
//! iPhone app calls it, with its Keychain in memory. It ends up holding the
//! person's user key and a pin of the chain that adds it, through the same
//! `pairing_join` the terminal's `krowk sync join` runs.
//!
//! The laptop's list is made here with krowk-client, as `krowk sync init`
//! makes it, so this tests pairing alone. The few helpers are
//! `sync_pairing.rs`'s.

#![cfg(all(feature = "harness", unix))]

use krowk_client::device_chain::{Chain, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceSigner};
use krowk_client::keystore::{self, Keystore, Slot, StorageLock};
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::{UserKey, UserKeys};
use krowk_ffi::{Account, KeyStorage, KrowkError};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One person's laptop and phone: two keys, one person (the stand-in's
/// person is the token up to `#`).
const LAPTOP: &str = "krowk_sk_pairing#laptop";
const PHONE: &str = "krowk_sk_pairing#phone";

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-pair-ffi-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("laptop")).unwrap();
    root.canonicalize().unwrap()
}

fn now() -> u64 {
    jiff::Timestamp::now().as_second() as u64
}

/// krowk with `home` as HOME and nothing else of this machine's, with the
/// debug build's stand-in for the person at the terminal on.
fn krowk(home: &Path, api: &str, token: &str, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", api)
        .env("KROWK_TOKEN", token)
        .env("KROWK_DEVICE_NAME", home.file_name().unwrap())
        .env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1")
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

/// The laptop set up as `sync init` sets a device up: its keys, the chain's
/// seq 0 with a recovery kit, generation 1 of the user key, and the pin.
fn set_up(home: &Path, api: &str) -> UserKey {
    let store = Keystore::new(&home.join(".krowk"));
    let device = store.device_key().unwrap();
    let signing = store.signing_key().unwrap();
    let kit = RecoveryKit::generate().device();
    let me = Subject { kind: Kind::Device, name: "laptop".into(), os: "Linux".into(), device: device.public(), signing: signing.public() };
    let recovery = Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: kit.key.public(), signing: kit.signing.public() };
    let (_, batch) = Chain::start(me, &signing, Some((recovery, &kit.signing)), now()).unwrap();
    let post = krowk_api::sync::ListPost {
        entries: batch.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: vec![],
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over: false,
    };
    let client = krowk_api::Client::new(api, LAPTOP).signed_by(DeviceSigner::new(device.id(), store.signing_key().unwrap()).shared());
    client.init_device_list(&post).unwrap();
    store.save_user_keys(&UserKeys::new(batch.newest.clone(), []).unwrap()).unwrap();
    store.save_device_list(&batch.entries).unwrap();
    batch.newest
}

/// The registry's chain, verified from seq 0.
fn served(api: &str) -> Chain {
    let list = krowk_api::Client::new(api, PHONE).device_list_all().unwrap();
    let entries: Vec<SignedEntry> = list.entries.iter().map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).unwrap(), &e2e::unhex(&e.signatures).unwrap()).unwrap()).collect();
    Chain::verify(&entries, None).unwrap()
}

/// Every stderr line of a child, as it comes.
fn lines(child: &mut Child) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    rx
}

/// The code `devices add` shows, `XXXX-XXXX`.
fn code_shown(rx: &std::sync::mpsc::Receiver<String>) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(code) = line.trim().strip_prefix("Code: ") {
                return code.split_whitespace().next().unwrap().to_string();
            }
            seen.push_str(&line);
            seen.push('\n');
        }
    }
    panic!("`krowk devices add` showed no code:\n{seen}");
}

fn rest(rx: &std::sync::mpsc::Receiver<String>) -> String {
    rx.try_iter().collect::<Vec<_>>().join("\n")
}

/// A `krowk devices add` that a failing test leaves neither stopped nor
/// running: on a panic its process is continued and killed.
struct Reaped(u32);

impl Drop for Reaped {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let pid = self.0.to_string();
            for sig in ["-CONT", "-KILL"] {
                let _ = Command::new("kill").args([sig, &pid]).status();
            }
        }
    }
}

/// The laptop's `krowk devices add`, its stderr lines, and the code it shows.
fn add(laptop: &Path, api: &str) -> (Child, Reaped, std::sync::mpsc::Receiver<String>, String) {
    let mut a = krowk(laptop, api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let reaped = Reaped(a.id());
    let a_err = lines(&mut a);
    let code = code_shown(&a_err);
    (a, reaped, a_err, code)
}

fn wait(mut child: Child, within: Duration) -> std::process::Output {
    let deadline = Instant::now() + within;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("a krowk did not finish in {within:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

/// The phone's Keychain, in memory.
#[derive(Default)]
struct Memory(Mutex<HashMap<String, Vec<u8>>>);

impl KeyStorage for Memory {
    fn read(&self, slot: String) -> Result<Option<Vec<u8>>, KrowkError> {
        Ok(self.0.lock().unwrap().get(&slot).cloned())
    }
    fn write(&self, slot: String, bytes: Vec<u8>) -> Result<(), KrowkError> {
        self.0.lock().unwrap().insert(slot, bytes);
        Ok(())
    }
    fn remove(&self, slot: String) -> Result<(), KrowkError> {
        self.0.lock().unwrap().remove(&slot);
        Ok(())
    }
}

/// The same Keychain as krowk-client's keystore reads it, to see what the
/// join left there.
struct Bridge(Arc<Memory>);

impl keystore::KeyStorage for Bridge {
    fn read(&self, slot: Slot) -> Result<Option<krowk_client::Zeroizing<Vec<u8>>>, String> {
        Ok(self.0.0.lock().unwrap().get(slot.file_name()).cloned().map(krowk_client::Zeroizing::new))
    }
    fn write(&self, slot: Slot, bytes: &[u8]) -> Result<(), String> {
        self.0.0.lock().unwrap().insert(slot.file_name().into(), bytes.to_vec());
        Ok(())
    }
    fn remove(&self, slot: Slot) -> Result<(), String> {
        self.0.0.lock().unwrap().remove(slot.file_name());
        Ok(())
    }
    fn lock(&self, _: Slot) -> Result<StorageLock, String> {
        Ok(StorageLock::new(()))
    }
    fn describe(&self, slot: Slot) -> String {
        slot.file_name().into()
    }
}

/// A phone signed in with the phone's key, as sign-in leaves it.
fn phone(api: &str) -> (Arc<Account>, Arc<Memory>) {
    let storage = Arc::new(Memory::default());
    let creds = serde_json::json!({"version": 1, "token": PHONE, "key_id": "key_phone", "workspace": "ws", "workspace_name": "Workspace"});
    storage.write("credentials.json".into(), serde_json::to_vec(&creds).unwrap()).unwrap();
    (Account::open(api.into(), storage.clone()).unwrap(), storage)
}

fn refusal(r: Result<krowk_ffi::Joined, KrowkError>) -> (String, String) {
    match r {
        Err(KrowkError::Refused { code, detail }) => (code, detail),
        other => panic!("not a refusal: {other:?}"),
    }
}

fn registry() -> (krowk_devregistry::Running, String) {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    (registry, api)
}

/// R-E2E-3, R-CLIENT-1: the phone joins with the code the terminal shows,
/// and holds the user key and a pin of the chain whose last entry adds it,
/// as an iPhone; the laptop lists it.
#[test]
fn r_e2e_3_the_phone_joins_with_the_terminals_code_and_holds_the_user_key_and_the_pin() {
    let (_registry, api) = registry();
    let r = root("join");
    let laptop = r.join("laptop");
    let key = set_up(&laptop, &api);
    let (account, storage) = phone(&api);
    assert!(!account.has_user_keys().unwrap());

    let (a, _reaped, a_err, code) = add(&laptop, &api);
    let joined = account.join_pairing(code, "Test iPhone".into()).unwrap();
    assert_eq!(joined.device_name, "Test iPhone");
    let a = wait(a, Duration::from_secs(30));
    let said = rest(&a_err);
    assert!(a.status.success(), "{}\n{said}", String::from_utf8_lossy(&a.stdout));
    assert!(said.contains("Add 'Test iPhone' (iOS) to your devices? [Y/n]"), "{said}");

    let store = Keystore::with(Arc::new(Bridge(storage)));
    assert!(account.has_user_keys().unwrap());
    assert_eq!(*store.user_keys().unwrap().unwrap().newest(), key, "the phone holds the laptop's user key");
    let pinned = store.device_list().unwrap().expect("the phone pins the list");
    assert_eq!(pinned.head(), served(&api).head(), "pinned at the head that adds it");
    assert_eq!(joined.seq, pinned.head().seq);
    let added = pinned.devices().iter().find(|d| d.added == joined.seq).expect("the last entry adds a device");
    assert_eq!((added.id().to_string(), added.name.as_str(), added.os.as_str()), (account.device_id(), "Test iPhone", "iOS"));

    let list = krowk(&laptop, &api, LAPTOP, &["devices"]).output().unwrap();
    let out = String::from_utf8_lossy(&list.stdout);
    assert!(list.status.success() && out.contains("Test iPhone"), "{out}{}", String::from_utf8_lossy(&list.stderr));

    // Once joined, a second join is refused before anything is sent.
    assert_eq!(refusal(account.join_pairing("K7QF-9M3X".into(), "Test iPhone".into())).0, "already_joined");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-E2E-3: a wrong code adds nothing and leaves the phone holding nothing;
/// it says to ask for a new code, and never tries again.
#[test]
fn r_e2e_3_a_wrong_code_adds_nothing_and_the_phone_keeps_no_user_key() {
    let (_registry, api) = registry();
    let r = root("wrong");
    let laptop = r.join("laptop");
    set_up(&laptop, &api);
    let (account, storage) = phone(&api);

    let (a, _reaped, a_err, code) = add(&laptop, &api);
    let wrong = if code.starts_with('2') { "3" } else { "2" }.to_string() + &code[1..];
    let (code, detail) = refusal(account.join_pairing(wrong, "Test iPhone".into()));
    assert_eq!(code, "pairing_failed");
    assert!(detail.contains("a code works once") && detail.contains("krowk devices add"), "{detail}");
    let a = wait(a, Duration::from_secs(30));
    let said = rest(&a_err);
    assert!(!a.status.success() && !said.contains("[Y/n]"), "the laptop asked nothing: {said}");

    assert_eq!(served(&api).head().seq, 0, "nothing reached the chain");
    assert!(!account.has_user_keys().unwrap());
    let store = Keystore::with(Arc::new(Bridge(storage)));
    assert!(store.user_keys().unwrap().is_none() && store.device_list().unwrap().is_none(), "the phone kept nothing");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-E2E-3: a malformed code is refused before anything is sent, and with
/// no pairing open the phone is told where to start one.
#[test]
fn r_e2e_3_a_malformed_code_sends_nothing_and_no_open_pairing_says_where_to_open_one() {
    let (_registry, api) = registry();
    let r = root("none");
    set_up(&r.join("laptop"), &api);
    let (account, _) = phone(&api);
    assert!(matches!(account.join_pairing("K7QF-9M3".into(), "Test iPhone".into()), Err(KrowkError::InvalidInput { .. })));
    let (code, detail) = refusal(account.join_pairing("K7QF-9M3X".into(), "Test iPhone".into()));
    assert_eq!(code, "no_pairing");
    assert!(detail.contains("krowk devices add"), "{detail}");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-E2E-3: cancelled from another thread while the laptop is held still
/// (stopped, so it never answers), the join ends within a second, the
/// pairing is ended on the registry, and the phone keeps nothing.
#[test]
fn r_e2e_3_a_join_cancelled_while_the_other_device_waits_ends_within_a_second() {
    let (_registry, api) = registry();
    let r = root("cancel");
    let laptop = r.join("laptop");
    set_up(&laptop, &api);
    let (account, _) = phone(&api);

    let (a, _reaped, a_err, code) = add(&laptop, &api);
    let pid = a.id().to_string();
    let signal = |sig: &str| assert!(Command::new("kill").args([sig, &pid]).status().unwrap().success());
    signal("-STOP");

    let joining = account.clone();
    let join = std::thread::spawn(move || joining.join_pairing(code, "Test iPhone".into()));
    // The phone's hello is out, and it waits for an answer that won't come.
    let looked = krowk_api::Client::new(&api, PHONE);
    let deadline = Instant::now() + Duration::from_secs(10);
    while looked.find_open_pairing().map(|p| p.joiner_message.is_empty()).unwrap_or(true) {
        assert!(Instant::now() < deadline, "the phone never joined");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A second join on the same account meanwhile is refused, not sent.
    assert_eq!(refusal(account.join_pairing("K7QF-9M3X".into(), "Test iPhone".into())).0, "pairing_busy");

    let asked = Instant::now();
    account.cancel_pairing();
    let (code, _) = refusal(join.join().unwrap());
    assert!(asked.elapsed() < Duration::from_secs(1), "the cancel took {:?}", asked.elapsed());
    assert_eq!(code, "cancelled");
    assert!(!account.has_user_keys().unwrap());
    let ended = looked.find_open_pairing().map(|p| p.state);
    assert!(matches!(&ended, Err(e) if e.code() == "pairing_gone") || matches!(&ended, Ok(s) if s == "dead"), "the pairing was ended: {ended:?}");

    signal("-CONT");
    let a = wait(a, Duration::from_secs(30));
    assert!(!a.status.success(), "the laptop's pairing is over: {}", rest(&a_err));
    assert_eq!(served(&api).head().seq, 0, "nothing reached the chain");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-E2E-3: a scanned invite is checked before its code is spent. One for
/// another pairing is refused with nothing sent, and the pairing it
/// names stays open for the right one, which joins.
#[test]
fn r_e2e_3_a_scanned_invite_for_another_pairing_spends_nothing_and_the_right_one_joins() {
    let (_registry, api) = registry();
    let r = root("invite");
    let laptop = r.join("laptop");
    set_up(&laptop, &api);
    let (account, _) = phone(&api);
    let user_id = krowk_api::Client::new(&api, PHONE).verify_key().unwrap().user_id;
    let laptop_device = Keystore::new(&laptop.join(".krowk")).device_key().unwrap().id().to_string();

    let (a, _reaped, a_err, code) = add(&laptop, &api);
    let invite = |code: &str, user: &str, device: &str| krowk_ffi::PairingInvite { code: code.into(), user_id: user.into(), device: device.into() };
    let other_device = "0123456789abcdef0123456789abcdef";
    for wrong in [invite(&code, &user_id, other_device), invite(&code, "someone-else", &laptop_device)] {
        assert_eq!(refusal(account.join_invite(wrong, "Test iPhone".into())).0, "invite_mismatch");
    }
    let open = krowk_api::Client::new(&api, PHONE).find_open_pairing().unwrap();
    assert!(open.state == "open" && open.joiner_message.is_empty(), "nothing was sent: {open:?}");

    let joined = account.join_invite(invite(&code, &user_id, &laptop_device), "Test iPhone".into()).unwrap();
    assert_eq!(joined.device_name, "Test iPhone");
    let a = wait(a, Duration::from_secs(30));
    assert!(a.status.success(), "{}", rest(&a_err));
    assert!(account.has_user_keys().unwrap());
    let _ = std::fs::remove_dir_all(&r);
}
