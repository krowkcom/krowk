//! Setting sync up, getting back in and replacing the kit, end to end on
//! scratch homes against the stand-in registry (D6; canon,
//! engineering/devices.md → Recovery, Replacing the kit, Starting over):
//! `krowk sync init` with the kit saved to a file, a start-over that keeps
//! the sessions this device can open, `krowk sync recovery new` with the
//! old kit's words, and `krowk sync recover` on a new machine.

#![cfg(all(feature = "harness", unix))]

use krowk_client::e2e::{self, DeviceSigner, SessionKey};
use krowk_client::keystore::Keystore;
use krowk_client::session_record::{self, Signer};
use serde_json::Value;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// One person's keys (`person_for` reads the part before `#`).
const LAPTOP: &str = "krowk_sk_kit#laptop";
const DESKTOP: &str = "krowk_sk_kit#desktop";
/// The account key id the stand-in's session calls register devices under.
const ACCOUNT: &str = "00112233445566778899aabbccddeeff";

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-recovery-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["laptop", "desktop"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    root.canonicalize().unwrap()
}

fn registry() -> (krowk_devregistry::Running, String) {
    let r = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", r.url());
    (r, api)
}

/// krowk with `home` as HOME and nothing else of this machine's, the debug
/// build's stand-in for the person at the terminal answering `answers` in
/// turn, and `input` on stdin.
fn krowk(home: &Path, api: &str, token: &str, args: &[&str], answers: &str, input: &str) -> Output {
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
        .env("KROWK_TEST_ANSWERS", answers)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn ok(out: &Output) -> Value {
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn keys(home: &Path) -> Keystore {
    Keystore::new(&home.join(".krowk"))
}

/// `sync init` on `home`, the kit written to `kit` (0600, never stdout).
fn init(home: &Path, api: &str, token: &str, kit: &Path) -> String {
    let out = krowk(home, api, token, &["sync", "init", "--save", kit.to_str().unwrap(), "--json"], "", "");
    let v = ok(&out);
    assert_eq!(v["data"]["recovery_kit"], true, "{v}");
    let words = std::fs::read_to_string(kit).unwrap().trim().to_string();
    assert_eq!(words.split(' ').count(), 12);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(kit).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&words) && !String::from_utf8_lossy(&out.stderr).contains(&words), "the kit is in the file alone");
    words
}

/// A session `home` publishes as its host would: its key sealed under the
/// user key it holds, the record signed by it.
fn publish(home: &Path, api: &str, token: &str, id: &str) -> SessionKey {
    let ks = keys(home);
    let (device, signing) = (ks.device().unwrap().unwrap(), ks.signing_key().unwrap());
    let user = ks.user_keys().unwrap().unwrap();
    let raw = krowk_harness::daemon::ws::uuid(id);
    let key = SessionKey::generate();
    let sealed = e2e::seal_session_key(&key, &raw, &user, user.newest().generation()).unwrap();
    let signature = session_record::sign(&raw, &sealed, session_record::SEAL_USER, user.newest(), &signing).unwrap();
    let signing_public = e2e::hex(&signing.public().0);
    let client = krowk_api::Client::new(api, token).signed_by(DeviceSigner::new(device.id(), signing).shared());
    // The stand-in's session calls still check signatures against the
    // devices registered the account-key way.
    client.register_device(&e2e::hex(&device.public().0), &signing_public, "laptop", ACCOUNT).unwrap();
    client.put_sync_session(id, &e2e::hex(&sealed), Some((&e2e::hex(&signature), &device.id().to_string())), None, None).unwrap();
    key
}

/// The key the session was published with, as `home` opens it now (the
/// record checked against the list it keeps, the ring opened with the user
/// keys it holds), and how many key epochs it has had since.
fn open_ring(home: &Path, api: &str, token: &str, id: &str) -> Result<(SessionKey, u32), String> {
    let ks = keys(home);
    let (user, chain) = (ks.user_keys().unwrap().unwrap(), ks.device_list().unwrap().unwrap());
    let s = krowk_api::Client::new(api, token).show_sync_session(id).map_err(|e| e.code())?;
    krowk_harness::sync::store::open_session_key(&s, id, &user, &chain, Signer::EverHeld).map(|k| (k.at(0).expect("epoch 0").clone(), k.epoch()))
}

fn open(home: &Path, api: &str, token: &str, id: &str) -> Result<SessionKey, String> {
    open_ring(home, api, token, id).map(|(k, _)| k)
}

/// D6 (C1 of #204's review): a start-over from a device that holds the
/// user key keeps its device key and brings its sessions along — one it
/// published itself included — sealed again under the new list's
/// generation 1, and the old keys stay aside until the person drops them.
#[test]
fn d6_a_start_over_seals_this_devices_sessions_again_under_the_new_list() {
    let (_r, api) = registry();
    let r = root("start-over");
    let laptop = r.join("laptop");
    init(&laptop, &api, LAPTOP, &r.join("kit-1"));
    let device = keys(&laptop).device().unwrap().unwrap().id();
    let id = "01a0ec7b-6666-7000-8000-000000000061";
    let key = publish(&laptop, &api, LAPTOP, id);
    let old_root = keys(&laptop).device_list().unwrap().unwrap().root();

    let out = krowk(&laptop, &api, LAPTOP, &["sync", "init", "--start-over", "--save", r.join("kit-2").to_str().unwrap(), "--json"], "y", "");
    let v = ok(&out);
    assert_eq!((v["data"]["started_over"].clone(), v["data"]["resealed"].clone(), v["data"]["left"].clone()), (true.into(), 1.into(), 0.into()), "{v}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("come along"), "the prompt says the sessions come along");

    let ks = keys(&laptop);
    assert_eq!(ks.device().unwrap().unwrap().id(), device, "the device key is kept");
    assert_ne!(ks.device_list().unwrap().unwrap().root(), old_root, "a new list");
    assert_eq!(ks.user_keys().unwrap().unwrap().newest().generation(), 1);
    let (first, epoch) = open_ring(&laptop, &api, LAPTOP, id).unwrap();
    assert_eq!((first.as_bytes(), epoch), (key.as_bytes(), 1), "the old session opens under the new list, a key epoch on");
    assert!(laptop.join(".krowk/before-start-over").is_dir(), "the old keys stay aside");

    // Run again, it goes through what is left: nothing.
    let again = ok(&krowk(&laptop, &api, LAPTOP, &["sync", "init", "--start-over", "--json"], "", ""));
    assert_eq!((again["data"]["resealed"].clone(), again["data"]["left"].clone()), (0.into(), 0.into()), "{again}");
    // And the aside goes only when the person says so.
    let dropped = ok(&krowk(&laptop, &api, LAPTOP, &["sync", "recovery", "discard-old", "--json"], "y", ""));
    assert_eq!((dropped["data"]["discarded"].clone(), dropped["data"]["left"].clone()), (true.into(), 0.into()), "{dropped}");
    assert!(!laptop.join(".krowk/before-start-over").exists());
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: with a kit on the list, `recovery new` takes the old kit's words and
/// replaces it at once — posted as the old kit, on this machine's own key
/// — and the key rotates.
#[test]
fn d6_recovery_new_with_the_old_kit_replaces_it_at_once() {
    let (_r, api) = registry();
    let r = root("kit-new");
    let laptop = r.join("laptop");
    let old = init(&laptop, &api, LAPTOP, &r.join("kit-1"));
    let v = ok(&krowk(&laptop, &api, LAPTOP, &["sync", "recovery", "new", "--save", r.join("kit-2").to_str().unwrap(), "--json"], "", &format!("{old}\n")));
    assert_eq!(v["data"]["generation"], 2, "{v}");
    let new = std::fs::read_to_string(r.join("kit-2")).unwrap();
    ok(&krowk(&laptop, &api, LAPTOP, &["sync", "recovery", "check", "--json"], "", &new));
    let refused = krowk(&laptop, &api, LAPTOP, &["sync", "recovery", "check", "--json"], "", &format!("{old}\n"));
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("kit_not_on_list"));
    // The device still syncs, on a key that speaks for it.
    let status = ok(&krowk(&laptop, &api, LAPTOP, &["sync", "status", "--json"], "", ""));
    assert_eq!((status["data"]["generation"].clone(), status["data"]["recovery_kit"].clone()), (2.into(), true.into()), "{status}");
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: a new machine gets back in from the kit's words, piped in: the list
/// verified from seq 0, every device on it kept or removed by the person,
/// and only then the key — which opens what the lost machine published.
#[test]
fn d6_recover_on_a_new_machine_keeps_or_removes_each_device_and_opens_the_old_sessions() {
    let (_r, api) = registry();
    let r = root("recover");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let words = init(&laptop, &api, LAPTOP, &r.join("kit"));
    let id = "01a0ec7b-6666-7000-8000-000000000062";
    let key = publish(&laptop, &api, LAPTOP, id);

    // The laptop is lost: removed in the review.
    let v = ok(&krowk(&desktop, &api, DESKTOP, &["sync", "recover", "--json"], "n", &format!("{words}\n")));
    assert_eq!((v["data"]["removed"].clone(), v["data"]["generation"].clone(), v["data"]["kept"].clone()), (1.into(), 2.into(), serde_json::json!([])), "{v}");
    let chain = keys(&desktop).device_list().unwrap().unwrap();
    let names: Vec<_> = chain.devices().iter().map(|d| d.name.clone()).collect();
    assert!(names.contains(&"desktop".to_string()) && !names.contains(&"laptop".to_string()), "{names:?}");
    assert_eq!(open(&desktop, &api, DESKTOP, id).unwrap().as_bytes(), key.as_bytes(), "the lost machine's session opens here");
    // And the removed laptop is refused.
    let refused = krowk(&laptop, &api, LAPTOP, &["sync", "status", "--json"], "", "");
    assert!(!refused.status.success(), "{}", String::from_utf8_lossy(&refused.stdout));
    let _ = std::fs::remove_dir_all(&r);
}

/// D7: `devices remove` takes a device off the list and rotates the key
/// away from it, so a session sealed afterwards does not open with what
/// the removed device holds — and the registry refuses it besides.
#[test]
fn d7_a_removed_device_cannot_open_a_session_sealed_after_its_removal() {
    let (_r, api) = registry();
    let r = root("remove");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let words = init(&laptop, &api, LAPTOP, &r.join("kit"));
    let back = ok(&krowk(&desktop, &api, DESKTOP, &["sync", "recover", "--json"], "y", &format!("{words}\n")));
    assert_eq!(back["data"]["kept"], serde_json::json!(["laptop"]), "{back}");
    let held = keys(&desktop).user_keys().unwrap().unwrap();
    // The laptop takes the desktop's list in, then removes it.
    ok(&krowk(&laptop, &api, LAPTOP, &["sync", "status", "--json"], "", ""));
    let v = ok(&krowk(&laptop, &api, LAPTOP, &["devices", "remove", "desktop", "--json"], "y", ""));
    assert_eq!(v["data"]["generation"], 2, "{v}");

    let id = "01a0ec7b-6666-7000-8000-000000000071";
    let key = publish(&laptop, &api, LAPTOP, id);
    assert_eq!(open(&laptop, &api, LAPTOP, id).unwrap().as_bytes(), key.as_bytes());
    let s = krowk_api::Client::new(&api, LAPTOP).show_sync_session(id).unwrap();
    let wrapped = e2e::unhex(&s.wrapped_key).unwrap();
    assert!(e2e::unwrap_session_key(&wrapped, &krowk_harness::daemon::ws::uuid(id), &held).is_err(), "the desktop's keys do not open it");
    let refused = krowk(&desktop, &api, DESKTOP, &["sync", "status", "--json"], "", "");
    assert!(!refused.status.success(), "and the desktop is off the list: {}", String::from_utf8_lossy(&refused.stdout));
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: a kit skipped at [s] is warned about once, where the person decides
/// — the summary after it does not say it again.
#[test]
fn d6_a_skipped_kit_is_warned_about_once() {
    let (_r, api) = registry();
    let r = root("skip");
    let out = krowk(&r.join("laptop"), &api, LAPTOP, &["sync", "init"], "s", "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(said.matches("no recovery kit").count(), 1, "{said}");
    assert!(said.contains("sync is set up"), "{said}");
    let _ = std::fs::remove_dir_all(&r);
}
