//! `krowk --resume <id>` on a machine that does not hold the session's log
//! but syncs the workspace: it attaches the synced session rather than
//! saying there is no such session, so the command an artifact card copies
//! (ticket 21) works on any of the workspace's machines. Against the
//! stand-in registry and the reference relay, with the host offline: the
//! session shows from its checkpoint, read-only (R-HAND-4).

#![cfg(all(feature = "harness", unix))]

use krowk_client::e2e::{self, AccountKey, SessionKey, SigningKey};
use krowk_client::keystore::Keystore;
use krowk_client::session_record;
use krowk_harness::sync::store;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "common/device_list.rs"]
mod device_list;
use device_list::People;

const TOKEN: &str = "krowk_sk_sync_resume_0000000000000000";
const MARKER: &str = "the session machine A ran, read on machine B";

#[test]
fn r_sync_1_resume_of_a_session_only_another_machine_holds_attaches_it_through_sync() {
    let root = std::env::temp_dir().join(format!("krowk-sync-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
    let api_url = format!("{}/v1", registry.url());
    let relay = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_url = format!("ws://{}", relay.local_addr().unwrap());
    let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
    let roster = krowk_harness::relay::Roster::parse(&roster).unwrap();
    std::thread::spawn(move || krowk_harness::relay::run(relay, krowk_harness::relay::Config { roster, origin: None, limits: Default::default(), state: None, origins: Vec::new(), whois: None, pin: None }));

    // Machines A and B, each a home of its own on the person's device list.
    let account = AccountKey::generate();
    let mut people = People::default();
    let home = |name: &str| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let ks = Keystore::new(&dir.join(".krowk"));
        ks.recover(AccountKey::from_bytes(*account.as_bytes())).unwrap();
        (dir, ks)
    };
    let (_, a_keys) = home("a");
    people.enlist(&a_keys, "machine-a");
    let (b_home, ks) = home("b");
    people.enlist(&ks, "machine-b");
    // The registry holds the list too: B brings the list it keeps up to
    // date from it before it attaches.
    people.publish(&api_url, TOKEN);

    // Machine A wrote the session to the registry, its record signed, and
    // went away.
    let (a, a_signing) = (a_keys.device().unwrap().unwrap(), a_keys.signing_key().unwrap());
    // A's calls act as A, signed by its key.
    let api = Arc::new(krowk_api::Client::new(&api_url, TOKEN).signed_by(e2e::DeviceSigner::new(a.id(), SigningKey::from_secret(&*a_signing.secret_bytes()).unwrap()).shared()));
    api.register_device(&e2e::hex(&a.public().0), &e2e::hex(&a_signing.public().0), "machine-a", &account.id().to_string()).unwrap();
    let id = "01a0ec7b-1111-7000-8000-000000000019".to_string();
    let raw = krowk_harness::daemon::ws::uuid(&id);
    let key = SessionKey::generate();
    let sealed_key = e2e::wrap_session_key(&key, &raw, people.user());
    let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, people.user(), &a_signing).unwrap();
    let wrapped = e2e::hex(&sealed_key);
    let index = store::Index { title: "resumed from sync".into(), ..Default::default() };
    api.put_sync_session(&id, &wrapped, Some((&e2e::hex(&signature), &a.id().to_string())), Some(&e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).unwrap()))), None).unwrap();
    let lease = api.acquire_lease(&id, &a.id().to_string(), 60, "development").unwrap();
    let mut w = store::Writer::take_up(api.clone(), key, &id, wrapped, index, lease.fence).unwrap();
    w.push(serde_json::json!({"id": "01a0ec7b-1111-7000-8000-0000000000e1", "type": "item.completed", "item": {"type": "userText", "text": MARKER}}));
    w.checkpoint(None, &lease.token).unwrap();
    api.release_lease(&id, &lease.token).unwrap();

    // Machine B: keys of its own, registered.
    let b = ks.device().unwrap().unwrap();
    let b_signing = ks.signing_key().unwrap();
    let b_api = krowk_api::Client::new(&api_url, TOKEN).signed_by(e2e::DeviceSigner::new(b.id(), SigningKey::from_secret(&*b_signing.secret_bytes()).unwrap()).shared());
    b_api.register_device(&e2e::hex(&b.public().0), &e2e::hex(&b_signing.public().0), "machine-b", &account.id().to_string()).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_krowk"))
        .args(["--resume", &id])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &b_home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", &api_url)
        .env("KROWK_TOKEN", TOKEN)
        .env("KROWK_RELAY_URL", &relay_url)
        .current_dir(&b_home)
        // Held open: `sync attach` ends when its stdin does.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut shown, mut read_only) = (false, false);
    let mut seen = Vec::new();
    while !(shown && read_only) && Instant::now() < deadline {
        let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) else {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            continue;
        };
        shown |= l.contains(MARKER);
        read_only |= l.contains(r#""type":"sync.host""#) && l.contains(r#""present":false"#);
        seen.push(l);
    }
    let _ = child.kill();
    let status = child.wait_with_output().unwrap();
    assert!(shown, "B shows the session from its checkpoint: {seen:?} {}", String::from_utf8_lossy(&status.stderr));
    assert!(read_only, "and says no host is online: {seen:?}");
    let _ = std::fs::remove_dir_all(&root);
}
