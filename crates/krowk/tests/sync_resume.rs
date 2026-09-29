//! `krowk --resume <id>` on a machine that does not hold the session's log
//! but syncs the workspace: it attaches the synced session rather than
//! saying there is no such session, so the command an artifact card copies
//! (ticket 21) works on any of the workspace's machines. Against the
//! stand-in registry and the reference relay, with the host offline: the
//! session shows from its checkpoint, read-only (R-HAND-4).

#![cfg(all(feature = "harness", unix))]

use krowk_client::e2e::{self, AccountKey, DeviceKey, SessionKey, SigningKey};
use krowk_client::keystore::Keystore;
use krowk_harness::sync::store;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "krowk_sk_sync_resume_0000000000000000";
const MARKER: &str = "the session machine A ran, read on machine B";

#[test]
fn r_sync_1_resume_of_a_session_only_another_machine_holds_attaches_it_through_sync() {
    let root = std::env::temp_dir().join(format!("krowk-sync-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
    let api_url = format!("{}/v1", registry.url());
    let api = Arc::new(krowk_api::Client::new(&api_url, TOKEN));
    let relay = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_url = format!("ws://{}", relay.local_addr().unwrap());
    let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
    let roster = krowk_harness::relay::Roster::parse(&roster).unwrap();
    std::thread::spawn(move || krowk_harness::relay::run(relay, krowk_harness::relay::Config { roster, origin: None, limits: Default::default(), state: None, origins: Vec::new(), whois: None }));

    // Machine A wrote the session to the registry and went away.
    let account = AccountKey::generate();
    let (a, a_signing) = (DeviceKey::generate(), SigningKey::generate());
    api.register_device(&e2e::hex(&a.public().0), &e2e::hex(&a_signing.public().0), "machine-a", &account.id().to_string()).unwrap();
    let id = "01a0ec7b-1111-7000-8000-000000000019".to_string();
    let raw = krowk_harness::daemon::ws::uuid(&id);
    let key = SessionKey::generate();
    let wrapped = e2e::hex(&e2e::wrap_session_key(&key, &raw, &account));
    let index = store::Index { title: "resumed from sync".into(), ..Default::default() };
    api.put_sync_session(&id, &wrapped, Some(&e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).unwrap()))), None).unwrap();
    let lease = api.acquire_lease(&id, &a.id().to_string(), 60, "development").unwrap();
    let mut w = store::Writer::take_up(api.clone(), key, &id, wrapped, index, lease.fence).unwrap();
    w.push(serde_json::json!({"id": "01a0ec7b-1111-7000-8000-0000000000e1", "type": "item.completed", "item": {"type": "userText", "text": MARKER}}));
    w.checkpoint(None, &lease.token).unwrap();
    api.release_lease(&id, &lease.token).unwrap();

    // Machine B: the same account key, keys of its own, registered.
    let b_home = root.join("b");
    std::fs::create_dir_all(&b_home).unwrap();
    let ks = Keystore::new(&b_home.join(".krowk"));
    ks.recover(AccountKey::from_bytes(*account.as_bytes())).unwrap();
    let b = ks.device().unwrap().unwrap();
    let b_signing = ks.signing_key().unwrap();
    api.register_device(&e2e::hex(&b.public().0), &e2e::hex(&b_signing.public().0), "machine-b", &account.id().to_string()).unwrap();

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
        .stdin(Stdio::null())
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
