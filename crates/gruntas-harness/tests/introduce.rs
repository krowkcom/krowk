//! A new machine signs in and a host answers it (R-EASE-5, R-EASE-6, D6),
//! both sides in this process against the stand-in registry: B asks for a
//! pairing and waits; A's host reads the request in its presence refresh,
//! opens the pairing and shows the code; B types it; A shows the new
//! machine's name and OS only after the code's checks and asks; and a yes
//! adds B. Two hosts open one pairing between them, a wrong code adds
//! nothing and asks nothing, and a request is taken up once.

use gruntas_api::sync::{DeliveredPairingRequest, ListPost, Pairing, PairingStep};
use gruntas_api::{Client, Error};
use gruntas_client::device_chain::{Chain, Change, Kind, SignedEntry, Subject};
use gruntas_client::e2e::{hex, DeviceId, DeviceKey, DeviceSigner, SigningKey};
use gruntas_client::keystore::{Keystore, MemoryStorage};
use gruntas_client::pairing::{Binding, NewDevice, PairingCode, PeerKind};
use gruntas_client::pairing_join::{self, JoinFailed, Mailbox};
use gruntas_client::protocol::{OfferStage, PairingOffer};
use gruntas_client::user_key::{UserKey, UserKeys};
use gruntas_harness::sync::introduce::{Introducer, Introductions, Timing};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "krowk_sk_introduce";
const T0: u64 = 1_790_000_000;

struct Machine {
    device: DeviceKey,
    signing: [u8; 32],
    key: String,
}

impl Machine {
    fn new(name: &str) -> Machine {
        Machine { device: DeviceKey::generate(), signing: *SigningKey::generate().secret_bytes(), key: format!("{TOKEN}#{name}") }
    }

    fn signing(&self) -> SigningKey {
        SigningKey::from_secret(&self.signing).unwrap()
    }

    fn subject(&self, name: &str) -> Subject {
        Subject { kind: Kind::Device, name: name.into(), os: "Linux".into(), device: self.device.public(), signing: self.signing().public() }
    }

    fn client(&self, url: &str) -> Client {
        Client::new(url, &self.key).signed_by(DeviceSigner::new(self.device.id(), self.signing()).shared())
    }
}

/// The person: the registry, two listed machines (each a host), and what
/// each needs to answer a request.
struct Person {
    _registry: gruntas_devregistry::Running,
    url: String,
    hosts: [Machine; 2],
    user: UserKey,
    chain: Chain,
    entries: Vec<SignedEntry>,
}

fn person() -> Person {
    let registry = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), gruntas_devregistry::Config::default()).unwrap();
    let url = format!("{}/v1", registry.url());
    let hosts = [Machine::new("a"), Machine::new("c")];
    let (chain, start) = Chain::start(hosts[0].subject("desktop"), &hosts[0].signing(), None, T0).unwrap();
    let user = start.newest.clone();
    let post = |b: &gruntas_client::device_chain::Batch, to: &Machine| ListPost {
        entries: b.entries.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        links: Vec::new(),
        wraps: vec![(to.device.id().to_string(), hex(&user.wrap_to(&to.device.public()).unwrap()))],
        start_over: false,
    };
    hosts[0].client(&url).init_device_list(&post(&start, &hosts[0])).unwrap();
    let (chain, added) = chain.batch(&user, vec![Change::Add(hosts[1].subject("server"))], hosts[0].device.id(), &hosts[0].signing(), T0 + 1).unwrap();
    hosts[0].client(&url).append_device_list(&post(&added, &hosts[1])).unwrap();
    hosts[1].client(&url).claim_key_device().unwrap();
    let entries = start.entries.iter().chain(added.entries.iter()).cloned().collect();
    Person { url, _registry: registry, hosts, user, chain, entries }
}

impl Person {
    /// The host `n`'s answer to requests, as the daemon builds it.
    fn introductions(&self, n: usize) -> Arc<Introductions> {
        let (url, user, chain, entries) = (self.url.clone(), self.user.clone(), self.chain.clone(), self.entries.clone());
        let me = &self.hosts[n];
        let (device, signing, key) = (me.device.id(), me.signing, me.key.clone());
        let build = move || {
            Ok(Introducer {
                api: Arc::new(Client::new(&url, &key).signed_by(DeviceSigner::new(device, SigningKey::from_secret(&signing).unwrap()).shared())),
                keys: UserKeys::new(user.clone(), []).unwrap(),
                chain: chain.clone(),
                entries: entries.clone(),
                device,
                signing: SigningKey::from_secret(&signing).unwrap(),
                keep: None,
            })
        };
        Introductions::with_timing(Box::new(build), Timing { poll: Duration::from_millis(20), wait: Duration::from_secs(30) })
    }

    /// A host's presence refresh, and what it carries.
    fn requests(&self, n: usize) -> Vec<DeliveredPairingRequest> {
        self.hosts[n].client(&self.url).put_presence(b"host", 30).unwrap().pairing_requests
    }
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stage(i: &Introductions, want: OfferStage) -> PairingOffer {
    until(&format!("{want:?}"), || i.offer().filter(|o| o.stage == want))
}

struct Mail {
    client: Client,
    id: String,
}

impl Mailbox for Mail {
    fn step(&self, step: PairingStep, message: &[u8]) -> Result<(), Error> {
        self.client.pairing_step(step, message).map(|_| ())
    }
    fn read(&self) -> Result<Pairing, Error> {
        self.client.show_pairing(&self.id)
    }
    fn end(&self) {
        let _ = self.client.end_pairing();
    }
    fn claim_device(&self) -> Result<(), Error> {
        self.client.claim_key_device().map(|_| ())
    }
    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The new machine, signed in as the person, joining with `typed`.
fn join(p: &Person, name: &str, typed: &str) -> std::thread::JoinHandle<Result<pairing_join::Joined, JoinFailed>> {
    let (url, name, typed) = (p.url.clone(), name.to_string(), typed.to_string());
    std::thread::spawn(move || {
        let b = Machine::new("b");
        let client = Client::new(&url, &b.key);
        let open = client.find_open_pairing().unwrap();
        let user = client.verify_key().unwrap().user_id;
        let store = Keystore::with(Arc::new(MemoryStorage::new()));
        let device = store.device_key().unwrap();
        let signing = store.signing_key().unwrap();
        let me = NewDevice { device: device.public(), signing: signing.public(), name, os: "Linux".into() };
        let binding = Binding { kind: PeerKind::SamePersonDevice, user_id: user, a_device: DeviceId::parse(&open.initiator_device.id).unwrap(), b_device: device.id() };
        let mail = Mail { client: client.signed_by(DeviceSigner::new(device.id(), signing).shared()), id: open.id };
        pairing_join::join(&mail, &store, &device, binding, PairingCode::parse(&typed).unwrap(), me, &pairing_join::Timing { poll: Duration::from_millis(20), ..Default::default() })
    })
}

fn listed(p: &Person) -> Vec<String> {
    let list = p.hosts[0].client(&p.url).device_list_all().unwrap();
    list.entries.iter().map(|e| e.entry.clone()).collect()
}

/// R-EASE-5: B signs in and asks; A's host reads the request in its
/// presence refresh, opens the pairing and shows the code; B types it; the
/// new machine's name and OS are shown, once, only after the code's checks;
/// a yes adds B to the list.
#[test]
fn r_ease_5_a_host_opens_the_pairing_and_a_typed_code_adds_the_new_machine() {
    let p = person();
    let host = p.introductions(0);
    assert!(host.offer().is_none(), "nothing before a request");
    let b = Client::new(&p.url, &Machine::new("b").key);
    b.request_pairing("SECRET-HINT", "Beos").unwrap();

    host.deliver(&p.requests(0));
    let waiting = stage(&host, OfferStage::Waiting);
    assert_eq!((waiting.name.as_str(), waiting.os.as_str()), ("", ""), "no name or OS before the checks: {waiting:?}");
    assert_eq!(waiting.code.len(), 9, "XXXX-XXXX");
    assert!(!format!("{waiting:?}").contains("SECRET-HINT"), "the request's hints are never shown");
    assert!(!host.answer(&waiting.pairing, true), "nothing is asked yet, so a yes is refused");

    let before = listed(&p).len();
    let joining = join(&p, "laptop", &waiting.code);
    let asked = stage(&host, OfferStage::Confirm);
    assert_eq!((asked.name.as_str(), asked.os.as_str(), asked.code.as_str()), ("laptop", "Linux", ""), "the pairing's own name and OS, after the checks: {asked:?}");
    assert_eq!(listed(&p).len(), before, "nothing is on the list before the yes");

    assert!(!host.answer("another-pairing", true), "a yes about another pairing is refused");
    assert!(host.answer(&asked.pairing, true));
    assert!(!host.answer(&asked.pairing, true), "the question is answered once, even before it is carried out");
    let joined = joining.join().unwrap().unwrap();
    assert_eq!(joined.name, "laptop");
    let added = stage(&host, OfferStage::Added);
    assert_eq!(added.name, "laptop");
    assert_eq!(listed(&p).len(), before + 1, "B is on the device list");
    assert!(!host.answer(&asked.pairing, true), "answered once");
    // The request is answered: no host hears of it again.
    assert!(p.requests(0).is_empty() && p.requests(1).is_empty());
}

/// R-EASE-6: two hosts online, one pairing: the registry's one-pairing rule
/// decides, and the other leaves it be.
#[test]
fn r_ease_6_two_hosts_online_open_exactly_one_pairing() {
    let p = person();
    let (a, c) = (p.introductions(0), p.introductions(1));
    Client::new(&p.url, &Machine::new("b").key).request_pairing("desk", "Linux").unwrap();
    let (for_a, for_c) = (p.requests(0), p.requests(1));
    assert_eq!(for_a.len(), 1);
    std::thread::scope(|s| {
        s.spawn(|| a.deliver(&for_a));
        s.spawn(|| c.deliver(&for_c));
    });
    // Both ran; one opened, the other found it open and shows nothing.
    until("one host with a code", || (a.offer().is_some() || c.offer().is_some()).then_some(()));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!([a.offer().is_some(), c.offer().is_some()].iter().filter(|x| **x).count(), 1);
}

/// A wrong code ends the pairing and asks nobody anything; nothing is added,
/// and the request is not taken up again by the host that ran it.
#[test]
fn a_wrong_code_asks_nothing_and_adds_nothing() {
    let p = person();
    let host = p.introductions(0);
    Client::new(&p.url, &Machine::new("b").key).request_pairing("desk", "Linux").unwrap();
    let carried = p.requests(0);
    host.deliver(&carried);
    let waiting = stage(&host, OfferStage::Waiting);
    let wrong = if waiting.code.starts_with('K') { "7777-7777" } else { "KKKK-KKKK" };
    let before = listed(&p).len();
    let joining = join(&p, "laptop", wrong);
    let failed = stage(&host, OfferStage::Failed);
    assert!(failed.note.contains("confirmation did not check"), "{failed:?}");
    assert!(joining.join().unwrap().is_err());
    assert_eq!(listed(&p).len(), before);
    assert!(failed.name.is_empty() && failed.code.is_empty(), "no name, no code left: {failed:?}");
    // The same request again: taken up once.
    host.deliver(&carried);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(host.offer().map(|o| o.stage), Some(OfferStage::Failed));
}

/// A no adds nothing; the new machine is told.
#[test]
fn a_no_adds_nothing() {
    let p = person();
    let host = p.introductions(0);
    Client::new(&p.url, &Machine::new("b").key).request_pairing("desk", "Linux").unwrap();
    host.deliver(&p.requests(0));
    let waiting = stage(&host, OfferStage::Waiting);
    let before = listed(&p).len();
    let joining = join(&p, "laptop", &waiting.code);
    let asked = stage(&host, OfferStage::Confirm);
    assert!(host.answer(&asked.pairing, false));
    assert_eq!(stage(&host, OfferStage::Failed).note, "not added — the new machine was told, and nothing reached your device list");
    assert!(joining.join().unwrap().is_err());
    assert_eq!(listed(&p).len(), before);
}
