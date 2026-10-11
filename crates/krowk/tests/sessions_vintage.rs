//! Vintages end to end, against the stand-in registry: a native session
//! idle past 14 days leaves the machine for its week's vintage and stays
//! listed, a pinned one and a recent one stay, the registry holds only
//! ciphertext, and opening the archived session brings it back whole.

#![cfg(all(feature = "harness", unix))]

use krowk_client::e2e;
use krowk_client::keystore::Keystore;
use krowk_harness::log::{self, SessionLog};
use krowk_harness::protocol::{Item, LogBody, LogEvent};
use serde_json::Value;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;


const TOKEN: &str = "krowk_sk_vintage_000000000000000000000";
const DAY_MS: i64 = 86_400_000;

/// A native session whose every event is `age_days` old, asking `prompt`.
fn session(sessions: &Path, cwd: &Path, prompt: &str, age_days: i64) -> String {
    let (mut l, root) = SessionLog::create(sessions, cwd, "test").unwrap();
    l.append(LogBody::ItemCompleted { turn_id: "t".into(), item_id: "i".into(), item: Item::user(prompt) }).unwrap();
    l.append(LogBody::ItemCompleted { turn_id: "t".into(), item_id: "j".into(), item: Item::AssistantText { text: format!("answering {prompt}") } }).unwrap();
    drop(l);
    let path = sessions.join(&root.session_id).join(log::EVENTS_FILE);
    let shift = age_days * DAY_MS;
    let aged: String = log::read_events(&path)
        .unwrap()
        .into_iter()
        .map(|mut e: LogEvent| {
            e.time_ms -= shift;
            serde_json::to_string(&e).unwrap() + "\n"
        })
        .collect();
    std::fs::write(&path, aged).unwrap();
    root.session_id
}

struct Machine {
    home: PathBuf,
    api_url: String,
}

impl Machine {
    fn krowk(&self, args: &[&str]) -> (bool, Value, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(args)
            .arg("--json")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("TMPDIR", "/tmp")
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_API_URL", &self.api_url)
            .env("KROWK_TOKEN", TOKEN)
            .env("KROWK_DEVICE_NAME", "this machine")
            .env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1")
            .current_dir(&self.home)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let v = serde_json::from_str(&stdout).unwrap_or(Value::Null);
        (out.status.success(), v, format!("{stdout}{}", String::from_utf8_lossy(&out.stderr)))
    }
}

/// R-VINT-2 and R-VINT-3: the weekly job takes the idle session only, and
/// waits a week to run again; the bodies leave, and it stays listed, a
/// rebuild included.
fn archived_and_listed(m: &Machine, sessions: &Path, old: &str, pinned: &str, fresh: &str) {
    let (ok, v, out) = m.krowk(&["sessions", "archive", "--weekly"]);
    assert!(ok, "{out}");
    let archived: Vec<String> = v["data"]["archived"].as_array().into_iter().flatten().filter_map(|a| a["id"].as_str().map(str::to_owned)).collect();
    assert_eq!(archived, vec![old.to_owned()], "{out}");
    assert!(sessions.join(pinned).join(log::EVENTS_FILE).is_file(), "a pinned session is never archived");
    assert!(sessions.join(fresh).join(log::EVENTS_FILE).is_file(), "a recent one stays");
    let (ok, v, out) = m.krowk(&["sessions", "archive", "--weekly"]);
    assert!(ok && v["data"]["due"] == false, "the weekly job waits a week: {out}");

    // R-VINT-3: the bodies left; the index row did not.
    assert!(!sessions.join(old).join(log::EVENTS_FILE).exists() && !sessions.join(old).join(log::CONTEXT_FILE).exists());
    assert!(sessions.join(old).join(krowk_harness::vintage::STUB_FILE).is_file());
    let (ok, v, out) = m.krowk(&["sessions"]);
    assert!(ok, "{out}");
    let listed = v["data"]["sessions"].as_array().unwrap().iter().find(|s| s.to_string().contains(old)).cloned().unwrap_or_else(|| panic!("still listed: {out}"));
    assert!(listed.to_string().contains("secret prompt"), "under its title: {listed}");
    let (ok, _, out) = m.krowk(&["sessions", "rebuild", "--yes"]);
    assert!(ok, "{out}");
    let (_, v, out) = m.krowk(&["sessions"]);
    assert!(v.to_string().contains(old), "a rebuild keeps it listed from its stub: {out}");
}

/// The acceptance criteria, in order: a 14-day-old session is archived by
/// the weekly job, leaves local storage but stays listed, and restores
/// fully when opened; a pinned session is never archived; the vintage's
/// bytes on the server are ciphertext.
#[test]
fn r_vint_1_r_vint_2_r_vint_3_r_vint_4_an_idle_session_is_archived_listed_and_restored_on_open() {
    let root = std::env::temp_dir().join(format!("krowk-vintage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
    let m = Machine { home: root.clone(), api_url: format!("{}/v1", registry.url()) };

    // Ticket 41: this machine set sync up with `krowk sync init` and
    // nothing else — a user key and a device list, no account key.
    let kit = root.join("kit.txt");
    let (ok, _, out) = m.krowk(&["sync", "init", "--save", kit.to_str().unwrap()]);
    assert!(ok, "{out}");
    let ks = Keystore::new(&root.join(".krowk"));
    assert!(ks.account().unwrap().is_none(), "no account key: the clean break");
    let keys = ks.user_keys().unwrap().expect("a user key");
    let me = ks.device().unwrap().unwrap().id();
    let api = krowk_api::Client::new(&m.api_url, TOKEN).signed_by(e2e::DeviceSigner::new(me, ks.signing_key().unwrap()).shared());

    let sessions = root.join(".krowk").join("sessions");
    let old = session(&sessions, &root, "the idle session's secret prompt", 20);
    let pinned = session(&sessions, &root, "a pinned one", 30);
    let fresh = session(&sessions, &root, "a recent one", 3);
    // A context worth restoring byte for byte, as a turn records it.
    std::fs::write(sessions.join(&old).join(log::CONTEXT_FILE), "{\"system\":\"the old session's system prompt\",\"tools\":[]}\n").unwrap();
    let original = std::fs::read(sessions.join(&old).join(log::EVENTS_FILE)).unwrap();
    let original_context = std::fs::read(sessions.join(&old).join(log::CONTEXT_FILE)).unwrap();
    let (ok, _, out) = m.krowk(&["sessions", "import", "--from", "all"]);
    assert!(ok, "{out}");
    let (ok, _, out) = m.krowk(&["sessions", "pin", &pinned]);
    assert!(ok, "{out}");

    archived_and_listed(&m, &sessions, &old, &pinned, &fresh);

    // R-VINT-1: the registry holds the week's vintage, and only ciphertext.
    let stub = krowk_harness::vintage::read_stub(&sessions, &old).unwrap();
    let week = stub.week;
    let vintage = api.week_vintage(&week).unwrap().expect("the week's vintage");
    assert_eq!(stub.vintage, vintage.slug, "the stub names its vintage");
    let sealed = api.read_vintage(&vintage).unwrap();
    assert!(!sealed.windows(6).any(|w| w == b"secret"), "no plaintext on the server");
    assert_eq!(sealed[0], e2e::VINTAGE_V2, "sealed under the user key");
    let plain = krowk_harness::vintage::unpack(&e2e::open_vintage(&sealed, &week, &keys).unwrap()).unwrap();
    assert!(plain.contains_key(&old) && plain.len() == 1);
    let stranger = krowk_client::user_key::UserKeys::new(krowk_client::user_key::UserKey::first(), []).unwrap();
    assert!(e2e::open_vintage(&sealed, &week, &stranger).is_err(), "only the person's user key opens it");

    // R-VINT-4: opening it brings it back, byte for byte.
    let (ok, v, out) = m.krowk(&["sessions", "show", &old]);
    assert!(ok, "{out}");
    assert!(v.to_string().contains("answering the idle session"), "shown in full: {out}");
    assert_eq!(std::fs::read(sessions.join(&old).join(log::EVENTS_FILE)).unwrap(), original);
    assert_eq!(std::fs::read(sessions.join(&old).join(log::CONTEXT_FILE)).unwrap(), original_context, "and its context");
    assert!(!sessions.join(&old).join(krowk_harness::vintage::STUB_FILE).exists());

    // R-VINT-1: a second session last active in the same week joins the
    // week's vintage: the old one is read and merged, then replaced.
    let sibling = session(&sessions, &root, "a sibling from the same week", 20);
    let (ok, v, out) = m.krowk(&["sessions", "archive"]);
    assert!(ok, "{out}");
    assert!(v.to_string().contains(&sibling) && v.to_string().contains(&old), "{out}");
    let merged = api.week_vintage(&week).unwrap().expect("the week's vintage");
    assert_ne!(merged.slug, vintage.slug, "replaced");
    assert_eq!(api.read_vintage(&vintage).unwrap(), sealed, "the replaced vintage's bytes are kept");
    let plain = krowk_harness::vintage::unpack(&e2e::open_vintage(&api.read_vintage(&merged).unwrap(), &week, &keys).unwrap()).unwrap();
    assert!(plain.contains_key(&old) && plain.contains_key(&sibling), "one vintage holds the week's sessions");
    // A replacement that did not read the week's latest is refused.
    assert_eq!(api.put_vintage(&week, b"stale", Some(&vintage.slug)).unwrap_err().code(), "vintage_conflict");
    let _ = std::fs::remove_dir_all(&root);
}

/// A stand-in machine with sync set up at `root`, the registry it syncs
/// through, the registry client signed as it, and its user keys.
fn synced_machine(root: &Path) -> (krowk_devregistry::Running, Machine, krowk_api::Client, krowk_client::user_key::UserKeys) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
    let m = Machine { home: root.to_path_buf(), api_url: format!("{}/v1", registry.url()) };
    let (ok, _, out) = m.krowk(&["sync", "init", "--save", root.join("kit.txt").to_str().unwrap()]);
    assert!(ok, "{out}");
    let ks = Keystore::new(&root.join(".krowk"));
    let keys = ks.user_keys().unwrap().expect("a user key");
    let api = krowk_api::Client::new(&m.api_url, TOKEN).signed_by(e2e::DeviceSigner::new(ks.device().unwrap().unwrap().id(), ks.signing_key().unwrap()).shared());
    (registry, m, api, keys)
}

/// R-VINT-4: a restore reads the vintage the session went into, by the
/// slug its stub recorded, even once a replacement that lost it took the
/// week's place; a stub that recorded none falls back from the week's
/// current vintage to its replaced ones. `krowk sessions vintage` lists
/// both, and `--from` restores from the one named.
#[test]
fn r_vint_4_a_restore_finds_the_session_in_the_vintage_it_went_into_when_a_replacement_lost_it() {
    let root = std::env::temp_dir().join(format!("krowk-vintage-replaced-{}", std::process::id()));
    let (_registry, m, api, keys) = synced_machine(&root);
    let sessions = root.join(".krowk").join("sessions");
    let a = session(&sessions, &root, "the first of the week", 20);
    let b = session(&sessions, &root, "the second of the week", 20);
    let (ok, _, out) = m.krowk(&["sessions", "import", "--from", "all"]);
    assert!(ok, "{out}");
    let (ok, _, out) = m.krowk(&["sessions", "archive"]);
    assert!(ok, "{out}");
    let stub = krowk_harness::vintage::read_stub(&sessions, &a).unwrap();
    let (week, kept) = (stub.week.clone(), stub.vintage.clone());

    // Another machine's bad merge: a replacement holding neither session.
    let generation = Keystore::new(&root.join(".krowk")).device_list().unwrap().unwrap().generation();
    let empty = e2e::seal_vintage(&keys, generation, &week, &krowk_harness::vintage::pack(&Default::default())).unwrap();
    let lost = api.put_vintage(&week, &empty, Some(&kept)).unwrap().slug;
    // A stub from before stubs recorded their vintage.
    let b_stub = sessions.join(&b).join(krowk_harness::vintage::STUB_FILE);
    let mut unrecorded: Value = serde_json::from_slice(&std::fs::read(&b_stub).unwrap()).unwrap();
    unrecorded["vintage"] = Value::String(String::new());
    std::fs::write(&b_stub, serde_json::to_vec(&unrecorded).unwrap()).unwrap();

    let (ok, v, out) = m.krowk(&["sessions", "vintage", &a]);
    assert!(ok, "{out}");
    let listed: Vec<(String, bool, bool)> = v["data"]["vintages"].as_array().unwrap().iter().map(|r| (r["slug"].as_str().unwrap().to_owned(), r["current"] == true, r["recorded"] == true)).collect();
    assert_eq!(listed, vec![(lost.clone(), true, false), (kept.clone(), false, true)], "the current one, then the one it replaced, which the stub names: {out}");
    let (ok, v, out) = m.krowk(&["sessions", "vintage"]);
    assert!(ok && v["data"]["vintages"].as_array().unwrap().len() == 2, "every week's, replaced ones included: {out}");

    let (ok, v, out) = m.krowk(&["sessions", "restore", &a]);
    assert!(ok, "{out}");
    assert_eq!(v["data"]["vintage"], kept.as_str(), "from the vintage it went into: {out}");
    assert_eq!(v["data"]["warnings"], serde_json::json!([]), "the recorded one, so nothing to say: {out}");
    assert!(sessions.join(&a).join(log::EVENTS_FILE).is_file());

    let (ok, _, out) = m.krowk(&["sessions", "restore", &b, "--from", &lost]);
    assert!(!ok && out.contains("does not hold session"), "the vintage named is the one read: {out}");
    let (ok, v, out) = m.krowk(&["sessions", "restore", &b]);
    assert!(ok, "{out}");
    assert_eq!(v["data"]["vintage"], kept.as_str(), "the week's current one lost it, so a replaced one is read: {out}");
    let warned = v["data"]["warnings"].to_string();
    assert!(warned.contains(&format!("not {lost}")) && warned.contains("does not hold session"), "and it says which it passed over, and why: {out}");
    assert!(sessions.join(&b).join(log::EVENTS_FILE).is_file());

    // The registry's restoration: the replaced vintage is the week's again,
    // the one that lost the sessions kept as replaced, and a current one
    // has nothing to be restored over.
    assert_eq!(api.restore_vintage(&kept).unwrap().slug, kept);
    assert_eq!(api.week_vintage(&week).unwrap().unwrap().slug, kept);
    assert_eq!(api.replaced_vintages(&week).unwrap().into_iter().map(|v| v.slug).collect::<Vec<_>>(), vec![lost]);
    assert_eq!(api.restore_vintage(&kept).unwrap_err().code(), "vintage_conflict");
    let _ = std::fs::remove_dir_all(&root);
}

/// R-VINT-3: on a machine without sync, opening an archived session shows
/// what its stub keeps, and says the rest needs sync, rather than failing.
#[test]
fn r_vint_3_an_archived_session_opened_without_sync_shows_its_stub_and_says_why() {
    let root = std::env::temp_dir().join(format!("krowk-vintage-stub-only-{}", std::process::id()));
    let (_registry, m, _, _) = synced_machine(&root);
    let sessions = root.join(".krowk").join("sessions");
    let id = session(&sessions, &root, "asked on the synced machine", 20);
    let (ok, _, out) = m.krowk(&["sessions", "archive"]);
    assert!(ok, "{out}");

    // Its stub, on a machine that never set sync up.
    let bare = Machine { home: root.join("bare"), api_url: m.api_url.clone() };
    let dir = bare.home.join(".krowk").join("sessions").join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(sessions.join(&id).join(krowk_harness::vintage::STUB_FILE), dir.join(krowk_harness::vintage::STUB_FILE)).unwrap();
    let (ok, _, out) = bare.krowk(&["sessions", "import", "--from", "all"]);
    assert!(ok, "{out}");
    let (ok, v, out) = bare.krowk(&["sessions", "show", &id]);
    assert!(ok, "shown, not refused: {out}");
    assert_eq!(v["data"]["archived"]["restored"], false, "{out}");
    assert_eq!(v["data"]["archived"]["error"]["code"], "not_set_up", "{out}");
    assert!(v["data"]["archived"]["summary"].as_str().unwrap().contains("asked on the synced machine"), "{out}");
    assert!(dir.join(krowk_harness::vintage::STUB_FILE).is_file() && !dir.join(log::EVENTS_FILE).exists(), "left archived");
    let _ = std::fs::remove_dir_all(&root);
}

/// R-VINT-4: a copy older than the stub records — a replacement lost the
/// newer one, or a registry rolled the week back — is never restored by
/// itself, and opening the session fails rather than showing the stub past
/// it. Named with `--from`, it is restored, and said to be older.
#[test]
fn r_vint_4_a_copy_older_than_the_stub_is_refused_unless_named() {
    let root = std::env::temp_dir().join(format!("krowk-vintage-stale-{}", std::process::id()));
    let (_registry, m, _, _) = synced_machine(&root);
    let sessions = root.join(".krowk").join("sessions");
    let id = session(&sessions, &root, "continued after it was restored", 20);
    let (ok, _, out) = m.krowk(&["sessions", "import", "--from", "all"]);
    assert!(ok, "{out}");
    let (ok, _, out) = m.krowk(&["sessions", "archive"]);
    assert!(ok, "{out}");
    let old = krowk_harness::vintage::read_stub(&sessions, &id).unwrap().vintage;
    // As if it was restored, continued a day, and archived again into a
    // vintage the registry no longer serves.
    let stub_path = sessions.join(&id).join(krowk_harness::vintage::STUB_FILE);
    let mut newer: Value = serde_json::from_slice(&std::fs::read(&stub_path).unwrap()).unwrap();
    newer["lastActiveMs"] = Value::from(newer["lastActiveMs"].as_i64().unwrap() + DAY_MS);
    newer["vintage"] = Value::String("art_gone".into());
    std::fs::write(&stub_path, serde_json::to_vec(&newer).unwrap()).unwrap();

    let (ok, _, out) = m.krowk(&["sessions", "restore", &id]);
    assert!(!ok && out.contains("older copy") && out.contains(&old), "refused, naming the vintage and what is older: {out}");
    assert!(out.contains(&format!("krowk sessions restore {id} --from {old}")), "and how to restore it anyway: {out}");
    let (ok, _, out) = m.krowk(&["sessions", "show", &id]);
    assert!(!ok && out.contains("\"restore_failed\""), "not shown past as a stub: {out}");
    assert!(stub_path.is_file() && !sessions.join(&id).join(log::EVENTS_FILE).exists(), "left archived");

    let (ok, v, out) = m.krowk(&["sessions", "restore", &id, "--from", &old]);
    assert!(ok, "{out}");
    assert!(v["data"]["warnings"].to_string().contains("older copy"), "restored as asked, and said to be older: {out}");
    let _ = std::fs::remove_dir_all(&root);
}
