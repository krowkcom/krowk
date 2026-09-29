//! Adding a device and syncing a session to it, end to end on two scratch
//! homes against the stand-in registry (R-E2E-3, R-E2E-1, R-SYNC-1,
//! R-SYNC-2): the laptop sets sync up from its phrase and puts a session in
//! the registry, the desktop asks to join, the laptop approves the code it
//! shows, and the desktop then opens the laptop's session — while nothing
//! that crossed the wire, in either direction, held the session's title.
//!
//! The first test runs against a real registry instead when
//! `KROWK_SYNC_TEST_API` and `KROWK_SYNC_TEST_TOKEN` name one and a key to a
//! Pro workspace on it — a local `bin/rails server`, never production. The
//! wire is then not recorded; the registry's own suite checks its tables.

#![cfg(all(feature = "harness", unix))]

use krowk_client::e2e::{self, AccountKey, Direction, SessionKey};
use krowk_client::keystore::Keystore;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOKEN: &str = "krowk_sk_sync_devices";
const TITLE: &str = "plaintext-marker: rotate the staging database password";

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-devices-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["laptop", "desktop"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    root.canonicalize().unwrap()
}

/// krowk with `home` as HOME and nothing else of this machine's. The debug
/// build's stand-in for the person saying yes at `join` and `approve` is on;
/// `attended` leaves it off.
fn command(home: &Path, api: &str, token: &str, args: &[&str]) -> Command {
    let mut c = attended(home, api, token, args);
    c.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1");
    c
}

fn attended(home: &Path, api: &str, token: &str, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", api)
        .env("KROWK_TOKEN", token)
        .current_dir(home)
        .stdin(Stdio::null());
    c
}

fn run(home: &Path, api: &str, token: &str, args: &[&str], input: &str) -> Output {
    let mut child = command(home, api, token, args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn json(out: &Output) -> Value {
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Every byte that crosses between the CLI (or this test) and the stand-in,
/// both ways, so a test can say something never went over the wire.
fn recording_proxy(registry: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let upstream = TcpStream::connect(registry).unwrap();
            for (mut from, mut to) in [(client.try_clone().unwrap(), upstream.try_clone().unwrap()), (upstream, client)] {
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16384];
                    while let Ok(n) = from.read(&mut buf) {
                        if n == 0 || to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        log.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                    let _ = to.shutdown(std::net::Shutdown::Write);
                });
            }
        }
    });
    (addr, seen)
}

/// The code a running `krowk sync join` shows, read off its stderr.
fn code_shown(join: &mut Child) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = join.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = String::new();
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            let t = line.trim();
            if t.len() == 39 && t.split(' ').all(|g| g.len() == 4 && g.bytes().all(|b| b.is_ascii_hexdigit())) {
                return t.to_string();
            }
            seen.push_str(&line);
            seen.push('\n');
        }
    }
    panic!("`krowk sync join` showed no code:\n{seen}");
}

fn uuid(b: &[u8; 16]) -> String {
    let h = e2e::hex(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

#[test]
fn r_e2e_3_a_device_approved_from_another_opens_the_session_it_made() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let (proxy, wire) = recording_proxy(registry.addr());
    let real = std::env::var("KROWK_SYNC_TEST_API").ok().zip(std::env::var("KROWK_SYNC_TEST_TOKEN").ok());
    let (api, token) = real.clone().unwrap_or_else(|| (format!("http://{proxy}/v1"), TOKEN.to_string()));
    let (api, token) = (api.as_str(), token.as_str());
    let r = root("approve");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));

    // The laptop: sync set up from a phrase (no terminal needed), which
    // registers it with the workspace.
    let words = krowk_client::phrase::encode(&AccountKey::generate());
    let set_up = json(&run(&laptop, api, token, &["sync", "recover", "--json"], &format!("{}\n", *words)));
    assert_eq!(set_up["data"]["registered"], true, "{set_up}");
    let account_id = set_up["data"]["account_key"].as_str().unwrap().to_string();

    // A session the laptop hosts: its key wrapped under the account key, its
    // title sealed under its key, and the lease taken before a later write.
    let client = krowk_api::Client::new(api, token);
    let laptop_keys = Keystore::new(&laptop.join(".krowk"));
    let account = laptop_keys.account().unwrap().unwrap();
    let laptop_device = laptop_keys.device().unwrap().unwrap().id().to_string();
    let id: [u8; 16] = e2e::random();
    let session_key = SessionKey::generate();
    let wrapped = e2e::hex(&e2e::wrap_session_key(&session_key, &id, &account));
    let sealed = e2e::hex(&e2e::seal_session_index(&session_key, &id, TITLE.as_bytes()));
    client.put_sync_session(&uuid(&id), &wrapped, Some(&sealed), None).unwrap();
    let lease = client.acquire_lease(&uuid(&id), &laptop_device, 60).unwrap();
    let resealed = e2e::hex(&e2e::seal_session_index(&session_key, &id, TITLE.as_bytes()));
    client.put_sync_session(&uuid(&id), &wrapped, Some(&resealed), Some(&lease.token)).unwrap();

    // The desktop asks to join, naming the account key id it was told.
    let mut join = command(&desktop, api, token, &["sync", "join", &account_id, "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let code = code_shown(&mut join);

    // A wrong code approves nothing; the right one wraps to the desktop.
    let wrong = run(&laptop, api, token, &["devices", "approve", &"0".repeat(32), "--json"], "");
    assert!(!wrong.status.success() && String::from_utf8_lossy(&wrong.stderr).contains("no_such_device_code"), "{}", String::from_utf8_lossy(&wrong.stderr));
    let approved = json(&run(&laptop, api, token, &["devices", "approve", &code, "--json"], ""));
    assert_eq!(approved["data"]["device"], code.replace(' ', ""), "{approved}");

    let mut out = String::new();
    join.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert!(join.wait().unwrap().success(), "{out}");
    let joined: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(joined["data"]["account_key"], account_id.as_str(), "the desktop holds the laptop's account key");
    assert_eq!(joined["data"]["joined"], true);

    // The desktop opens the laptop's session with nothing but its own home.
    let desktop_account = Keystore::new(&desktop.join(".krowk")).account().unwrap().expect("the desktop kept the account key");
    let held = client.show_sync_session(&uuid(&id)).unwrap();
    let key = e2e::unwrap_session_key(&e2e::unhex(&held.wrapped_key).unwrap(), &id, &desktop_account).expect("the desktop unwraps the session key");
    let title = e2e::open_session_index(&e2e::unhex(&held.sealed_index).unwrap(), &id, &key).unwrap();
    assert_eq!(title, TITLE.as_bytes());
    // And the frames the session streams, under the key it now holds.
    let mut opener = e2e::Opener::new(&key, id, Direction::HostToClient);
    let mut sealer = e2e::Sealer::new(&session_key, id, Direction::HostToClient, opener.epoch());
    let header: [u8; e2e::HEADER] = [&[1u8, 1, 0, e2e::ENC_XCHACHA20_POLY1305][..], &id, &[0; 8]].concat().try_into().unwrap();
    assert_eq!(opener.open(&header, &sealer.seal(&header, b"turn 1").unwrap()).unwrap(), b"turn 1");

    let listed = json(&run(&desktop, api, token, &["devices", "list", "--json"], ""));
    assert_eq!(listed["data"]["devices"].as_array().unwrap().len(), 2, "{listed}");
    assert_eq!(listed["data"]["devices"].as_array().unwrap().iter().filter(|d| d["this_device"] == true).count(), 1);

    // R-E2E-1: the title never crossed the wire in the clear, either way.
    let wire = wire.lock().unwrap();
    assert!(real.is_some() || !wire.is_empty());
    assert!(!wire.windows(TITLE.len()).any(|w| w == TITLE.as_bytes()), "the session's title went over the wire in the clear");
    assert!(!wire.windows(16).any(|w| w == &account.as_bytes()[..16]), "the account key went over the wire in the clear");
    let _ = std::fs::remove_dir_all(&r);
}

/// A join that is told a different account key id than the one approved
/// keeps nothing — what catches a registry wrapping its own key.
#[test]
fn r_e2e_3_a_join_keeps_nothing_when_the_approved_key_is_not_the_one_named() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("mismatch");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let words = krowk_client::phrase::encode(&AccountKey::generate());
    json(&run(&laptop, &api, TOKEN, &["sync", "recover", "--json"], &format!("{}\n", *words)));

    let other = AccountKey::generate().id().to_string();
    let mut join = command(&desktop, &api, TOKEN, &["sync", "join", &other, "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let code = code_shown(&mut join);
    json(&run(&laptop, &api, TOKEN, &["devices", "approve", &code, "--json"], ""));
    assert!(!join.wait().unwrap().success());
    assert!(!desktop.join(".krowk/account-key.json").exists(), "nothing was kept");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-SYNC-1: a free workspace is refused every sync call with a fix, and
/// setting sync up still works locally — it just is not registered.
#[test]
fn r_sync_1_a_free_workspace_is_refused_with_a_fix_and_sync_still_sets_up_locally() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("free");
    let laptop = r.join("laptop");
    let free = "krowk_sk_free_workspace";

    let listed = run(&laptop, &api, free, &["devices", "list", "--json"], "");
    assert!(!listed.status.success());
    let err = String::from_utf8_lossy(&listed.stderr);
    assert!(err.contains("sync_requires_paid_plan") && err.contains("upgrade this workspace to Pro"), "{err}");

    let words = krowk_client::phrase::encode(&AccountKey::generate());
    let set_up = json(&run(&laptop, &api, free, &["sync", "recover", "--json"], &format!("{}\n", *words)));
    assert_eq!(set_up["data"]["registered"], false, "{set_up}");
    assert!(laptop.join(".krowk/account-key.json").exists());
    let _ = std::fs::remove_dir_all(&r);
}

/// R-SYNC-2: a second device cannot take a lease that is held, and the
/// holder's stale fence is refused once the lease has moved on.
#[test]
fn r_sync_2_a_stale_lease_holders_write_is_refused() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let client = krowk_api::Client::new(&format!("{}/v1", registry.url()), TOKEN);
    let account = AccountKey::generate();
    let (a, b) = (e2e::DeviceKey::generate(), e2e::DeviceKey::generate());
    for d in [&a, &b] {
        client.register_device(&e2e::hex(&d.public().0), "machine", &account.id().to_string()).unwrap();
    }
    let id: [u8; 16] = e2e::random();
    let key = SessionKey::generate();
    let wrapped = e2e::hex(&e2e::wrap_session_key(&key, &id, &account));
    client.put_sync_session(&uuid(&id), &wrapped, Some(&e2e::hex(&e2e::seal_session_index(&key, &id, b"one"))), None).unwrap();

    let held = client.acquire_lease(&uuid(&id), &a.id().to_string(), 60).unwrap();
    assert_eq!(held.token.len(), 32, "the acquirer, and only it, is handed a token");
    let refused = client.acquire_lease(&uuid(&id), &b.id().to_string(), 60).unwrap_err();
    assert_eq!(refused.code(), "lease_held");
    assert!(!format!("{:?}", refused.body).contains(&held.token));
    // Knowing the fence is not holding the lease.
    assert_eq!(client.renew_lease(&uuid(&id), &b.id().to_string(), &held.fence.to_string(), 60).unwrap_err().code(), "lease_stale");
    let listed = client.list_sync_sessions("", 50).unwrap();
    assert_eq!(listed.sessions[0].lease.as_ref().unwrap().device, a.id().to_string());
    assert!(listed.sessions[0].sealed_index.is_empty(), "a listing carries no sealed index");

    let moved = client.renew_lease(&uuid(&id), &b.id().to_string(), &held.token, 60).unwrap();
    assert!(moved.fence > held.fence && !moved.token.is_empty() && moved.token != held.token);

    let stale = client.put_sync_session(&uuid(&id), &wrapped, Some(&e2e::hex(&e2e::seal_session_index(&key, &id, b"two"))), Some(&held.token)).unwrap_err();
    assert_eq!(stale.code(), "lease_stale");
    assert!(stale.fix().contains("acquire it again"), "{:?}", stale.body);
    client.put_sync_session(&uuid(&id), &wrapped, Some(&e2e::hex(&e2e::seal_session_index(&key, &id, b"two"))), Some(&moved.token)).unwrap();
    // A write that names no index leaves it as it was.
    client.put_sync_session(&uuid(&id), &wrapped, None, Some(&moved.token)).unwrap();
    let shown = client.show_sync_session(&uuid(&id)).unwrap();
    assert_eq!(e2e::open_session_index(&e2e::unhex(&shown.sealed_index).unwrap(), &id, &key).unwrap(), b"two");
    client.release_lease(&uuid(&id), &moved.token).unwrap();
}

/// R-E2E-3: off a terminal, `approve` and `join` refuse — a code or an id
/// handed to an agent does not add a device — and nothing is wrapped.
#[test]
fn r_e2e_3_approve_and_join_refuse_without_a_person_at_a_terminal() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("headless");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let words = krowk_client::phrase::encode(&AccountKey::generate());
    json(&run(&laptop, &api, TOKEN, &["sync", "recover", "--json"], &format!("{}\n", *words)));

    for (home, args) in [(&laptop, vec!["devices", "approve", &"0".repeat(32), "--json"]), (&desktop, vec!["sync", "join", &"0".repeat(32), "--json"])] {
        let out = attended(home, &api, TOKEN, &args).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("confirmation_required") && err.contains("a person at a terminal"), "{err}");
    }
    assert!(!desktop.join(".krowk/device.json").exists(), "join asked for nothing");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-SYNC-1: with a key but no registry answering, setting sync up still
/// succeeds, locally, and `krowk sync register` registers it later.
#[test]
fn r_sync_1_setup_with_the_registry_unreachable_stays_local_and_registers_later() {
    let r = root("offline");
    let laptop = r.join("laptop");
    let words = krowk_client::phrase::encode(&AccountKey::generate());
    let set_up = json(&run(&laptop, "http://127.0.0.1:9/v1", TOKEN, &["sync", "recover", "--json"], &format!("{}\n", *words)));
    assert_eq!(set_up["data"]["registered"], false, "{set_up}");
    assert!(set_up["summary"].as_str().unwrap().contains("krowk sync register"), "{set_up}");

    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let registered = json(&run(&laptop, &api, TOKEN, &["sync", "register", "--name", "work laptop", "--json"], ""));
    assert_eq!(registered["data"]["registered"], true, "{registered}");
    let listed = json(&run(&laptop, &api, TOKEN, &["devices", "list", "--json"], ""));
    assert_eq!(listed["data"]["devices"][0]["name"], "work laptop");
    let _ = std::fs::remove_dir_all(&r);
}
