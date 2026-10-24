//! Presences against the stand-in registry (R-HOST-3, R-E2E-1, R-SYNC-1): a
//! host publishes its sealed announcement and refreshes it like a lease,
//! the person's devices list it online until it lapses by the registry's
//! clock, and the host clears it going away. The stand-in makes the real
//! registry's refusals, and shows what it stored (`GET /_presences`,
//! stand-in only) so a test can see nothing but the bytes was kept.

use jiff::{SignedDuration, Timestamp};
use krowk_api::sync::ListPost;
use krowk_api::Client;
use krowk_client::device_chain::{Chain, Change, Kind, Subject};
use krowk_client::user_key::UserKey;
use krowk_client::e2e::{hex, DeviceKey, DeviceSigner, SigningKey};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "krowk_sk_presence";
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

    fn client_as(&self, url: &str, key: &str) -> Client {
        Client::new(url, key).signed_by(DeviceSigner::new(self.device.id(), self.signing()).shared())
    }

    fn client(&self, url: &str) -> Client {
        self.client_as(url, &self.key)
    }
}

/// The stand-in on a clock the test moves, the host that started the
/// person's device list, and the phone it added, each key bound to its
/// device.
struct Person {
    _registry: krowk_devregistry::Running,
    clock: Arc<Mutex<Timestamp>>,
    addr: std::net::SocketAddr,
    url: String,
    host: Machine,
    phone: Machine,
    chain: Chain,
    user: UserKey,
}

impl Person {
    fn advance(&self, secs: i64) {
        *self.clock.lock().unwrap() += SignedDuration::from_secs(secs);
    }
}

fn person() -> Person {
    let clock = Arc::new(Mutex::new(Timestamp::now()));
    let c = Arc::clone(&clock);
    let config = krowk_devregistry::Config { clock: Some(Arc::new(move || *c.lock().unwrap())), ..krowk_devregistry::Config::default() };
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).unwrap();
    let url = format!("{}/v1", registry.url());
    let (host, phone) = (Machine::new("host"), Machine::new("phone"));
    let (chain, start) = Chain::start(host.subject("host"), &host.signing(), None, T0).unwrap();
    let user = start.newest;
    let entries = |b: &[krowk_client::device_chain::SignedEntry]| b.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect();
    let init = ListPost { entries: entries(&start.entries), links: Vec::new(), wraps: vec![(host.id(), hex(&user.wrap_to(&host.device.public()).unwrap()))], start_over: false };
    host.client(&url).init_device_list(&init).unwrap();
    let (chain, added) = chain.batch(&user, vec![Change::Add(phone.subject("phone"))], host.device.id(), &host.signing(), T0 + 1).unwrap();
    let append = ListPost { entries: entries(&added.entries), links: Vec::new(), wraps: vec![(phone.id(), hex(&user.wrap_to(&phone.device.public()).unwrap()))], start_over: false };
    host.client(&url).append_device_list(&append).unwrap();
    phone.client(&url).claim_key_device().unwrap();
    Person { addr: registry.addr(), _registry: registry, clock, url, host, phone, chain, user: added.newest }
}

/// A stand-in-only call, over HTTP as another crate's test reaches it.
fn raw(addr: std::net::SocketAddr, request: &str) -> String {
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    write!(conn, "{request}").unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    answer
}

fn stored(addr: std::net::SocketAddr) -> Value {
    let answer = raw(addr, "GET /_presences HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    serde_json::from_str(&answer[answer.find("\r\n\r\n").unwrap() + 4..]).unwrap()
}

/// R-HOST-3: a host publishes, the phone lists it online with the bytes as
/// sent, a refresh replaces it, past its ttl it is listed offline and still
/// listed, and a delete clears it — twice is fine.
#[test]
fn r_host_3_a_host_publishes_refreshes_lapses_and_clears_its_presence() {
    let p = person();
    let (host, phone) = (p.host.client(&p.url), p.phone.client(&p.url));
    assert_eq!(phone.list_presences().unwrap(), vec![]);

    let put = host.put_presence(b"sealed announcement", 30).unwrap();
    assert!(put.expires_at.parse::<Timestamp>().is_ok(), "{put:?}");
    let listed = phone.list_presences().unwrap();
    assert_eq!(listed.len(), 1);
    let first = &listed[0];
    assert_eq!((first.device.as_str(), first.sealed.as_slice(), first.online), (p.host.id().as_str(), &b"sealed announcement"[..], true));
    assert_eq!(first.expires_at, put.expires_at);

    // A refresh with a short ttl: the same one row, the new bytes.
    p.advance(5);
    host.put_presence(b"refreshed", 10).unwrap();
    let refreshed = phone.list_presences().unwrap();
    assert_eq!(refreshed.len(), 1);
    assert_eq!((refreshed[0].sealed.as_slice(), refreshed[0].online), (&b"refreshed"[..], true));
    assert!(refreshed[0].updated_at > first.updated_at, "{refreshed:?}");

    // Past it, offline by the registry's clock, and still there to say when
    // it was last seen.
    p.advance(11);
    let lapsed = host.list_presences().unwrap();
    assert_eq!((lapsed.len(), lapsed[0].online, &lapsed[0].updated_at), (1, false, &refreshed[0].updated_at));

    host.delete_presence().unwrap();
    host.delete_presence().unwrap();
    assert_eq!(phone.list_presences().unwrap(), vec![]);
    assert_eq!(stored(p.addr), json!({ "presences": [] }));
}

/// R-HOST-3, R-E2E-1: what the stand-in keeps is the device, the sealed
/// bytes and the times, nothing else — the `_presences` read ticket 122's
/// test leans on.
#[test]
fn r_host_3_the_stand_in_keeps_only_the_device_the_bytes_and_the_times() {
    let p = person();
    p.host.client(&p.url).put_presence(&[0xab, 0xcd], 30).unwrap();
    let held = stored(p.addr);
    let row = &held["presences"][0];
    assert_eq!((row["device"].as_str(), row["sealed"].as_str()), (Some(p.host.id().as_str()), Some("abcd")));
    let mut keys: Vec<&String> = row.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["created_at", "device", "expires_at", "sealed", "updated_at"]);
}

/// R-HOST-3: only the device the key speaks for publishes or clears its
/// presence: the phone's key signed by the host is `device_mismatch`, and the
/// key alone, unsigned, is refused — by the client before it sends, and by
/// the stand-in when it is sent anyway.
#[test]
fn r_host_3_a_presence_signed_by_another_device_is_refused() {
    let p = person();
    p.phone.client(&p.url).put_presence(b"phone", 30).unwrap();
    let crossed = p.host.client_as(&p.url, &p.phone.key);
    for e in [crossed.put_presence(b"host", 30).unwrap_err(), crossed.delete_presence().unwrap_err()] {
        assert_eq!((e.code().as_str(), e.status), ("device_mismatch", 403), "{e:?}");
    }
    let unsigned = Client::new(&p.url, &p.phone.key);
    assert_eq!(unsigned.put_presence(b"host", 30).unwrap_err().code(), "device_signature_missing");
    let body = r#"{"presence":{"sealed":"ab"}}"#;
    let request = format!("PUT /v1/key/device/presence HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", p.phone.key, body.len());
    let answer = raw(p.addr, &request);
    assert!(answer.starts_with("HTTP/1.1 401") && answer.contains("signature_required"), "{answer}");
    assert_eq!(stored(p.addr)["presences"][0]["sealed"], json!(hex(b"phone")));
}

/// R-HOST-3: a ttl outside 10 to 120 seconds is refused, never clamped, and
/// so are no bytes and more than 16 KiB of them, each naming its field.
#[test]
fn r_host_3_a_bad_ttl_or_size_is_refused_naming_the_field() {
    let p = person();
    let host = p.host.client(&p.url);
    let too_big = vec![0u8; 16 * 1024 + 1];
    for (sealed, ttl, field) in [(&b"x"[..], 5, "ttl"), (b"x", 9, "ttl"), (b"x", 121, "ttl"), (b"x", 500, "ttl"), (b"", 30, "sealed"), (too_big.as_slice(), 30, "sealed")] {
        let e = host.put_presence(sealed, ttl).unwrap_err();
        assert_eq!((e.code().as_str(), e.status), ("invalid", 422), "{ttl} {}", sealed.len());
        assert!(e.body.get("details").and_then(|d| d.get(field)).is_some(), "{e:?}");
    }
    assert_eq!(stored(p.addr), json!({ "presences": [] }));
    host.put_presence(&vec![0u8; 16 * 1024], 10).unwrap();
    host.put_presence(b"x", 120).unwrap();
}

/// R-HOST-3: a device the dashboard revokes is no longer listed as a host,
/// its presence gone with it.
#[test]
fn r_host_3_a_revoked_devices_presence_is_not_listed() {
    let p = person();
    p.phone.client(&p.url).put_presence(b"phone", 30).unwrap();
    p.host.client(&p.url).put_presence(b"host", 30).unwrap();
    let request = format!("POST /_settings/devices/{}/revocation HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", p.phone.id(), p.host.key);
    let answer = raw(p.addr, &request);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let listed = p.host.client(&p.url).list_presences().unwrap();
    assert_eq!(listed.iter().map(|x| x.device.as_str()).collect::<Vec<_>>(), [p.host.id().as_str()]);
    assert_eq!(stored(p.addr)["presences"].as_array().unwrap().len(), 1);
}

/// R-HOST-3: a device the chain removes is no longer listed as a host, its
/// presence gone with it.
#[test]
fn r_host_3_a_removed_devices_presence_is_not_listed() {
    let p = person();
    p.phone.client(&p.url).put_presence(b"phone", 30).unwrap();
    p.host.client(&p.url).put_presence(b"host", 30).unwrap();
    let (_, removed) = p.chain.batch(&p.user, vec![Change::Remove(p.phone.subject("phone"))], p.host.device.id(), &p.host.signing(), T0 + 2).unwrap();
    let post = ListPost {
        entries: removed.entries.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        links: removed.links.iter().map(|l| hex(l)).collect(),
        wraps: removed.wraps.iter().map(|(d, w)| (d.to_string(), hex(w))).collect(),
        start_over: false,
    };
    p.host.client(&p.url).append_device_list(&post).unwrap();
    let listed = p.host.client(&p.url).list_presences().unwrap();
    assert_eq!(listed.iter().map(|x| x.device.as_str()).collect::<Vec<_>>(), [p.host.id().as_str()]);
    assert_eq!(stored(p.addr)["presences"].as_array().unwrap().len(), 1);
}

/// R-SYNC-1: presences are sync's, so a free workspace is refused all three
/// calls with the paid plan's fix.
#[test]
fn r_sync_1_a_free_workspace_is_refused_presences() {
    let p = person();
    // The stand-in's FREE_PLAN_KEY_MARKER: a key with `free` in it.
    let free = p.host.client_as(&p.url, "krowk_sk_free_presence");
    for e in [free.put_presence(b"x", 30).unwrap_err(), free.delete_presence().unwrap_err(), free.list_presences().unwrap_err()] {
        assert_eq!((e.code().as_str(), e.status), ("sync_requires_paid_plan", 402), "{e:?}");
    }
}
