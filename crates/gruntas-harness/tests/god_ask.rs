//! The god agent's questions against the stand-in registry (GA01, D3, D8):
//! the phone asks the host a `read`, the host answers with a page too big
//! for the 4 KB answer, which goes in the command's answer record and is
//! read back by `god::ask`; and a host older than the command refuses it
//! `unknown_command`, which `ask` says as "that machine needs updating".

use gruntas_api::sync::ListPost;
use gruntas_api::Client;
use gruntas_client::device_chain::{Chain, Change, Kind, Subject};
use gruntas_client::e2e::{hex, DeviceId, DeviceKey, DeviceSigner, SigningKey};
use gruntas_client::host_command::{self, HostAnswer, HostCommand, TranscriptPage};
use gruntas_client::user_key::{UserKey, UserKeys};
use gruntas_harness::sync::announce::Announcer;
use gruntas_harness::sync::god;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

const T0: u64 = 1_790_000_000;
const SESSION: &str = "01a0ec7b-0000-7000-8000-0000000000a1";

struct Machine {
    device: DeviceKey,
    signing: [u8; 32],
    key: String,
}

impl Machine {
    fn new(name: &str) -> Machine {
        Machine { device: DeviceKey::generate(), signing: *SigningKey::generate().secret_bytes(), key: format!("krowk_sk_god#{name}") }
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

    fn announcer(&self, url: &str, chain: &Chain, user: &UserKey) -> Announcer {
        Announcer { api: Arc::new(self.client(url)), keys: UserKeys::new(user.clone(), []).unwrap(), chain: chain.clone(), device: self.device.id() }
    }
}

/// The stand-in, and the host and phone on the person's list.
fn person() -> (gruntas_devregistry::Running, Announcer, Announcer) {
    let registry = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), gruntas_devregistry::Config::default()).unwrap();
    let url = format!("{}/v1", registry.url());
    let (host, phone) = (Machine::new("host"), Machine::new("phone"));
    let (chain, start) = Chain::start(host.subject("host"), &host.signing(), None, T0).unwrap();
    let entries = |b: &[gruntas_client::device_chain::SignedEntry]| b.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect();
    let init = ListPost { entries: entries(&start.entries), links: Vec::new(), wraps: vec![(host.device.id().to_string(), hex(&start.newest.wrap_to(&host.device.public()).unwrap()))], start_over: false };
    host.client(&url).init_device_list(&init).unwrap();
    let (chain, added) = chain.batch(&start.newest, vec![Change::Add(phone.subject("phone"))], host.device.id(), &host.signing(), T0 + 1).unwrap();
    let append = ListPost { entries: entries(&added.entries), links: Vec::new(), wraps: vec![(phone.device.id().to_string(), hex(&added.newest.wrap_to(&phone.device.public()).unwrap()))], start_over: false };
    host.client(&url).append_device_list(&append).unwrap();
    phone.client(&url).claim_key_device().unwrap();
    let (h, p) = (host.announcer(&url, &chain, &added.newest), phone.announcer(&url, &chain, &added.newest));
    (registry, h, p)
}

/// The host's side: each command delivered in a refresh opened and given
/// `reply`'s answer, through the answer record when it does not fit.
fn serve(host: Announcer, reply: fn(HostCommand) -> HostAnswer) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for _ in 0..200 {
            if let Some(c) = host.api.put_presence(b"announcement", 30).unwrap().commands.into_iter().next() {
                let from = DeviceId::parse(&c.from).unwrap();
                let command = host_command::open_command(&c.sealed, &c.id, &from, &host.device, &host.keys, &host.chain).unwrap();
                let answer = command.map_or_else(|refused| refused, reply);
                let (sealed, record) = host_command::seal_answer_or_record(&answer, &c.id, &host.device, &from, &host.keys, host.chain.generation()).unwrap();
                if let Some(record) = record {
                    host.api.put_answer_record(&c.id, &record).unwrap();
                }
                host.api.answer_host_command(&c.id, &sealed).unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}

fn page(c: HostCommand) -> HostAnswer {
    let HostCommand::Read { session, .. } = c else { panic!("not a read: {c:?}") };
    HostAnswer::Page(TranscriptPage { session, text: "ä line of the transcript\n".repeat(500), next: Some("before:4096".into()) })
}

/// An older host's refusal of a command it does not know.
fn outdated(_: HostCommand) -> HostAnswer {
    HostAnswer::refused("unknown_command", "this host does not know the command \"read\" — upgrade gruntas on it")
}

#[test]
fn ga01_a_page_too_big_for_an_answer_comes_back_through_the_answer_record() {
    let (_registry, host, phone) = person();
    let to = host.device;
    let served = serve(host, page);
    let read = HostCommand::Read { session: SESSION.into(), page: None };
    let answer = god::ask(&phone, &to, "host", &read, Duration::from_secs(10)).unwrap();
    served.join().unwrap();
    let HostAnswer::Page(p) = answer else { panic!("{answer:?}") };
    assert!(p.text.len() > host_command::MAX_ANSWER, "{}", p.text.len());
    assert_eq!((p.session.as_str(), p.next.as_deref()), (SESSION, Some("before:4096")));
}

#[test]
fn ga01_an_older_hosts_unknown_command_reads_as_needs_updating() {
    let (_registry, host, phone) = person();
    let to = host.device;
    let served = serve(host, outdated);
    let e = god::ask(&phone, &to, "host", &HostCommand::Changes { since: "2026-11-24T10:00:00Z".into() }, Duration::from_secs(10)).unwrap_err();
    served.join().unwrap();
    assert_eq!(e.0, "host_outdated");
    assert!(e.1.contains(host_command::NEEDS_UPDATING), "{}", e.1);
}
