//! The status layer against the stand-in registry (R-STA-5, R-STA-6): a
//! host publishes its sealed status board and another of the person's
//! devices fetches and opens it; a machine's counts go under a random
//! statistics id and come back as sent. The stand-in keeps the board's
//! bytes and the counts, nothing else (`GET /_status_layer`, stand-in only).

use gruntas_api::sync::ListPost;
use gruntas_api::Client;
use gruntas_client::device_chain::{Chain, Change, Kind, Subject};
use gruntas_client::e2e::{hex, DeviceKey, DeviceSigner, SigningKey};
use gruntas_client::status::{self, Board, Counts, Field, Health, Message, Task, TaskMetadata, TaskState, TaskStatus};
use gruntas_client::user_key::{UserKey, UserKeys};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;

const TOKEN: &str = "krowk_sk_status";
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

    fn id(&self) -> String {
        self.device.id().to_string()
    }

    fn signing(&self) -> SigningKey {
        SigningKey::from_secret(&self.signing).unwrap()
    }

    fn subject(&self, name: &str) -> Subject {
        Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: self.device.public(), signing: self.signing().public() }
    }

    fn client(&self, url: &str) -> Client {
        Client::new(url, &self.key).signed_by(DeviceSigner::new(self.device.id(), self.signing()).shared())
    }
}

/// The stand-in, the host that started the person's device list and the
/// phone it added, each key bound to its device.
struct Person {
    _registry: gruntas_devregistry::Running,
    addr: std::net::SocketAddr,
    url: String,
    host: Machine,
    phone: Machine,
    chain: Chain,
    user: UserKey,
}

fn person() -> Person {
    let registry = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), gruntas_devregistry::Config::default()).unwrap();
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
    Person { addr: registry.addr(), _registry: registry, url, host, phone, chain, user: added.newest }
}

fn held(addr: std::net::SocketAddr) -> Value {
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    write!(conn, "GET /_status_layer HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    serde_json::from_str(&answer[answer.find("\r\n\r\n").unwrap() + 4..]).unwrap()
}

fn board(p: &Person) -> Board {
    let mut working = Task { id: "s1".into(), status: TaskStatus { state: TaskState::Working, message: Some(Message::text("m1", "secret summary")), timestamp: "2026-11-23T12:00:00Z".into() }, history: Vec::new(), metadata: TaskMetadata::default() };
    working.metadata.gruntas.health = Some(Field::harness(Health::Serving));
    working.metadata.gruntas.project = Some(Field::harness("github.com/acme/secret".into()));
    let waiting = Task { id: "s2".into(), status: TaskStatus { state: TaskState::InputRequired, message: None, timestamp: "2026-11-23T12:00:00Z".into() }, ..working.clone() };
    Board { device: p.host.id(), published_at: "2026-11-23T12:00:01Z".into(), sessions: vec!["s1".into(), "s2".into()], tasks: vec![working, waiting] }
}

/// R-STA-5: the host seals and publishes its board; the phone, which has
/// none, fetches the host's and opens it as it was. A second publish
/// replaces the first.
#[test]
fn r_sta_5_a_board_round_trips_through_the_dev_registry() {
    let p = person();
    let (host, phone) = (p.host.client(&p.url), p.phone.client(&p.url));
    assert_eq!(phone.status_board(&p.host.id()).unwrap(), None);
    let keys = UserKeys::new(p.user.clone(), []).unwrap();
    let b = board(&p);
    let sealed = status::seal_board(&b, &keys, p.chain.generation(), &p.host.device.id()).unwrap();
    let put = host.put_status_board(&sealed).unwrap();
    assert_eq!(put.device, p.host.id());
    let got = phone.status_board(&p.host.id()).unwrap().unwrap();
    assert_eq!((got.device.as_str(), got.updated_at.as_str()), (p.host.id().as_str(), put.updated_at.as_str()));
    assert_eq!(status::open_board(&got.sealed, &p.host.device.id(), &keys, &p.chain).unwrap(), b);
    assert_eq!(phone.status_board(&p.phone.id()).unwrap(), None);

    let mut next = b.clone();
    next.tasks.pop();
    host.put_status_board(&status::seal_board(&next, &keys, p.chain.generation(), &p.host.device.id()).unwrap()).unwrap();
    let got = phone.status_board(&p.host.id()).unwrap().unwrap();
    assert_eq!(status::open_board(&got.sealed, &p.host.device.id(), &keys, &p.chain).unwrap().tasks.len(), 1);
    let stored = held(p.addr);
    assert_eq!(stored["status_boards"].as_array().unwrap().len(), 1);
    assert!(!stored.to_string().contains("secret"), "{stored}");
}

/// R-STA-5: only the device the key speaks for publishes its board, signed,
/// and at most 256 KB of it.
#[test]
fn r_sta_5_a_board_is_the_signing_devices_own_and_within_its_size() {
    let p = person();
    let crossed = Client::new(&p.url, &p.phone.key).signed_by(DeviceSigner::new(p.host.device.id(), p.host.signing()).shared());
    let e = crossed.put_status_board(b"x").unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("device_mismatch", 403), "{e:?}");
    assert_eq!(Client::new(&p.url, &p.phone.key).put_status_board(b"x").unwrap_err().code(), "device_signature_missing");
    let host = p.host.client(&p.url);
    let e = host.put_status_board(&vec![0u8; status::MAX_SEALED + 1]).unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("status_board_too_large", 413), "{e:?}");
    host.put_status_board(&vec![0u8; status::MAX_SEALED]).unwrap();
}

/// R-STA-6: a machine's counts go under its random statistics id and come
/// back as sent; the stand-in holds them and the board's bytes, and
/// nothing names the device. A field or name it does not count, and an id
/// of another form, are refused.
#[test]
fn r_sta_6_counts_round_trip_under_a_statistics_id() {
    let p = person();
    let host = p.host.client(&p.url);
    let id = status::statistics_id();
    assert_eq!(host.statistics(&id).unwrap_err().code(), "no_statistics");
    let counts = Counts::of(&board(&p));
    host.put_statistics(&id, &counts).unwrap();
    let got = p.phone.client(&p.url).statistics(&id).unwrap();
    assert_eq!(got["id"], json!(id));
    assert!(got["updated_at"].as_str().is_some(), "{got}");
    let mut fields = got.as_object().unwrap().clone();
    fields.remove("id");
    fields.remove("updated_at");
    assert_eq!(serde_json::from_value::<Counts>(Value::Object(fields)).unwrap(), counts);
    assert_eq!(got["states"], json!({ "TASK_STATE_INPUT_REQUIRED": 1, "TASK_STATE_WORKING": 1 }));

    let stored = held(p.addr);
    assert_eq!(stored["statistics"].as_array().unwrap().len(), 1);
    assert!(!stored.to_string().contains(&p.host.id()), "{stored}");

    for (body, field) in [
        (json!({ "health": "SERVING", "project": "github.com/acme/app" }), "statistics"),
        (json!({ "health": "BUSY" }), "health"),
        (json!({ "health": "SERVING", "states": { "stalled": 1 } }), "states"),
        (json!({ "health": "SERVING", "health_counts": { "SERVING": -1 } }), "health_counts"),
    ] {
        let e = host.put_statistics(&id, &body).unwrap_err();
        assert_eq!((e.code().as_str(), e.status), ("invalid", 422), "{body}");
        assert!(e.body.get("details").and_then(|d| d.get(field)).is_some(), "{body}: {e:?}");
    }
    assert_eq!(host.put_statistics("ABC", &counts).unwrap_err().code(), "invalid");
    assert_eq!(host.statistics(&id).unwrap()["states"], got["states"], "a refused put leaves the last");
}
