//! `krowk sync take <id>` on a machine that cannot run the session: it
//! refuses with a fix before asking the host for anything, and the session
//! stays where it is (R-INST-5, R-CRED-1). Against the stand-in registry.

#![cfg(all(feature = "harness", unix))]

use krowk_client::e2e::{self, AccountKey, SessionKey};
use krowk_client::keystore::Keystore;
use krowk_client::session_record;
use krowk_harness::sync::store;
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;

#[path = "common/device_list.rs"]
mod device_list;
use device_list::People;

const TOKEN: &str = "krowk_sk_sync_take_00000000000000000000";

#[test]
fn r_inst_5_take_refuses_with_a_fix_when_the_sessions_instance_is_missing_and_the_session_stays() {
    let root = std::env::temp_dir().join(format!("krowk-sync-take-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
    let api_url = format!("{}/v1", registry.url());

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
    people.publish(&api_url, TOKEN);

    // A hosts a session whose turns run on an instance B never connected.
    let (a, a_signing) = (a_keys.device().unwrap().unwrap(), a_keys.signing_key().unwrap());
    let api = Arc::new(people.client(&api_url, TOKEN, 0));
    let id = "01a0ec7b-1111-7000-8000-000000000023".to_string();
    let raw = krowk_harness::daemon::ws::uuid(&id);
    let key = SessionKey::generate();
    let sealed_key = e2e::wrap_session_key(&key, &raw, people.user());
    let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, people.user(), &a_signing).unwrap();
    let wrapped = e2e::hex(&sealed_key);
    let index = store::Index { title: "on a's instance".into(), ..Default::default() };
    api.put_sync_session(&id, &wrapped, Some((&e2e::hex(&signature), &a.id().to_string())), Some(&e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).unwrap()))), None).unwrap();
    let lease = api.acquire_lease(&id, &a.id().to_string(), 60, "development").unwrap();
    let mut w = store::Writer::take_up(api.clone(), key.into(), &id, wrapped, index, lease.fence).unwrap();
    w.push(serde_json::json!({"id": "01a0ec7b-1111-7000-8000-0000000000e1", "type": "turn.started", "turnId": "t", "model": {"instance": "work-claude", "model": "claude-sonnet-4-6"}}));
    w.checkpoint(None, &lease.token).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_krowk"))
        .args(["sync", "take", &id])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &b_home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", &api_url)
        .env("KROWK_TOKEN", device_list::key(TOKEN, "machine-b"))
        .current_dir(&b_home)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "refused: {err}");
    assert!(err.contains("work-claude") && err.contains("krowk connect") && err.contains("stays where it is"), "a fix naming the instance: {err}");
    let held = api.show_sync_session(&id).unwrap().lease.expect("still leased");
    assert_eq!(held.device, a.id().to_string(), "the session stayed on A");
    let _ = std::fs::remove_dir_all(&root);
}
