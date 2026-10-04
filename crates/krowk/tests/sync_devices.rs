//! Sync's devices and sessions against the stand-in registry, on scratch
//! homes (R-E2E-3, R-SYNC-1, R-SYNC-2, R-RELAY-1): setup on and off the
//! registry, leases and chunks, signing keys and relay tickets. Adding a
//! device by pairing is `sync_pairing.rs`.

#![cfg(all(feature = "harness", unix))]

use krowk_api::client::RequestSigner;
use krowk_client::e2e::{self, AccountKey, SessionKey};
use krowk_client::keystore::Keystore;
use krowk_client::user_key::UserKey;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

#[path = "common/device_list.rs"]
mod device_list;
use device_list::People;

const TOKEN: &str = "krowk_sk_sync_devices";

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-devices-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["laptop", "desktop"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    root.canonicalize().unwrap()
}

/// krowk with `home` as HOME and nothing else of this machine's.
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

/// Sync set up in `home`: an account key and the device's own keys, and
/// the device alone on the person's device list in the registry, `token`
/// bound to it — as `krowk sync init` leaves a machine.
fn set_up(home: &Path, api: &str, token: &str) -> krowk_api::Client {
    let keys = Keystore::new(&home.join(".krowk"));
    keys.recover(AccountKey::generate()).unwrap();
    device_list::start(api, token, &keys, "laptop")
}

fn uuid(b: &[u8; 16]) -> String {
    let h = e2e::hex(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// R-SYNC-1: a free workspace is refused every sync call with a fix.
#[test]
fn r_sync_1_a_free_workspace_is_refused_with_a_fix() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("free");
    let free = "krowk_sk_free_workspace";

    let refused = krowk_api::Client::new(&api, free).device_list_all().unwrap_err();
    assert!(refused.code() == "sync_requires_paid_plan" && refused.fix().contains("upgrade this workspace to Pro"), "{refused:?}");

    let _ = std::fs::remove_dir_all(&r);
}

/// R-SYNC-2: a second device cannot take a lease that is held, and the
/// holder's stale fence is refused once the lease has moved on.
#[test]
fn r_sync_2_a_stale_lease_holders_write_is_refused() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let (ma, mb) = two_machines(&api, "stale-holder");
    let ((a, client), (b, cb)) = ((&ma.device, &ma.client), (&mb.device, &mb.client));
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
    set_up(&laptop, &api, TOKEN);

    for (home, args) in [(&laptop, vec!["devices", "add", "--json"]), (&desktop, vec!["sync", "join", "--json"])] {
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
    let (ma, mb) = two_machines(&api, "stale-chunk");
    let ((a, client), (b, cb)) = ((&ma.device, &ma.client), (&mb.device, &mb.client));
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
/// ceiling on creates, and revocation by the dashboard's Revoke.
#[test]
fn the_stand_in_models_the_session_cap_the_burst_ceiling_and_revocation() {
    let config = krowk_devregistry::Config { max_sessions: 1, ..Default::default() };
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).unwrap();
    let api = format!("{}/v1", registry.url());
    let (ma, mb) = two_machines(&api, "limits");
    let (device, mut client) = (&ma.device, ma.client);
    // A 429 is retried after its Retry-After; the minute is not waited out here.
    client.sleep = |_| {};
    let wrapped = |id: &[u8; 16]| e2e::hex(&e2e::wrap_session_key(&SessionKey::generate(), id, &UserKey::first()));

    let first: [u8; 16] = e2e::random();
    client.put_sync_session(&uuid(&first), &wrapped(&first), None, None, None).unwrap();
    let second: [u8; 16] = e2e::random();
    assert_eq!(client.put_sync_session(&uuid(&second), &wrapped(&second), None, None, None).unwrap_err().code(), "session_limit_reached");

    // 120 creates a minute, then 429 (two spent above). Each a session of
    // its own, so no two are the same signed request a moment apart.
    let refused = (0..125)
        .find_map(|_| {
            let id: [u8; 16] = e2e::random();
            client.put_sync_session(&uuid(&id), &wrapped(&id), None, None, None).err().filter(|e| e.code() != "session_limit_reached")
        })
        .expect("the ceiling was met");
    assert_eq!(refused.code(), "too_many_requests");

    // The dashboard's Revoke: the device is refused, and its key with it,
    // reads as well as writes.
    revoke(registry.addr(), &ma.token, &device.id().to_string());
    assert_eq!(client.acquire_lease(&uuid(&first), &device.id().to_string(), 60, "production").unwrap_err().code(), "unauthorized");
    assert_eq!(client.show_sync_session(&uuid(&first)).unwrap_err().code(), "unauthorized");
    // Another of the person's keys, signed by the revoked device, is
    // refused for the device.
    let other = krowk_api::Client::new(&api, &mb.token).signed_by(ma.signer.clone());
    assert_eq!(other.show_sync_session(&uuid(&first)).unwrap_err().code(), "device_revoked");
    mb.client.show_sync_session(&uuid(&first)).unwrap();
}

/// R-SYNC-2, as the stand-in holds it: a pending chunk a displaced holder
/// declared is not the next holder's to finalize, does not block its index,
/// and a chunk is at most 64 MiB.
#[test]
fn r_sync_2_a_displaced_holders_pending_chunk_is_replaced_not_finalized() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let (ma, mb) = two_machines(&api, "displaced");
    let ((a, client), (b, cb)) = ((&ma.device, &ma.client), (&mb.device, &mb.client));
    let id: [u8; 16] = e2e::random();
    let key = SessionKey::generate();
    let session = uuid(&id);
    client.put_sync_session(&session, &e2e::hex(&e2e::wrap_session_key(&key, &id, &UserKey::first())), None, None, None).unwrap();

    // A declares chunk 0 by hand and is displaced before finalizing it.
    let held = client.acquire_lease(&session, &a.id().to_string(), 60, "production").unwrap();
    let stale = e2e::ChunkSealer::new(&key, id, 0, e2e::NO_PREVIOUS_CHUNK, held.fence).seal(b"A's", false).unwrap();
    let declare = |by: &Machine, token: &str, blob: &[u8], size: usize| {
        let body = serde_json::json!({ "chunk": { "index": 0, "byte_size": size, "checksum": e2e::hex(&sha(blob)), "lease_token": token } });
        raw_as(registry.addr(), &by.token, Some(&*by.signer), "POST", &format!("/v1/sessions/{session}/chunks"), &body.to_string())
    };
    assert!(declare(&ma, &held.token, &stale, stale.len()).starts_with("HTTP/1.1 201"));
    let moved = client.renew_lease(&session, &b.id().to_string(), &held.token, 60, "production").unwrap();
    let fin = raw_as(registry.addr(), &mb.token, Some(&*mb.signer), "PUT", &format!("/v1/sessions/{session}/chunks/0/finalization"), &serde_json::json!({ "chunk": { "lease_token": moved.token } }).to_string());
    assert!(fin.starts_with("HTTP/1.1 409") && fin.contains("lease_stale"), "{fin}");

    // B's own chunk 0 replaces it and lands.
    let mine = e2e::ChunkSealer::new(&key, id, 0, e2e::NO_PREVIOUS_CHUNK, moved.fence).seal(b"B's", false).unwrap();
    cb.put_chunk(&session, 0, &mine, &moved.token).unwrap();
    let listed = client.list_chunks(&session, None, 50).unwrap();
    assert_eq!(e2e::ChunkReader::new(&key, id).open(&client.read_chunk(&listed.chunks[0]).unwrap()).unwrap(), b"B's");

    let too_big = declare(&mb, &moved.token, b"x", (64 << 20) + 1);
    assert!(too_big.starts_with("HTTP/1.1 422") && too_big.contains("byte_size"), "{too_big}");
}

fn sha(b: &[u8]) -> [u8; 32] {
    e2e::chunk_digest(b)
}

/// One of the person's machines: its device, the key it holds, a client
/// acting as it and its signer for raw calls.
struct Machine {
    device: e2e::DeviceKey,
    /// Its signing key's public half, which its list entry carries.
    signing: [u8; 32],
    token: String,
    client: krowk_api::Client,
    signer: Arc<dyn RequestSigner>,
}

/// A machine of the person's, a home of its own under `root`: its device on
/// the person's device list, and a key of its own that speaks for it, as
/// `krowk sync init` or `sync join` leaves one. Its client is signed by the
/// device's own key (crypto.md → Signed registry requests).
fn machine(api: &str, people: &mut People, root: &Path, name: &str) -> Machine {
    let keys = Keystore::new(&root.join(name).join(".krowk"));
    let (device, signing) = (keys.device_key().unwrap(), keys.signing_key().unwrap());
    people.enlist(&keys, name);
    people.publish(api, TOKEN);
    let client = people.client(api, TOKEN, people.len() - 1);
    let public = signing.public().0;
    let signer = e2e::DeviceSigner::new(device.id(), signing).shared();
    Machine { device, signing: public, token: device_list::key(TOKEN, name), client, signer }
}

/// The laptop and the desktop, in that order: the laptop's device starts
/// the list, and adds the desktop's.
fn two_machines(api: &str, name: &str) -> (Machine, Machine) {
    let (mut people, r) = (People::default(), root(name));
    (machine(api, &mut people, &r, "laptop"), machine(api, &mut people, &r, "desktop"))
}

/// One request to the stand-in on `token`, signed by `signer` when there is
/// one, as krowk-api signs, and its whole answer.
fn raw_as(addr: std::net::SocketAddr, token: &str, signer: Option<&dyn RequestSigner>, method: &str, path: &str, body: &str) -> String {
    let signed = signer.map_or(String::new(), |s| {
        let at = jiff::Timestamp::now().as_millisecond().to_string();
        let signature = e2e::hex(&s.sign(&krowk_api::client::signing_input(method, path, &at, body.as_bytes())));
        format!("X-Krowk-Device: {}\r\nX-Krowk-Timestamp: {at}\r\nX-Krowk-Signature: {signature}\r\n", s.device())
    });
    let mut conn = TcpStream::connect(addr).unwrap();
    write!(conn, "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n{signed}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    answer
}

/// The dashboard's Revoke of `device`, by the person `token` is a key of.
fn revoke(addr: std::net::SocketAddr, token: &str, device: &str) {
    let answer = raw_as(addr, token, None, "POST", &format!("/_settings/devices/{device}/revocation"), "");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
}

/// R-RELAY-1: a device's signing key is the one its list entry carries,
/// and the relay tickets the registry issues name it. Registering one the
/// account key's way — which any key of the workspace could call, naming
/// any key — is gone, so no other key can swap in a key it holds and stand
/// in the device's relay channels.
#[test]
fn r_relay_1_a_signing_key_is_the_lists_and_cannot_be_swapped() {
    use krowk_client::relay_ticket;
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("signing");
    let laptop = r.join("laptop");
    let client = set_up(&laptop, &api, TOKEN);
    let ks = Keystore::new(&laptop.join(".krowk"));
    let device = ks.device().unwrap().unwrap();
    let held = ks.signing_key().unwrap().public().0;
    let account = ks.account_id().unwrap().unwrap().to_string();
    let signer = e2e::DeviceSigner::new(device.id(), ks.signing_key().unwrap()).shared();
    let id: [u8; 16] = e2e::random();
    client.put_sync_session(&uuid(&id), &e2e::hex(&e2e::wrap_session_key(&SessionKey::generate(), &id, &UserKey::first())), None, None, None).unwrap();
    let keys = vec![(krowk_devregistry::TICKET_KID, krowk_devregistry::ticket_public_key())];
    let ticketed = || {
        let t = client.relay_ticket(&uuid(&id), &device.id().to_string(), "production").unwrap();
        relay_ticket::verify(&t.relay_ticket, &keys, relay_ticket::now()).unwrap().signing_key
    };
    assert_eq!(ticketed(), held);

    let swap = serde_json::json!({"device": {"public_key": e2e::hex(&device.public().0), "signing_key": e2e::hex(&[9; 32]), "name": "laptop", "account_key_id": account}});
    let answer = raw_as(registry.addr(), TOKEN, Some(&*signer), "POST", "/v1/devices", &swap.to_string());
    assert!(answer.starts_with("HTTP/1.1 410") && answer.contains("sync_reset"), "{answer}");
    assert_eq!(ticketed(), held, "the tickets still name the list's key");
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
    let (ma, mb) = two_machines(&api, "tickets");
    let ((a, client), (b, cb)) = ((&ma.device, &ma.client), (&mb.device, &mb.client));
    let id: [u8; 16] = e2e::random();
    client.put_sync_session(&uuid(&id), &e2e::hex(&e2e::wrap_session_key(&SessionKey::generate(), &id, &UserKey::first())), None, None, None).unwrap();

    let held = client.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "development").unwrap();
    let t = relay_ticket::verify(&held.relay_ticket, &keys, relay_ticket::now()).unwrap();
    assert_eq!((t.role, t.env, t.fence, t.device, t.signing_key, t.session), (e2e::RELAY_ROLE_HOST, relay_ticket::ENV_DEVELOPMENT, held.fence, a.id().0, ma.signing, id));
    let moved = client.renew_lease(&uuid(&id), &b.id().to_string(), &held.token, 60, "production").unwrap();
    let t = relay_ticket::verify(&moved.relay_ticket, &keys, relay_ticket::now()).unwrap();
    assert_eq!((t.device, t.fence, t.env), (b.id().0, held.fence + 1, relay_ticket::ENV_PRODUCTION), "the new holder's, at the new fence");

    let watch = client.relay_ticket(&uuid(&id), &a.id().to_string(), "production").unwrap();
    // A ticket is the named device's own to ask for: another device of the
    // workspace, signing as itself, is refused one in its name.
    assert_eq!(cb.relay_ticket(&uuid(&id), &a.id().to_string(), "production").unwrap_err().code(), "device_mismatch");
    let t = relay_ticket::verify(&watch.relay_ticket, &keys, relay_ticket::now()).unwrap();
    assert_eq!((t.role, t.fence, t.signing_key), (e2e::RELAY_ROLE_VIEWER, 0, ma.signing));
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
    let (ma, mb) = two_machines(&api, "signed");
    let (a, client, signer) = (&ma.device, &ma.client, &ma.signer);
    let id: [u8; 16] = e2e::random();
    client.put_sync_session(&uuid(&id), &e2e::hex(&e2e::wrap_session_key(&SessionKey::generate(), &id, &UserKey::first())), None, None, None).unwrap();

    let unsigned = krowk_api::Client::new(&api, &ma.token);
    assert_eq!(unsigned.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "production").unwrap_err().code(), "device_signature_missing");
    let lease = format!("/v1/sessions/{}/lease", uuid(&id));
    let body = serde_json::json!({ "lease": { "device": a.id().to_string() } }).to_string();
    assert!(raw_as(registry.addr(), &ma.token, None, "POST", &lease, &body).contains("signature_required"));
    let stranger = e2e::DeviceSigner::new(a.id(), e2e::SigningKey::generate());
    assert!(raw_as(registry.addr(), &ma.token, Some(&stranger), "POST", &lease, &body).contains("signature_invalid"));
    let ticket = format!("/v1/sessions/{}/relay_ticket?device={}", uuid(&id), a.id());
    assert!(raw_as(registry.addr(), &ma.token, Some(&stranger), "GET", &ticket, "").contains("signature_invalid"));

    // The same signed request twice: the second is a replay.
    let at = jiff::Timestamp::now().as_millisecond().to_string();
    let send = |at: &str| {
        let signature = e2e::hex(&signer.sign(&krowk_api::client::signing_input("GET", &ticket, at, b"")));
        let mut conn = TcpStream::connect(registry.addr()).unwrap();
        write!(conn, "GET {ticket} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nX-Krowk-Device: {}\r\nX-Krowk-Timestamp: {at}\r\nX-Krowk-Signature: {signature}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", ma.token, a.id()).unwrap();
        let mut answer = String::new();
        conn.read_to_string(&mut answer).unwrap();
        answer
    };
    assert!(send(&at).starts_with("HTTP/1.1 200"));
    assert!(send(&at).contains("signature_replayed"));
    let stale = (jiff::Timestamp::now().as_millisecond() - 6 * 60 * 1000).to_string();
    assert!(send(&stale).contains("signature_stale"));

    // Revoked by the dashboard's Revoke: its own signed calls are refused,
    // the key it holds revoked with it.
    revoke(registry.addr(), &ma.token, &a.id().to_string());
    assert_eq!(client.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "production").unwrap_err().code(), "unauthorized");
    // Signed by it on another of the person's keys, it is the device that
    // is refused.
    let as_revoked = krowk_api::Client::new(&api, &mb.token).signed_by(signer.clone());
    assert_eq!(as_revoked.acquire_lease(&uuid(&id), &a.id().to_string(), 60, "production").unwrap_err().code(), "device_revoked");
}
