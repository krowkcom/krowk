//! Adding a device and syncing a session to it, end to end on two scratch
//! homes against the stand-in registry (R-E2E-3, R-E2E-1, R-SYNC-1,
//! R-SYNC-2): the laptop sets sync up with an account key and puts a session in
//! the registry, the desktop asks to join, the laptop approves the code it
//! shows, and the desktop then opens the laptop's session — while nothing
//! that crossed the wire, in either direction, held the session's title.
//!
//! The first test runs against a real registry instead when
//! `KROWK_SYNC_TEST_API` and `KROWK_SYNC_TEST_TOKEN` name one and a key to a
//! Pro workspace on it — a local `bin/rails server`, never production. The
//! wire is then not recorded; the registry's own suite checks its tables.

#![cfg(all(feature = "harness", unix))]

use krowk_api::client::RequestSigner;
use krowk_client::e2e::{self, AccountKey, Direction, SessionKey};
use krowk_client::keystore::Keystore;
use krowk_client::user_key::{UserKey, UserKeys};
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

/// Sync on the account key, as joining and leases still run on it until
/// sessions move to the user key: an account key in `home`, and the device
/// registered with the workspace. What `krowk sync recover` did from the
/// 24-word phrase before the recovery kit replaced it.
fn set_up(home: &Path, api: &str, token: &str) -> Value {
    let keys = Keystore::new(&home.join(".krowk"));
    let (setup, _) = keys.recover(AccountKey::generate()).unwrap();
    let signing = keys.signing_key().unwrap();
    let public = e2e::hex(&signing.public().0);
    let client = krowk_api::Client::new(api, token).signed_by(e2e::DeviceSigner::new(setup.device.id(), signing).shared());
    let registered = client.register_device(&e2e::hex(&setup.device.public().0), &public, "laptop", &setup.account.id().to_string());
    serde_json::json!({ "data": { "registered": registered.is_ok(), "account_key": setup.account.id().to_string() } })
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
    let set_up = set_up(&laptop, api, token);
    assert_eq!(set_up["data"]["registered"], true, "{set_up}");
    let account_id = set_up["data"]["account_key"].as_str().unwrap().to_string();

    // A session the laptop hosts: its key wrapped under the person's user key, its
    // title sealed under its key, and the lease taken before a later write.
    let laptop_keys = Keystore::new(&laptop.join(".krowk"));
    let account = laptop_keys.account().unwrap().unwrap();
    let user = UserKey::first();
    laptop_keys.save_user_keys(&UserKeys::new(user.clone(), []).unwrap()).unwrap();
    let laptop_id = laptop_keys.device().unwrap().unwrap().id();
    let client = krowk_api::Client::new(api, token).signed_by(e2e::DeviceSigner::new(laptop_id, laptop_keys.signing_key().unwrap()).shared());
    let laptop_device = laptop_id.to_string();
    let id: [u8; 16] = e2e::random();
    let session_key = SessionKey::generate();
    let wrapped = e2e::hex(&e2e::wrap_session_key(&session_key, &id, &user));
    let sealed = e2e::hex(&e2e::seal_session_index(&session_key, &id, TITLE.as_bytes()));
    client.put_sync_session(&uuid(&id), &wrapped, None, Some(&sealed), None).unwrap();
    let lease = client.acquire_lease(&uuid(&id), &laptop_device, 60, "production").unwrap();
    let resealed = e2e::hex(&e2e::seal_session_index(&session_key, &id, TITLE.as_bytes()));
    client.put_sync_session(&uuid(&id), &wrapped, None, Some(&resealed), Some(&lease.token)).unwrap();

    // The desktop asks to join, naming the account key id it was told.
    let mut join = command(&desktop, api, token, &["sync", "join", &account_id, "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let code = code_shown(&mut join);

    // A wrong code approves nothing; the right one wraps to the desktop.
    let wrong = run(&laptop, api, token, &["devices", "approve", &"0".repeat(32), "--json"], "");
    assert!(!wrong.status.success() && String::from_utf8_lossy(&wrong.stderr).contains("no_such_device_code"), "{}", String::from_utf8_lossy(&wrong.stderr));
    let approved = json(&run(&laptop, api, token, &["devices", "approve", &code, "--json"], ""));
    assert_eq!(approved["data"]["code"], code.replace(' ', ""), "{approved}");

    let mut out = String::new();
    join.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert!(join.wait().unwrap().success(), "{out}");
    let joined: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(joined["data"]["account_key"], account_id.as_str(), "the desktop holds the laptop's account key");
    assert_eq!(joined["data"]["joined"], true);

    // The desktop opens the laptop's session with nothing but its own home.
    // Until adding a device carries the user key to it (D5), the test hands
    // the laptop's over, wrapped to the desktop's device key as the join
    // would leave it.
    let desktop_home = Keystore::new(&desktop.join(".krowk"));
    desktop_home.save_user_keys(&UserKeys::new(user.clone(), []).unwrap()).unwrap();
    let desktop_user = desktop_home.user_keys().unwrap().expect("the desktop kept the user key");
    let held = client.show_sync_session(&uuid(&id)).unwrap();
    assert_eq!(held.seal, "user", "a private session is sealed under its owner's user key");
    let key = e2e::unwrap_session_key(&e2e::unhex(&held.wrapped_key).unwrap(), &id, &desktop_user).expect("the desktop unwraps the session key");
    let title = e2e::open_session_index(&e2e::unhex(&held.sealed_index).unwrap(), &id, &key).unwrap();
    assert_eq!(title, TITLE.as_bytes());
    // And the frames the session streams, under the key it now holds.
    let mut opener = e2e::Opener::new(&key, id, Direction::HostToClient);
    let mut sealer = e2e::Sealer::new(&session_key, id, Direction::HostToClient, opener.epoch());
    let header: [u8; e2e::HEADER] = [&[1u8, 1, 0, e2e::ENC_XCHACHA20_POLY1305][..], &id, &[0; 8]].concat().try_into().unwrap();
    assert_eq!(opener.open(&header, &sealer.seal(&header, b"turn 1").unwrap()).unwrap(), b"turn 1");

    // The session's log: the laptop, holding the lease, writes two sealed
    // chunks through presign → storage → finalize; the desktop reads them
    // back in order and opens them (R-E2E-1, R-VINT-5's stored bytes).
    let mut sealer = e2e::ChunkSealer::new(&session_key, id, 0, e2e::NO_PREVIOUS_CHUNK, lease.fence);
    for (text, last) in [(TITLE, false), ("the last turn", true)] {
        let sealed = sealer.seal(text.as_bytes(), last).unwrap();
        let put = client.put_chunk(&uuid(&id), sealer.next() - 1, &sealed, &lease.token).unwrap();
        assert_eq!(put.state, "ready");
    }
    let page = client.list_chunks(&uuid(&id), None, 50).unwrap();
    assert_eq!(page.chunks.iter().map(|c| c.index).collect::<Vec<_>>(), [0, 1]);
    let mut reader = e2e::ChunkReader::new(&key, id);
    let log: Vec<Vec<u8>> = page.chunks.iter().map(|c| reader.open(&client.read_chunk(c).unwrap()).unwrap()).collect();
    assert_eq!(log, [TITLE.as_bytes().to_vec(), b"the last turn".to_vec()]);
    assert!(reader.finished(), "the log ends with its final chunk");

    // R-RELAY-1: both machines registered the relay signing key they hold
    // — the laptop at recover, the desktop at join — which is what the
    // hosted relay checks their joins against.
    let desktop_keys = Keystore::new(&desktop.join(".krowk"));
    for keys in [&laptop_keys, &desktop_keys] {
        let id = keys.device().unwrap().unwrap().id().to_string();
        let registered = client.list_devices().unwrap().into_iter().find(|d| d.id == id).expect("the device is registered");
        assert_eq!(registered.signing_key, e2e::hex(&keys.signing_key().unwrap().public().0), "device {id} registered its signing key");
    }

    let listed = json(&run(&desktop, api, token, &["devices", "list", "--json"], ""));
    assert_eq!(listed["data"]["devices"].as_array().unwrap().len(), 2, "{listed}");
    assert_eq!(listed["data"]["devices"].as_array().unwrap().iter().filter(|d| d["this_device"] == true).count(), 1);

    // R-E2E-1: the title never crossed the wire in the clear, either way.
    let wire = wire.lock().unwrap();
    assert!(real.is_some() || !wire.is_empty());
    assert!(!wire.windows(TITLE.len()).any(|w| w == TITLE.as_bytes()), "the session's title went over the wire in the clear");
    assert!(!wire.windows(16).any(|w| w == &account.as_bytes()[..16]), "the account key went over the wire in the clear");
    assert!(!wire.windows(16).any(|w| w == &user.as_bytes()[..16]), "the user key went over the wire in the clear");
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
    set_up(&laptop, &api, TOKEN);

    let other = AccountKey::generate().id().to_string();
    let mut join = command(&desktop, &api, TOKEN, &["sync", "join", &other, "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let code = code_shown(&mut join);
    json(&run(&laptop, &api, TOKEN, &["devices", "approve", &code, "--json"], ""));
    assert!(!join.wait().unwrap().success());
    assert!(!desktop.join(".krowk/account-key.json").exists(), "nothing was kept");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-SYNC-2: a second device cannot take a lease that is held, and the
/// holder's stale fence is refused once the lease has moved on.
#[test]
fn r_sync_2_a_stale_lease_holders_write_is_refused() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let account = AccountKey::generate();
    let ((a, client, _), (b, cb, _)) = (machine(&api, &account), machine(&api, &account));
    let id: [u8; 16] = e2e::random();
    let key = SessionKey::generate();
    let wrapped = e2e::hex(&e2e::wrap_session_key(&key, &id, &UserKey::first()));
    client.put_sync_session(&uuid(&id), &wrapped, None, Some(&e2e::hex(&e2e::seal_session_index(&key, &id, b"one"))), None).unwrap();

    let held = client.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "production").unwrap();
    assert_eq!(held.token.len(), 32, "the acquirer, and only it, is handed a token");
    let refused = cb.acquire_lease(&uuid(&id), &b.id().to_string(), 60, "production").unwrap_err();
    assert_eq!(refused.code(), "lease_held");
    assert!(!format!("{:?}", refused.body).contains(&held.token));
    // Knowing the fence is not holding the lease.
    assert_eq!(cb.renew_lease(&uuid(&id), &b.id().to_string(), &held.fence.to_string(), 60, "production").unwrap_err().code(), "lease_stale");
    // Nor is holding its token, for another device: the holder's calls are
    // signed by the holder.
    assert_eq!(cb.renew_lease(&uuid(&id), &b.id().to_string(), &held.token, 60, "production").unwrap_err().code(), "lease_stale");
    let listed = client.list_sync_sessions("", 50).unwrap();
    assert_eq!(listed.sessions[0].lease.as_ref().unwrap().device, a.id().to_string());
    assert!(listed.sessions[0].sealed_index.is_empty(), "a listing carries no sealed index");

    let moved = client.renew_lease(&uuid(&id), &b.id().to_string(), &held.token, 60, "production").unwrap();
    assert!(moved.fence > held.fence && !moved.token.is_empty() && moved.token != held.token);

    let stale = client.put_sync_session(&uuid(&id), &wrapped, None, Some(&e2e::hex(&e2e::seal_session_index(&key, &id, b"two"))), Some(&held.token)).unwrap_err();
    assert_eq!(stale.code(), "lease_stale");
    assert!(stale.fix().contains("acquire it again"), "{:?}", stale.body);
    cb.put_sync_session(&uuid(&id), &wrapped, None, Some(&e2e::hex(&e2e::seal_session_index(&key, &id, b"two"))), Some(&moved.token)).unwrap();
    // A write that names no index leaves it as it was.
    cb.put_sync_session(&uuid(&id), &wrapped, None, None, Some(&moved.token)).unwrap();
    let shown = client.show_sync_session(&uuid(&id)).unwrap();
    assert_eq!(e2e::open_session_index(&e2e::unhex(&shown.sealed_index).unwrap(), &id, &key).unwrap(), b"two");
    cb.release_lease(&uuid(&id), &moved.token).unwrap();
}

/// R-E2E-3: off a terminal, `approve` and `join` refuse — a code or an id
/// handed to an agent does not add a device — and nothing is wrapped.
#[test]
fn r_e2e_3_approve_and_join_refuse_without_a_person_at_a_terminal() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("headless");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    set_up(&laptop, &api, TOKEN);

    for (home, args) in [(&laptop, vec!["devices", "approve", &"0".repeat(32), "--json"]), (&desktop, vec!["sync", "join", &"0".repeat(32), "--json"])] {
        let out = attended(home, &api, TOKEN, &args).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("confirmation_required") && err.contains("a person at a terminal"), "{err}");
    }
    assert!(!desktop.join(".krowk/device.json").exists(), "join asked for nothing");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-SYNC-2: a chunk is the lease holder's to write. A stale token and a
/// missing one are both refused with a fix, and nothing is stored.
#[test]
fn r_sync_2_a_chunk_with_a_stale_or_missing_lease_token_is_refused() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let account = AccountKey::generate();
    let ((a, client, _), (b, cb, _)) = (machine(&api, &account), machine(&api, &account));
    let id: [u8; 16] = e2e::random();
    let key = SessionKey::generate();
    client.put_sync_session(&uuid(&id), &e2e::hex(&e2e::wrap_session_key(&key, &id, &UserKey::first())), None, None, None).unwrap();
    let held = client.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "production").unwrap();
    let moved = client.renew_lease(&uuid(&id), &b.id().to_string(), &held.token, 60, "production").unwrap();

    let sealed = e2e::ChunkSealer::new(&key, id, 0, e2e::NO_PREVIOUS_CHUNK, held.fence).seal(b"a stale holder's turn", false).unwrap();
    for token in [held.token.as_str(), ""] {
        let e = cb.put_chunk(&uuid(&id), 0, &sealed, token).unwrap_err();
        assert_eq!(e.code(), "lease_stale", "{:?}", e.body);
        assert!(e.fix().contains("acquire it again"), "{:?}", e.body);
    }
    assert!(client.list_chunks(&uuid(&id), None, 50).unwrap().chunks.is_empty());

    // Its token, from the device it was handed to, is not the holder's.
    assert_eq!(client.put_chunk(&uuid(&id), 0, &sealed, &moved.token).unwrap_err().code(), "lease_stale");
    cb.put_chunk(&uuid(&id), 0, &sealed, &moved.token).unwrap();
    let again = cb.put_chunk(&uuid(&id), 0, &sealed, &moved.token).unwrap_err();
    assert_eq!(again.code(), "chunk_exists");
    // A chunk is no artifact: the listing a push reads does not have it.
    let artifacts = client.list_artifacts("", 50).unwrap();
    assert!(artifacts.artifacts.is_empty(), "{artifacts:?}");
}

/// The stand-in holds the registry's limits: the session cap, the burst
/// ceiling on creates, and revocation by the owner's reset.
#[test]
fn the_stand_in_models_the_session_cap_the_burst_ceiling_and_revocation() {
    let config = krowk_devregistry::Config { max_sessions: 1, ..Default::default() };
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).unwrap();
    let account = AccountKey::generate();
    let (device, signing) = (e2e::DeviceKey::generate(), e2e::SigningKey::generate());
    let signing_public = e2e::hex(&signing.public().0);
    let mut client = krowk_api::Client::new(&format!("{}/v1", registry.url()), TOKEN).signed_by(e2e::DeviceSigner::new(device.id(), signing).shared());
    // A 429 is retried after its Retry-After; the minute is not waited out here.
    client.sleep = |_| {};
    let public = e2e::hex(&device.public().0);
    client.register_device(&public, &signing_public, "laptop", &account.id().to_string()).unwrap();
    let wrapped = |id: &[u8; 16]| e2e::hex(&e2e::wrap_session_key(&SessionKey::generate(), id, &UserKey::first()));

    let first: [u8; 16] = e2e::random();
    client.put_sync_session(&uuid(&first), &wrapped(&first), None, None, None).unwrap();
    let second: [u8; 16] = e2e::random();
    assert_eq!(client.put_sync_session(&uuid(&second), &wrapped(&second), None, None, None).unwrap_err().code(), "session_limit_reached");

    // The owner's reset revokes the device: it cannot act until it registers again.
    let mut conn = TcpStream::connect(registry.addr()).unwrap();
    write!(conn, "POST /_reset/sync HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert_eq!(client.acquire_lease(&uuid(&first), &device.id().to_string(), 60, "production").unwrap_err().code(), "device_revoked");
    // The reset cleared the pin too, so registering again may pin another key.
    let fresh = AccountKey::generate().id().to_string();
    client.register_device(&public, &signing_public, "laptop", &fresh).unwrap();
    client.acquire_lease(&uuid(&first), &device.id().to_string(), 60, "production").unwrap();

    // 120 creates a minute, then 429 (two spent above).
    // Each named apart, so no two are the same signed request a moment apart.
    let refused = (0..125).find_map(|i| client.register_device(&public, &signing_public, &format!("laptop {i}"), &fresh).err()).expect("the ceiling was met");
    assert_eq!(refused.code(), "too_many_requests");
}

/// R-SYNC-2, as the stand-in holds it: a pending chunk a displaced holder
/// declared is not the next holder's to finalize, does not block its index,
/// and a chunk is at most 64 MiB.
#[test]
fn r_sync_2_a_displaced_holders_pending_chunk_is_replaced_not_finalized() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let account = AccountKey::generate();
    let ((a, client, sa), (b, cb, sb)) = (machine(&api, &account), machine(&api, &account));
    let id: [u8; 16] = e2e::random();
    let key = SessionKey::generate();
    let session = uuid(&id);
    client.put_sync_session(&session, &e2e::hex(&e2e::wrap_session_key(&key, &id, &UserKey::first())), None, None, None).unwrap();

    // A declares chunk 0 by hand and is displaced before finalizing it.
    let held = client.acquire_lease(&session, &a.id().to_string(), 60, "production").unwrap();
    let stale = e2e::ChunkSealer::new(&key, id, 0, e2e::NO_PREVIOUS_CHUNK, held.fence).seal(b"A's", false).unwrap();
    let declare = |by: &dyn RequestSigner, token: &str, blob: &[u8], size: usize| {
        let body = serde_json::json!({ "chunk": { "index": 0, "byte_size": size, "checksum": e2e::hex(&sha(blob)), "lease_token": token } });
        raw_as(registry.addr(), Some(by), "POST", &format!("/v1/sessions/{session}/chunks"), &body.to_string())
    };
    assert!(declare(&*sa, &held.token, &stale, stale.len()).starts_with("HTTP/1.1 201"));
    let moved = client.renew_lease(&session, &b.id().to_string(), &held.token, 60, "production").unwrap();
    let fin = raw_as(registry.addr(), Some(&*sb), "PUT", &format!("/v1/sessions/{session}/chunks/0/finalization"), &serde_json::json!({ "chunk": { "lease_token": moved.token } }).to_string());
    assert!(fin.starts_with("HTTP/1.1 409") && fin.contains("lease_stale"), "{fin}");

    // B's own chunk 0 replaces it and lands.
    let mine = e2e::ChunkSealer::new(&key, id, 0, e2e::NO_PREVIOUS_CHUNK, moved.fence).seal(b"B's", false).unwrap();
    cb.put_chunk(&session, 0, &mine, &moved.token).unwrap();
    let listed = client.list_chunks(&session, None, 50).unwrap();
    assert_eq!(e2e::ChunkReader::new(&key, id).open(&client.read_chunk(&listed.chunks[0]).unwrap()).unwrap(), b"B's");

    let too_big = declare(&*sb, &moved.token, b"x", (64 << 20) + 1);
    assert!(too_big.starts_with("HTTP/1.1 422") && too_big.contains("byte_size"), "{too_big}");
}

fn sha(b: &[u8]) -> [u8; 32] {
    e2e::chunk_digest(b)
}

/// A device of the workspace and a client acting as it: registered through
/// a client signed by its own key (crypto.md → Signed registry requests),
/// as `krowk sync init` registers one, and its signer for raw calls.
fn machine(api: &str, account: &AccountKey) -> (e2e::DeviceKey, krowk_api::Client, Arc<dyn RequestSigner>) {
    let (device, signing) = (e2e::DeviceKey::generate(), e2e::SigningKey::generate());
    let signing_public = e2e::hex(&signing.public().0);
    let signer = e2e::DeviceSigner::new(device.id(), signing).shared();
    let client = krowk_api::Client::new(api, TOKEN).signed_by(signer.clone());
    client.register_device(&e2e::hex(&device.public().0), &signing_public, "machine", &account.id().to_string()).unwrap();
    (device, client, signer)
}

/// One request to the stand-in, keyed, and its whole answer.
fn raw(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
    raw_as(addr, None, method, path, body)
}

/// As `raw`, signed by `signer` when there is one, as krowk-api signs.
fn raw_as(addr: std::net::SocketAddr, signer: Option<&dyn RequestSigner>, method: &str, path: &str, body: &str) -> String {
    let signed = signer.map_or(String::new(), |s| {
        let at = jiff::Timestamp::now().as_millisecond().to_string();
        let signature = e2e::hex(&s.sign(&krowk_api::client::signing_input(method, path, &at, body.as_bytes())));
        format!("X-Krowk-Device: {}\r\nX-Krowk-Timestamp: {at}\r\nX-Krowk-Signature: {signature}\r\n", s.device())
    });
    let mut conn = TcpStream::connect(addr).unwrap();
    write!(conn, "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n{signed}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    answer
}

/// R-RELAY-1: a device's signing key is required and set once — a
/// register without one, or naming another, is refused — so no other key
/// of the workspace can swap in a key it holds and stand in the device's
/// relay channels.
#[test]
fn r_relay_1_a_signing_key_is_required_and_set_once() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("signing");
    let laptop = r.join("laptop");
    let set_up = set_up(&laptop, &api, TOKEN);
    assert_eq!(set_up["data"]["registered"], true, "{set_up}");
    let keys = Keystore::new(&laptop.join(".krowk"));
    let public = e2e::hex(&keys.device().unwrap().unwrap().public().0);
    let account = keys.account_id().unwrap().unwrap().to_string();
    let signer = e2e::DeviceSigner::new(keys.device().unwrap().unwrap().id(), keys.signing_key().unwrap()).shared();
    let client = krowk_api::Client::new(&api, TOKEN).signed_by(signer.clone());
    let held = client.list_devices().unwrap().remove(0).signing_key;
    assert_eq!(held, e2e::hex(&keys.signing_key().unwrap().public().0));

    let answer = raw_as(registry.addr(), Some(&*signer), "POST", "/v1/devices", &serde_json::json!({"device": {"public_key": public, "name": "laptop", "account_key_id": account}}).to_string());
    assert!(answer.starts_with("HTTP/1.1 400") && answer.contains("signing_key"), "{answer}");
    let answer = raw(registry.addr(), "POST", "/v1/device_approvals", &serde_json::json!({"device_approval": {"public_key": e2e::hex(&[3; 32]), "name": "desktop"}}).to_string());
    assert!(answer.starts_with("HTTP/1.1 400") && answer.contains("signing_key"), "{answer}");

    let refused = client.register_device(&public, &e2e::hex(&[9; 32]), "laptop", &account).unwrap_err();
    assert_eq!((refused.status, refused.code().to_string()), (409, "signing_key_mismatch".to_string()), "{refused:?}");
    assert_eq!(client.list_devices().unwrap()[0].signing_key, held);
    let _ = std::fs::remove_dir_all(&r);
}

/// R-RELAY-1: the registry vouches for a device at the relay with tickets
/// (relay.md → Tickets): a host's comes with the lease, at its fence, and
/// goes to the new holder at a hand-over; a viewer's is asked for, by a
/// device of the workspace that is not revoked. Each verifies under the
/// registry's key and names the device's own signing key.
#[test]
fn r_relay_1_leases_and_viewers_are_issued_tickets_the_relay_can_check() {
    use krowk_client::relay_ticket;
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let keys = vec![(krowk_devregistry::TICKET_KID, krowk_devregistry::ticket_public_key())];
    let account = AccountKey::generate();
    let (a, b) = (e2e::DeviceKey::generate(), e2e::DeviceKey::generate());
    let (sa, sb) = (e2e::SigningKey::generate(), e2e::SigningKey::generate());
    let (pa, pb) = (sa.public(), sb.public());
    let client = krowk_api::Client::new(&api, TOKEN).signed_by(e2e::DeviceSigner::new(a.id(), e2e::SigningKey::from_secret(&*sa.secret_bytes()).unwrap()).shared());
    let cb = krowk_api::Client::new(&api, TOKEN).signed_by(e2e::DeviceSigner::new(b.id(), e2e::SigningKey::from_secret(&*sb.secret_bytes()).unwrap()).shared());
    for (d, s, c) in [(&a, &pa, &client), (&b, &pb, &cb)] {
        c.register_device(&e2e::hex(&d.public().0), &e2e::hex(&s.0), "machine", &account.id().to_string()).unwrap();
    }
    let id: [u8; 16] = e2e::random();
    client.put_sync_session(&uuid(&id), &e2e::hex(&e2e::wrap_session_key(&SessionKey::generate(), &id, &UserKey::first())), None, None, None).unwrap();

    let held = client.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "development").unwrap();
    let t = relay_ticket::verify(&held.relay_ticket, &keys, relay_ticket::now()).unwrap();
    assert_eq!((t.role, t.env, t.fence, t.device, t.signing_key, t.session), (e2e::RELAY_ROLE_HOST, relay_ticket::ENV_DEVELOPMENT, held.fence, a.id().0, sa.public().0, id));
    let moved = client.renew_lease(&uuid(&id), &b.id().to_string(), &held.token, 60, "production").unwrap();
    let t = relay_ticket::verify(&moved.relay_ticket, &keys, relay_ticket::now()).unwrap();
    assert_eq!((t.device, t.fence, t.env), (b.id().0, held.fence + 1, relay_ticket::ENV_PRODUCTION), "the new holder's, at the new fence");

    let watch = client.relay_ticket(&uuid(&id), &a.id().to_string(), "production").unwrap();
    // A ticket is the named device's own to ask for: another device of the
    // workspace, signing as itself, is refused one in its name.
    assert_eq!(cb.relay_ticket(&uuid(&id), &a.id().to_string(), "production").unwrap_err().code(), "device_mismatch");
    let t = relay_ticket::verify(&watch.relay_ticket, &keys, relay_ticket::now()).unwrap();
    assert_eq!((t.role, t.fence, t.signing_key), (e2e::RELAY_ROLE_VIEWER, 0, sa.public().0));
    assert_eq!(client.relay_ticket(&uuid(&id), &a.id().to_string(), "staging").unwrap_err().code(), "invalid");
    let other: [u8; 16] = e2e::random();
    assert_eq!(client.relay_ticket(&uuid(&other), &a.id().to_string(), "production").unwrap_err().status, 404);
}

/// R-RELAY-1, R-E2E-3: one waiting approval request per device key, and
/// the code a person compares covers the signing key too — a request with
/// the new device's X25519 key and another signing key is refused while
/// the first waits, and would show another code anyway; the approving
/// machine approves only the request whose code is exactly the one typed.
#[test]
fn r_relay_1_an_approval_request_cannot_be_doubled_or_approved_by_its_device_key_alone() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let client = krowk_api::Client::new(&api, TOKEN);
    let device = e2e::DeviceKey::generate();
    let (own, other) = (e2e::SigningKey::generate(), e2e::SigningKey::generate());
    let public = e2e::hex(&device.public().0);
    client.request_device_approval(&public, &e2e::hex(&own.public().0), "desktop").unwrap();
    // Asked again the same way: the same request.
    client.request_device_approval(&public, &e2e::hex(&own.public().0), "desktop").unwrap();
    let doubled = client.request_device_approval(&public, &e2e::hex(&other.public().0), "desktop").unwrap_err();
    assert_eq!((doubled.status, doubled.code().to_string()), (409, "approval_pending".to_string()), "{doubled:?}");
    assert_eq!(client.list_device_approvals().unwrap().len(), 1);
    assert_ne!(e2e::approval_code(&device.public(), &own.public()), e2e::approval_code(&device.public(), &other.public()));
    assert_ne!(e2e::approval_code(&device.public(), &own.public()), device.public().id(), "the code is not the device id");

    // The laptop that approves: the device id alone, the code before
    // signing keys, is no code at all.
    let r = root("doubled");
    let laptop = r.join("laptop");
    set_up(&laptop, &api, TOKEN);
    let by_id = run(&laptop, &api, TOKEN, &["devices", "approve", &device.public().id().to_string(), "--json"], "");
    assert!(!by_id.status.success() && String::from_utf8_lossy(&by_id.stderr).contains("no_such_device_code"), "{}", String::from_utf8_lossy(&by_id.stderr));
    let code = e2e::approval_code(&device.public(), &own.public()).to_string();
    let approved = json(&run(&laptop, &api, TOKEN, &["devices", "approve", &code, "--json"], ""));
    assert_eq!(approved["data"]["device"], device.public().id().to_string(), "{approved}");
    let row = client.list_devices().unwrap().into_iter().find(|d| d.id == device.public().id().to_string()).unwrap();
    assert_eq!(row.signing_key, e2e::hex(&own.public().0), "the approval registered the requesting device's own signing key");
    let _ = std::fs::remove_dir_all(&r);
}

/// Ticket 16c, as the stand-in holds it: a call acting as a device is
/// signed by that device's key. The API key alone — unsigned, or signed by
/// a key that is not the device's — acquires nothing and mints nothing; a
/// signed request sent twice, or signed outside five minutes, is refused;
/// and a revoked device's signed calls are refused.
#[test]
fn signed_calls_refuse_the_api_key_alone_a_replay_a_stale_signature_and_a_revoked_device() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let account = AccountKey::generate();
    let (a, client, signer) = machine(&api, &account);
    let id: [u8; 16] = e2e::random();
    client.put_sync_session(&uuid(&id), &e2e::hex(&e2e::wrap_session_key(&SessionKey::generate(), &id, &UserKey::first())), None, None, None).unwrap();

    let unsigned = krowk_api::Client::new(&api, TOKEN);
    assert_eq!(unsigned.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "production").unwrap_err().code(), "device_signature_missing");
    let lease = format!("/v1/sessions/{}/lease", uuid(&id));
    let body = serde_json::json!({ "lease": { "device": a.id().to_string() } }).to_string();
    assert!(raw(registry.addr(), "POST", &lease, &body).contains("signature_required"));
    let stranger = e2e::DeviceSigner::new(a.id(), e2e::SigningKey::generate());
    assert!(raw_as(registry.addr(), Some(&stranger), "POST", &lease, &body).contains("signature_invalid"));
    let ticket = format!("/v1/sessions/{}/relay_ticket?device={}", uuid(&id), a.id());
    assert!(raw_as(registry.addr(), Some(&stranger), "GET", &ticket, "").contains("signature_invalid"));

    // The same signed request twice: the second is a replay.
    let at = jiff::Timestamp::now().as_millisecond().to_string();
    let send = |at: &str| {
        let signature = e2e::hex(&signer.sign(&krowk_api::client::signing_input("GET", &ticket, at, b"")));
        let mut conn = TcpStream::connect(registry.addr()).unwrap();
        write!(conn, "GET {ticket} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nX-Krowk-Device: {}\r\nX-Krowk-Timestamp: {at}\r\nX-Krowk-Signature: {signature}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", a.id()).unwrap();
        let mut answer = String::new();
        conn.read_to_string(&mut answer).unwrap();
        answer
    };
    assert!(send(&at).starts_with("HTTP/1.1 200"));
    assert!(send(&at).contains("signature_replayed"));
    let stale = (jiff::Timestamp::now().as_millisecond() - 6 * 60 * 1000).to_string();
    assert!(send(&stale).contains("signature_stale"));

    // Revoked by the owner's reset: its own signed calls are refused.
    assert!(raw(registry.addr(), "POST", "/_reset/sync", "").starts_with("HTTP/1.1 200"));
    assert_eq!(client.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "production").unwrap_err().code(), "device_revoked");
}
