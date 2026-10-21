//! Notifications and push tokens against the stand-in registry (R-IOS-12,
//! R-SYNC-1, R-SYNC-2): the lease holder asks for the person's other devices
//! to be rung, and a device sets and clears its own APNs token. The
//! stand-in makes the real registry's refusals, and records what was asked
//! (`GET /_notifications`, `GET /_push_tokens`, stand-in only) so a test can
//! see it.

use krowk_api::sync::{ListPost, NotifyKind};
use krowk_api::Client;
use krowk_client::device_chain::{Chain, Change, Kind, Subject};
use krowk_client::e2e::{hex, DeviceKey, DeviceSigner, SigningKey};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;

const TOKEN: &str = "krowk_sk_push";
const SESSION: &str = "0190f3a8-7c1e-7a9b-8c2d-3e4f5a6b7c8d";
const T0: u64 = 1_790_000_000;

/// One of the person's machines: its device, and its signing key's secret,
/// to sign as it.
struct Machine {
    device: DeviceKey,
    signing: [u8; 32],
    key: String,
}

impl Machine {
    fn new(name: &str) -> Machine {
        Machine { device: DeviceKey::generate(), signing: *SigningKey::generate().secret_bytes(), key: format!("{TOKEN}#{name}") }
    }

    fn id(&self) -> String {
        self.device.id().to_string()
    }

    fn signing(&self) -> SigningKey {
        SigningKey::from_secret(&self.signing).unwrap()
    }

    fn subject(&self, name: &str) -> Subject {
        Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: self.device.public(), signing: self.signing().public() }
    }

    /// The registry as `key`, signed by this machine's device.
    fn client_as(&self, url: &str, key: &str) -> Client {
        Client::new(url, key).signed_by(DeviceSigner::new(self.device.id(), self.signing()).shared())
    }

    fn client(&self, url: &str) -> Client {
        self.client_as(url, &self.key)
    }
}

/// The stand-in, the laptop that started the person's device list (its key
/// bound to it, as `krowk sync init` leaves it) and the phone it added (its
/// own key bound to it, as `krowk sync join` leaves it).
struct Person {
    _registry: krowk_devregistry::Running,
    addr: std::net::SocketAddr,
    url: String,
    laptop: Machine,
    phone: Machine,
}

fn person() -> Person {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let url = format!("{}/v1", registry.url());
    let (laptop, phone) = (Machine::new("laptop"), Machine::new("phone"));
    let (chain, start) = Chain::start(laptop.subject("laptop"), &laptop.signing(), None, T0).unwrap();
    let user = start.newest;
    let entries = |b: &[krowk_client::device_chain::SignedEntry]| b.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect();
    let init = ListPost { entries: entries(&start.entries), links: Vec::new(), wraps: vec![(laptop.id(), hex(&user.wrap_to(&laptop.device.public()).unwrap()))], start_over: false };
    laptop.client(&url).init_device_list(&init).unwrap();
    let (_, added) = chain.batch(&user, vec![Change::Add(phone.subject("phone"))], laptop.device.id(), &laptop.signing(), T0 + 1).unwrap();
    let append = ListPost { entries: entries(&added.entries), links: Vec::new(), wraps: vec![(phone.id(), hex(&user.wrap_to(&phone.device.public()).unwrap()))], start_over: false };
    laptop.client(&url).append_device_list(&append).unwrap();
    phone.client(&url).claim_key_device().unwrap();
    Person { addr: registry.addr(), _registry: registry, url, laptop, phone }
}

/// A stand-in-only read, over HTTP as another crate's test reaches it.
fn seen(addr: std::net::SocketAddr, path: &str) -> Value {
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    write!(conn, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    serde_json::from_str(&answer[answer.find("\r\n\r\n").unwrap() + 4..]).unwrap()
}

/// R-IOS-12: the lease holder's notification is recorded once, naming the
/// session, the kind, the dedup and the device; the same dedup again is a
/// duplicate, whatever kind it says, and another dedup is a new event.
#[test]
fn r_ios_12_a_lease_holders_notification_is_recorded_once_and_a_repeat_is_a_duplicate() {
    let p = person();
    let host = p.laptop.client(&p.url);
    host.put_sync_session(SESSION, &"00".repeat(74), None, None, None).unwrap();
    let lease = host.acquire_lease(SESSION, &p.laptop.id(), 60, "production").unwrap();
    let (first, second) = ("ab".repeat(32), "cd".repeat(32));

    assert!(host.notify_session(SESSION, NotifyKind::Approval, &first, &lease.token).unwrap(), "a new event");
    assert!(!host.notify_session(SESSION, NotifyKind::Approval, &first, &lease.token).unwrap(), "a repeat");
    assert!(!host.notify_session(SESSION, NotifyKind::Done, &first, &lease.token).unwrap(), "a repeat under another kind");
    assert!(host.notify_session(SESSION, NotifyKind::Done, &second, &lease.token).unwrap(), "another event");

    let recorded = seen(p.addr, "/_notifications");
    let want = serde_json::json!({ "notifications": [
        { "session": SESSION, "kind": "approval", "dedup": first, "device": p.laptop.id() },
        { "session": SESSION, "kind": "done", "dedup": second, "device": p.laptop.id() },
    ] });
    assert_eq!(recorded, want);

    // A dedup that is not 64 lowercase hex characters names the field.
    for bad in ["AB".repeat(32), "ab".repeat(31), "zz".repeat(32)] {
        let e = host.notify_session(SESSION, NotifyKind::Done, &bad, &lease.token).unwrap_err();
        assert_eq!((e.code().as_str(), e.status), ("invalid", 422), "{bad}");
        assert!(e.body.get("details").and_then(|d| d.get("dedup")).is_some(), "{e:?}");
    }
    assert_eq!(seen(p.addr, "/_notifications")["notifications"].as_array().unwrap().len(), 2);
}

/// R-SYNC-2: only the lease holder notifies, signed by itself and under its
/// live token: another device of the person's, even holding the token, a
/// wrong token, no token and a session nobody holds are each `lease_stale`,
/// and nothing is recorded.
#[test]
fn r_sync_2_a_device_without_the_lease_is_refused_lease_stale() {
    let p = person();
    let host = p.laptop.client(&p.url);
    let dedup = "ef".repeat(32);
    host.put_sync_session(SESSION, &"00".repeat(74), None, None, None).unwrap();
    let nobody = host.notify_session(SESSION, NotifyKind::Done, &dedup, "no-lease").unwrap_err();
    assert_eq!((nobody.code().as_str(), nobody.status), ("lease_stale", 409));
    assert!(!nobody.fix().is_empty(), "{nobody:?}");

    let lease = host.acquire_lease(SESSION, &p.laptop.id(), 60, "production").unwrap();
    let phone = p.phone.client(&p.url);
    assert_eq!(phone.notify_session(SESSION, NotifyKind::Approval, &dedup, &lease.token).unwrap_err().code(), "lease_stale", "the token copied to another device");
    assert_eq!(host.notify_session(SESSION, NotifyKind::Approval, &dedup, &"0".repeat(32)).unwrap_err().code(), "lease_stale", "a token not the lease's");
    assert_eq!(host.notify_session(SESSION, NotifyKind::Approval, &dedup, "").unwrap_err().code(), "lease_stale", "no token");
    host.release_lease(SESSION, &lease.token).unwrap();
    assert_eq!(host.notify_session(SESSION, NotifyKind::Approval, &dedup, &lease.token).unwrap_err().code(), "lease_stale", "a released lease");
    let other = "0190f3a8-7c1e-7a9b-8c2d-000000000000";
    assert_eq!(host.notify_session(other, NotifyKind::Done, &dedup, &lease.token).unwrap_err().status, 404, "no such session");
    assert_eq!(seen(p.addr, "/_notifications"), serde_json::json!({ "notifications": [] }));
}

/// R-IOS-12: a device registers its push token, replaces it and deletes it,
/// and a delete with none is fine. A token another device registers moves
/// there. What the stand-in shows never holds the token, and bad input names
/// its field.
#[test]
fn r_ios_12_a_device_registers_replaces_and_deletes_its_push_token() {
    let p = person();
    let (phone, laptop) = (p.phone.client(&p.url), p.laptop.client(&p.url));
    let token = "cd".repeat(32);
    let held = |want: Value| assert_eq!(seen(p.addr, "/_push_tokens"), serde_json::json!({ "push_tokens": want }));

    phone.put_push_token(&token, "sandbox").unwrap();
    held(serde_json::json!([{ "device": p.phone.id(), "environment": "sandbox" }]));
    assert!(!seen(p.addr, "/_push_tokens").to_string().contains(&token), "the token is never shown");
    phone.put_push_token(&token, "production").unwrap();
    held(serde_json::json!([{ "device": p.phone.id(), "environment": "production" }]));
    // The same token from the laptop moves it, as re-pairing a phone does.
    laptop.put_push_token(&token, "sandbox").unwrap();
    held(serde_json::json!([{ "device": p.laptop.id(), "environment": "sandbox" }]));
    phone.put_push_token(&"ef".repeat(100), "production").unwrap();
    laptop.delete_push_token().unwrap();
    held(serde_json::json!([{ "device": p.phone.id(), "environment": "production" }]));
    phone.delete_push_token().unwrap();
    phone.delete_push_token().unwrap();
    held(serde_json::json!([]));

    for (bad_token, environment, field) in [("ab".repeat(31), "sandbox", "token"), ("ab".repeat(101), "sandbox", "token"), ("xy".repeat(32), "sandbox", "token"), (format!("{}a", "ab".repeat(32)), "sandbox", "token"), (token.clone(), "staging", "environment")] {
        let e = phone.put_push_token(&bad_token, environment).unwrap_err();
        assert_eq!((e.code().as_str(), e.status), ("invalid", 422), "{bad_token} {environment}");
        assert!(e.body.get("details").and_then(|d| d.get(field)).is_some(), "{e:?}");
    }
    held(serde_json::json!([]));
}

/// R-IOS-12: only the device the key speaks for sets or clears its token: the
/// phone's key signed by the laptop is refused `device_mismatch`, and nothing
/// changes.
#[test]
fn a_push_token_call_signed_by_another_device_is_refused_device_mismatch() {
    let p = person();
    p.phone.client(&p.url).put_push_token(&"cd".repeat(32), "sandbox").unwrap();
    let crossed = p.laptop.client_as(&p.url, &p.phone.key);
    for e in [crossed.put_push_token(&"ab".repeat(32), "production").unwrap_err(), crossed.delete_push_token().unwrap_err()] {
        assert_eq!((e.code().as_str(), e.status), ("device_mismatch", 403), "{e:?}");
        assert!(!e.fix().is_empty(), "{e:?}");
    }
    assert_eq!(seen(p.addr, "/_push_tokens"), serde_json::json!({ "push_tokens": [{ "device": p.phone.id(), "environment": "sandbox" }] }));
    // And unsigned, the key alone sets nothing.
    let unsigned = Client::new(&p.url, &p.phone.key);
    assert_eq!(unsigned.put_push_token(&"ab".repeat(32), "production").unwrap_err().code(), "device_signature_missing");
}

/// R-SYNC-1: pushes are sync's, so a free workspace is refused all three
/// calls with the paid plan's fix.
#[test]
fn r_sync_1_a_free_workspace_is_refused() {
    let p = person();
    // The stand-in's FREE_PLAN_KEY_MARKER: a key with `free` in it.
    let free = p.laptop.client_as(&p.url, "krowk_sk_free_push");
    let refusals = [
        free.notify_session(SESSION, NotifyKind::Done, &"ab".repeat(32), "token").unwrap_err(),
        free.put_push_token(&"ab".repeat(32), "sandbox").unwrap_err(),
        free.delete_push_token().unwrap_err(),
    ];
    for e in refusals {
        assert_eq!((e.code().as_str(), e.status), ("sync_requires_paid_plan", 402), "{e:?}");
        assert!(e.fix().contains("paid plan"), "{e:?}");
    }
}
