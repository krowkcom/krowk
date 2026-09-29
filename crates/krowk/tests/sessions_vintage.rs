//! Vintages end to end, against the stand-in registry: a native session
//! idle past 14 days leaves the machine for its week's vintage and stays
//! listed, a pinned one and a recent one stay, the registry holds only
//! ciphertext, and opening the archived session brings it back whole.

#![cfg(all(feature = "harness", unix))]

use krowk_client::e2e::{self, AccountKey, SigningKey};
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
    l.append(LogBody::ItemCompleted { turn_id: "t".into(), item_id: "i".into(), item: Item::UserText { text: prompt.into() } }).unwrap();
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
            .current_dir(&self.home)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let v = serde_json::from_str(&stdout).unwrap_or(Value::Null);
        (out.status.success(), v, format!("{stdout}{}", String::from_utf8_lossy(&out.stderr)))
    }
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

    // This machine has joined sync: an account key, its own device, registered.
    let account = AccountKey::generate();
    let ks = Keystore::new(&root.join(".krowk"));
    ks.recover(AccountKey::from_bytes(*account.as_bytes())).unwrap();
    let device = ks.device().unwrap().unwrap();
    let signing = ks.signing_key().unwrap();
    let api = krowk_api::Client::new(&m.api_url, TOKEN).signed_by(e2e::DeviceSigner::new(device.id(), SigningKey::from_secret(&*signing.secret_bytes()).unwrap()).shared());
    api.register_device(&e2e::hex(&device.public().0), &e2e::hex(&signing.public().0), "this machine", &account.id().to_string()).unwrap();

    let sessions = root.join(".krowk").join("sessions");
    let old = session(&sessions, &root, "the idle session's secret prompt", 20);
    let pinned = session(&sessions, &root, "a pinned one", 30);
    let fresh = session(&sessions, &root, "a recent one", 3);
    let original = std::fs::read(sessions.join(&old).join(log::EVENTS_FILE)).unwrap();
    let (ok, _, out) = m.krowk(&["sessions", "import", "--from", "all"]);
    assert!(ok, "{out}");
    let (ok, _, out) = m.krowk(&["sessions", "pin", &pinned]);
    assert!(ok, "{out}");

    // R-VINT-2: the weekly job takes the idle one only.
    let (ok, v, out) = m.krowk(&["sessions", "archive", "--weekly"]);
    assert!(ok, "{out}");
    let archived: Vec<String> = v["data"]["archived"].as_array().into_iter().flatten().filter_map(|a| a["id"].as_str().map(str::to_owned)).collect();
    assert_eq!(archived, vec![old.clone()], "{out}");
    assert!(sessions.join(&pinned).join(log::EVENTS_FILE).is_file(), "a pinned session is never archived");
    assert!(sessions.join(&fresh).join(log::EVENTS_FILE).is_file(), "a recent one stays");
    let (ok, v, out) = m.krowk(&["sessions", "archive", "--weekly"]);
    assert!(ok && v["data"]["due"] == false, "the weekly job waits a week: {out}");

    // R-VINT-3: the bodies left; the index row did not.
    assert!(!sessions.join(&old).join(log::EVENTS_FILE).exists() && !sessions.join(&old).join(log::CONTEXT_FILE).exists());
    assert!(sessions.join(&old).join(krowk_harness::vintage::STUB_FILE).is_file());
    let (ok, v, out) = m.krowk(&["sessions"]);
    assert!(ok, "{out}");
    let listed = v["data"]["sessions"].as_array().unwrap().iter().find(|s| s.to_string().contains(&old)).cloned().unwrap_or_else(|| panic!("still listed: {out}"));
    assert!(listed.to_string().contains("secret prompt"), "under its title: {listed}");
    let (ok, _, out) = m.krowk(&["sessions", "rebuild", "--yes"]);
    assert!(ok, "{out}");
    let (_, v, out) = m.krowk(&["sessions"]);
    assert!(v.to_string().contains(&old), "a rebuild keeps it listed from its stub: {out}");

    // R-VINT-1: the registry holds the week's vintage, and only ciphertext.
    let week = krowk_harness::vintage::read_stub(&sessions, &old).unwrap().week;
    let vintage = api.week_vintage(&week).unwrap().expect("the week's vintage");
    let sealed = api.read_vintage(&vintage).unwrap();
    assert!(!sealed.windows(6).any(|w| w == b"secret"), "no plaintext on the server");
    let plain = krowk_harness::vintage::unpack(&e2e::open_vintage(&sealed, &week, &account).unwrap()).unwrap();
    assert!(plain.contains_key(&old) && plain.len() == 1);
    assert!(e2e::open_vintage(&sealed, &week, &AccountKey::generate()).is_err(), "only the account key opens it");

    // R-VINT-4: opening it brings it back, byte for byte.
    let (ok, v, out) = m.krowk(&["sessions", "show", &old]);
    assert!(ok, "{out}");
    assert!(v.to_string().contains("answering the idle session"), "shown in full: {out}");
    assert_eq!(std::fs::read(sessions.join(&old).join(log::EVENTS_FILE)).unwrap(), original);
    assert!(!sessions.join(&old).join(krowk_harness::vintage::STUB_FILE).exists());

    // R-VINT-1: a second session last active in the same week joins the
    // week's vintage: the old one is read and merged, then replaced.
    let sibling = session(&sessions, &root, "a sibling from the same week", 20);
    let (ok, v, out) = m.krowk(&["sessions", "archive"]);
    assert!(ok, "{out}");
    assert!(v.to_string().contains(&sibling) && v.to_string().contains(&old), "{out}");
    let merged = api.week_vintage(&week).unwrap().expect("the week's vintage");
    assert_ne!(merged.slug, vintage.slug, "replaced");
    let plain = krowk_harness::vintage::unpack(&e2e::open_vintage(&api.read_vintage(&merged).unwrap(), &week, &account).unwrap()).unwrap();
    assert!(plain.contains_key(&old) && plain.contains_key(&sibling), "one vintage holds the week's sessions");
    // A replacement that did not read the week's latest is refused.
    assert_eq!(api.put_vintage(&week, b"stale", Some(&vintage.slug)).unwrap_err().code(), "vintage_conflict");
    let _ = std::fs::remove_dir_all(&root);
}
