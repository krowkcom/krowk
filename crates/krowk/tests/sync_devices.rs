//! Sync's devices and sessions against the stand-in registry, on scratch
//! homes (R-E2E-3, R-SYNC-1, R-SYNC-2, R-RELAY-1): setup on and off the
//! registry, leases and chunks, signing keys and relay tickets. Adding a
//! device by pairing is `sync_pairing.rs`.

#![cfg(all(feature = "harness", unix))]

use krowk_api::client::RequestSigner;
use krowk_client::e2e::{self, AccountKey, SessionKey};
use krowk_client::keystore::Keystore;
use krowk_client::user_key::UserKey;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

const TOKEN: &str = "krowk_sk_sync_devices";

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

fn uuid(b: &[u8; 16]) -> String {
    let h = e2e::hex(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
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

/// R-E2E-3: off a terminal, `devices add` and `sync join` refuse — a code
/// handed to an agent does not add a device — and nothing is made.
#[test]
fn r_e2e_3_add_and_join_refuse_without_a_person_at_a_terminal() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("headless");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let words = krowk_client::phrase::encode(&AccountKey::generate());
    json(&run(&laptop, &api, TOKEN, &["sync", "recover", "--json"], &format!("{}\n", *words)));

    for (home, args) in [(&laptop, vec!["devices", "add", "--json"]), (&desktop, vec!["sync", "join", "--json"])] {
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
    let words = krowk_client::phrase::encode(&AccountKey::generate());
    let set_up = json(&run(&laptop, &api, TOKEN, &["sync", "recover", "--json"], &format!("{}\n", *words)));
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
