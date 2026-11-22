//! A newly signed-in machine asking the person's hosts for a pairing
//! (R-EASE-5, R-EASE-6, D6) against the stand-in registry: the machine, a
//! key of the person's with no device, records its proposed name and OS; a
//! listed host reads it in its presence refresh; a second request is refused
//! while one is pending; it lapses after ten minutes on the stand-in's
//! clock. The request opens nothing and carries no keys.

use jiff::{SignedDuration, Timestamp};
use gruntas_api::sync::{ListPost, PairingRequestState};
use gruntas_api::Client;
use gruntas_client::device_chain::{Chain, Change, Kind, Subject};
use gruntas_client::e2e::{hex, DeviceKey, DeviceSigner, SigningKey};
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
    _registry: gruntas_devregistry::Running,
    clock: Arc<Mutex<Timestamp>>,
    url: String,
    host: Machine,
    phone: Machine,
}

impl Person {
    fn advance(&self, secs: i64) {
        *self.clock.lock().unwrap() += SignedDuration::from_secs(secs);
    }
}

fn person() -> Person {
    let clock = Arc::new(Mutex::new(Timestamp::now()));
    let c = Arc::clone(&clock);
    let config = gruntas_devregistry::Config { clock: Some(Arc::new(move || *c.lock().unwrap())), ..gruntas_devregistry::Config::default() };
    let registry = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).unwrap();
    let url = format!("{}/v1", registry.url());
    let (host, phone) = (Machine::new("host"), Machine::new("phone"));
    let (chain, start) = Chain::start(host.subject("host"), &host.signing(), None, T0).unwrap();
    let user = start.newest;
    let entries = |b: &[gruntas_client::device_chain::SignedEntry]| b.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect();
    let init = ListPost { entries: entries(&start.entries), links: Vec::new(), wraps: vec![(host.id(), hex(&user.wrap_to(&host.device.public()).unwrap()))], start_over: false };
    host.client(&url).init_device_list(&init).unwrap();
    let (chain, added) = chain.batch(&user, vec![Change::Add(phone.subject("phone"))], host.device.id(), &host.signing(), T0 + 1).unwrap();
    let append = ListPost { entries: entries(&added.entries), links: Vec::new(), wraps: vec![(phone.id(), hex(&user.wrap_to(&phone.device.public()).unwrap()))], start_over: false };
    host.client(&url).append_device_list(&append).unwrap();
    phone.client(&url).claim_key_device().unwrap();
    let _ = (chain, added);
    Person { _registry: registry, clock, url, host, phone }
}

/// The new machine: signed in as the person (a key of theirs), no device.
fn new_machine(p: &Person, name: &str) -> Client {
    Client::new(&p.url, &format!("{TOKEN}#{name}"))
}

/// R-EASE-5: an unlisted signed-in machine records a request and polls it;
/// a listed device's presence refresh receives it, with the machine's
/// name and OS and nothing else.
#[test]
fn r_ease_5_an_unlisted_machine_records_a_request_and_a_host_receives_it() {
    let p = person();
    let host = p.host.client(&p.url);
    assert_eq!(host.put_presence(b"host", 30).unwrap().pairing_requests, vec![]);

    let b = new_machine(&p, "b");
    let made = b.request_pairing("desk", "Linux").unwrap();
    assert!(made.id.starts_with("prq_"), "{made:?}");
    assert_eq!((made.state, made.name.as_str(), made.os.as_str()), (PairingRequestState::Pending, "desk", "Linux"));
    assert_eq!(b.pairing_request().unwrap(), made);

    // Delivered to every listed device's refresh, on every refresh while pending.
    for _ in 0..2 {
        let put = host.put_presence(b"host", 30).unwrap();
        let [got] = put.pairing_requests.as_slice() else { panic!("{put:?}") };
        assert_eq!((got.id.as_str(), got.name.as_str(), got.os.as_str(), got.expires_at.as_str()), (made.id.as_str(), "desk", "Linux", made.expires_at.as_str()));
    }
    assert_eq!(p.phone.client(&p.url).put_presence(b"phone", 30).unwrap().pairing_requests.len(), 1);
}

/// R-EASE-6: a second request while one is pending is `pairing_request_pending`,
/// from the same machine or another; once the first lapses a new one is taken.
#[test]
fn r_ease_6_a_second_request_is_refused_while_one_is_pending() {
    let p = person();
    new_machine(&p, "b").request_pairing("one", "Linux").unwrap();
    for who in ["b", "c"] {
        let e = new_machine(&p, who).request_pairing("two", "macOS").unwrap_err();
        assert_eq!((e.code().as_str(), e.status), ("pairing_request_pending", 409), "{e:?}");
    }
    p.advance(11 * 60);
    let again = new_machine(&p, "c").request_pairing("two", "macOS").unwrap();
    assert_eq!(again.state, PairingRequestState::Pending);
}

/// R-EASE-6: ten minutes on the registry's clock and it is expired: the
/// machine's poll says so, and hosts stop hearing of it.
#[test]
fn r_ease_6_a_request_expires_after_ten_minutes() {
    let p = person();
    let b = new_machine(&p, "b");
    let made = b.request_pairing("desk", "Linux").unwrap();
    assert_eq!(made.expires_at.parse::<Timestamp>().unwrap().duration_since(*p.clock.lock().unwrap()), SignedDuration::from_mins(10));

    p.advance(9 * 60);
    assert_eq!(p.host.client(&p.url).put_presence(b"host", 30).unwrap().pairing_requests.len(), 1);
    assert_eq!(b.pairing_request().unwrap().state, PairingRequestState::Pending);
    p.advance(2 * 60);
    assert_eq!(p.host.client(&p.url).put_presence(b"host", 30).unwrap().pairing_requests, vec![]);
    assert_eq!(b.pairing_request().unwrap().state, PairingRequestState::Expired);
}

/// A host opening a pairing answers the request: the machine's poll says
/// the pairing is open and it can find it, the hosts stop hearing of it, and
/// the next machine may ask. A request made while a pairing is open is answered
/// at once.
#[test]
fn a_host_opening_a_pairing_answers_the_request() {
    let p = person();
    let host = p.host.client(&p.url);
    let b = new_machine(&p, "b");
    b.request_pairing("desk", "Linux").unwrap();
    let opened = host.open_pairing().unwrap();

    assert_eq!(b.pairing_request().unwrap().state, PairingRequestState::PairingOpen);
    assert_eq!(b.find_open_pairing().unwrap().id, opened.id);
    assert_eq!(host.put_presence(b"host", 30).unwrap().pairing_requests, vec![]);

    let c = new_machine(&p, "c");
    assert_eq!(c.request_pairing("laptop", "macOS").unwrap().state, PairingRequestState::PairingOpen);
}

/// A request opens nothing and carries no keys: no pairing, no device, and
/// the key that made it still speaks for none.
#[test]
fn a_request_opens_nothing() {
    let p = person();
    new_machine(&p, "b").request_pairing("SECRET-NAME", "SECRET-OS").unwrap();

    assert_eq!(new_machine(&p, "b").find_open_pairing().unwrap_err().code(), "no_pairing");
}

/// Only the machine that asked reads its request; a key that speaks for a
/// device asks nothing; a person with no device list has nobody to ask;
/// name and OS are plain text within their length.
#[test]
fn a_request_is_its_machines_and_its_text_is_plain() {
    let p = person();
    let b = new_machine(&p, "b");
    assert_eq!(b.pairing_request().unwrap_err().code(), "no_pairing_request");
    b.request_pairing("desk", "Linux").unwrap();
    assert_eq!(new_machine(&p, "c").pairing_request().unwrap_err().code(), "no_pairing_request");

    let e = p.host.client(&p.url).request_pairing("desk", "Linux").unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("device_mismatch", 403), "{e:?}");

    let lonely = Client::new(&p.url, "krowk_sk_nobody");
    assert_eq!(lonely.request_pairing("desk", "Linux").unwrap_err().code(), "no_device_list");

    let c = new_machine(&p, "c");
    p.advance(11 * 60);
    let (long_name, long_os) = ("n".repeat(101), "o".repeat(33));
    for (name, os, field) in [("esc\u{1b}[2J", "Linux", "name"), ("ok", "Li\nnux", "os"), ("  ", "Linux", "name"), ("ok", "", "os"), (long_name.as_str(), "Linux", "name"), ("ok", long_os.as_str(), "os"), ("\u{202e}evil", "Linux", "name"), ("ok", "Li\u{200b}nux", "os"), ("a\u{2028}b", "Linux", "name")] {
        let e = c.request_pairing(name, os).unwrap_err();
        assert_eq!((e.code().as_str(), e.status), ("invalid", 422), "{name:?} {os:?}");
        assert!(e.body.get("details").and_then(|d| d.get(field)).is_some(), "{e:?}");
    }
    assert_eq!(c.request_pairing("\u{a0}", "Linux").unwrap_err().code(), "invalid");
    assert_eq!(c.request_pairing("  desk 2  ", "Linux").unwrap().name, "desk 2");
}
