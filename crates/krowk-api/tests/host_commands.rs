//! Host commands against the stand-in registry (R-HOST-4, R-E2E-1,
//! R-SYNC-1): one of the person's devices sends a host a sealed command,
//! the host is delivered it in its presence refresh until it answers, and
//! the sender reads the sealed answer back. The stand-in makes the real
//! registry's refusals: only the target answers, only the sender reads, and
//! after ten minutes on its clock the command is neither delivered nor
//! answered.

use jiff::{SignedDuration, Timestamp};
use krowk_api::sync::ListPost;
use krowk_api::Client;
use krowk_client::device_chain::{Chain, Change, Kind, Subject};
use krowk_client::user_key::UserKey;
use krowk_client::e2e::{hex, DeviceKey, DeviceSigner, SigningKey};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "krowk_sk_commands";
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
/// person's device list, and the phone and laptop it added, each key bound
/// to its device.
struct Person {
    _registry: krowk_devregistry::Running,
    clock: Arc<Mutex<Timestamp>>,
    addr: std::net::SocketAddr,
    url: String,
    host: Machine,
    phone: Machine,
    laptop: Machine,
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
    let (host, phone, laptop) = (Machine::new("host"), Machine::new("phone"), Machine::new("laptop"));
    let (chain, start) = Chain::start(host.subject("host"), &host.signing(), None, T0).unwrap();
    let user = start.newest;
    let entries = |b: &[krowk_client::device_chain::SignedEntry]| b.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect();
    let init = ListPost { entries: entries(&start.entries), links: Vec::new(), wraps: vec![(host.id(), hex(&user.wrap_to(&host.device.public()).unwrap()))], start_over: false };
    host.client(&url).init_device_list(&init).unwrap();
    let changes = vec![Change::Add(phone.subject("phone")), Change::Add(laptop.subject("laptop"))];
    let (chain, added) = chain.batch(&user, changes, host.device.id(), &host.signing(), T0 + 1).unwrap();
    let wraps = [&phone, &laptop].iter().map(|m| (m.id(), hex(&user.wrap_to(&m.device.public()).unwrap()))).collect();
    let append = ListPost { entries: entries(&added.entries), links: Vec::new(), wraps, start_over: false };
    host.client(&url).append_device_list(&append).unwrap();
    phone.client(&url).claim_key_device().unwrap();
    laptop.client(&url).claim_key_device().unwrap();
    Person { addr: registry.addr(), _registry: registry, clock, url, host, phone, laptop, chain, user: added.newest }
}

/// A stand-in-only call, over HTTP as another crate's test reaches it.
fn raw(addr: std::net::SocketAddr, request: &str) -> String {
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    write!(conn, "{request}").unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    answer
}

const ID: &str = "0f9e8d7c-6b5a-4938-8271-605f4e3d2c1b";

/// R-HOST-4: the phone sends, the host is delivered it on every refresh
/// until it answers, the phone polls and reads the answer, and sending the
/// same command again is the same command.
#[test]
fn r_host_4_a_command_is_sent_delivered_answered_and_read() {
    let p = person();
    let (host, phone) = (p.host.client(&p.url), p.phone.client(&p.url));

    let sent = phone.send_host_command(&p.host.id(), ID, b"sealed command").unwrap();
    assert_eq!((sent.id.as_str(), sent.answered, sent.answer.as_deref()), (ID, false, None));
    let expires_at: Timestamp = sent.expires_at.parse().unwrap();
    assert_eq!(expires_at.duration_since(*p.clock.lock().unwrap()), SignedDuration::from_mins(10));

    // A resend: the same command, stored once.
    assert_eq!(phone.send_host_command(&p.host.id(), ID, b"sealed command").unwrap(), sent);

    for _ in 0..2 {
        let delivered = host.put_presence(b"announcement", 30).unwrap().commands;
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        let c = &delivered[0];
        assert_eq!((c.id.as_str(), c.from.as_str(), c.sealed.as_slice()), (ID, p.phone.id().as_str(), &b"sealed command"[..]));
        assert!(c.created_at.parse::<Timestamp>().is_ok(), "{c:?}");
    }
    assert!(!phone.host_command(&p.host.id(), ID).unwrap().answered);

    host.answer_host_command(ID, b"sealed answer").unwrap();
    // The same answer again is fine; another is refused, the first kept.
    host.answer_host_command(ID, b"sealed answer").unwrap();
    let e = host.answer_host_command(ID, b"other").unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("command_answered", 409), "{e:?}");

    assert_eq!(host.put_presence(b"announcement", 30).unwrap().commands, vec![]);
    let read = phone.host_command(&p.host.id(), ID).unwrap();
    assert_eq!((read.id.as_str(), read.answered, read.answer.as_deref()), (ID, true, Some(&b"sealed answer"[..])));
    assert_eq!(read.expires_at, sent.expires_at);
}

/// R-HOST-4: other bytes under a used id, or the same bytes to another
/// device or from another, are `command_id_taken`; an id that is not a UUID
/// and bytes out of size are refused naming the field.
#[test]
fn r_host_4_an_id_is_one_command_and_bad_fields_are_refused() {
    let p = person();
    let phone = p.phone.client(&p.url);
    phone.send_host_command(&p.host.id(), ID, b"first").unwrap();
    for e in [
        phone.send_host_command(&p.host.id(), ID, b"second").unwrap_err(),
        phone.send_host_command(&p.laptop.id(), ID, b"first").unwrap_err(),
        p.laptop.client(&p.url).send_host_command(&p.host.id(), ID, b"first").unwrap_err(),
    ] {
        assert_eq!((e.code().as_str(), e.status), ("command_id_taken", 409), "{e:?}");
    }

    let too_big = vec![0u8; 16 * 1024 + 1];
    let fresh = "1f9e8d7c-6b5a-4938-8271-605f4e3d2c1b";
    for (id, sealed, field) in [("cmd-1", &b"x"[..], "id"), (fresh, b"", "sealed"), (fresh, too_big.as_slice(), "sealed")] {
        let e = phone.send_host_command(&p.host.id(), id, sealed).unwrap_err();
        assert_eq!((e.code().as_str(), e.status), ("invalid", 422), "{id} {}", sealed.len());
        assert!(e.body.get("details").and_then(|d| d.get(field)).is_some(), "{e:?}");
    }
    phone.send_host_command(&p.host.id(), fresh, &vec![0u8; 16 * 1024]).unwrap();

    let host = p.host.client(&p.url);
    let e = host.answer_host_command(ID, &vec![0u8; 4 * 1024 + 1]).unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("invalid", 422), "{e:?}");
    host.answer_host_command(ID, &vec![0u8; 4 * 1024]).unwrap();
}

/// R-HOST-4: only the target answers and only the sender reads. The laptop
/// answering the host's command, or the sender answering its own, is
/// `no_command`; so is the host or the laptop reading it; the host's key
/// signed by the laptop is `device_mismatch`; and a device not on the
/// person's list is `no_device`.
#[test]
fn r_host_4_the_wrong_device_is_refused() {
    let p = person();
    let (host, phone, laptop) = (p.host.client(&p.url), p.phone.client(&p.url), p.laptop.client(&p.url));
    phone.send_host_command(&p.host.id(), ID, b"for the host").unwrap();

    for e in [laptop.answer_host_command(ID, b"x").unwrap_err(), phone.answer_host_command(ID, b"x").unwrap_err()] {
        assert_eq!((e.code().as_str(), e.status), ("no_command", 404), "{e:?}");
    }
    let crossed = p.laptop.client_as(&p.url, &p.host.key);
    let e = crossed.answer_host_command(ID, b"x").unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("device_mismatch", 403), "{e:?}");

    host.answer_host_command(ID, b"answer").unwrap();
    for e in [host.host_command(&p.host.id(), ID).unwrap_err(), laptop.host_command(&p.host.id(), ID).unwrap_err()] {
        assert_eq!((e.code().as_str(), e.status), ("no_command", 404), "{e:?}");
    }
    assert_eq!(laptop.put_presence(b"laptop", 30).unwrap().commands, vec![]);

    let stranger = DeviceKey::generate().id().to_string();
    for e in [phone.send_host_command(&stranger, "2f9e8d7c-6b5a-4938-8271-605f4e3d2c1b", b"x").unwrap_err(), phone.host_command(&stranger, ID).unwrap_err()] {
        assert_eq!((e.code().as_str(), e.status), ("no_device", 404), "{e:?}");
    }
    let unsigned = Client::new(&p.url, &p.phone.key);
    assert_eq!(unsigned.send_host_command(&p.host.id(), ID, b"x").unwrap_err().code(), "device_signature_missing");
}

/// R-HOST-4: ten minutes on, a command is no longer delivered and can no
/// longer be answered; the sender still reads it, unanswered.
#[test]
fn r_host_4_a_command_lapses_after_ten_minutes() {
    let p = person();
    let (host, phone) = (p.host.client(&p.url), p.phone.client(&p.url));
    phone.send_host_command(&p.host.id(), ID, b"command").unwrap();
    p.advance(9 * 60 + 59);
    assert_eq!(host.put_presence(b"a", 30).unwrap().commands.len(), 1);
    p.advance(1);
    assert_eq!(host.put_presence(b"a", 30).unwrap().commands, vec![]);
    let e = host.answer_host_command(ID, b"late").unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("no_command", 404), "{e:?}");
    assert!(!phone.host_command(&p.host.id(), ID).unwrap().answered);
}

/// R-HOST-4: a refresh carries at most sixteen commands, oldest first.
#[test]
fn r_host_4_a_refresh_carries_at_most_sixteen_oldest_first() {
    let p = person();
    let (host, phone) = (p.host.client(&p.url), p.phone.client(&p.url));
    let ids: Vec<String> = (0..18).map(|i| format!("{i:08x}-0000-4000-8000-000000000000")).collect();
    for id in ids.iter().rev() {
        p.advance(1);
        phone.send_host_command(&p.host.id(), id, b"c").unwrap();
    }
    let delivered: Vec<String> = host.put_presence(b"a", 30).unwrap().commands.into_iter().map(|c| c.id).collect();
    let oldest_first: Vec<String> = ids.iter().rev().take(16).cloned().collect();
    assert_eq!(delivered, oldest_first);
}

/// R-HOST-4: a device the dashboard revokes takes its commands with it,
/// sent and received.
#[test]
fn r_host_4_a_revoked_devices_commands_go() {
    let p = person();
    let (host, phone, laptop) = (p.host.client(&p.url), p.phone.client(&p.url), p.laptop.client(&p.url));
    phone.send_host_command(&p.host.id(), ID, b"from the phone").unwrap();
    let from_laptop = "3f9e8d7c-6b5a-4938-8271-605f4e3d2c1b";
    laptop.send_host_command(&p.host.id(), from_laptop, b"from the laptop").unwrap();

    let request = format!("POST /_settings/devices/{}/revocation HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", p.phone.id(), p.host.key);
    let answer = raw(p.addr, &request);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let left: Vec<String> = host.put_presence(b"a", 30).unwrap().commands.into_iter().map(|c| c.id).collect();
    assert_eq!(left, [from_laptop]);

    // Revoking a target: what was sent it is not there to poll, and nothing
    // more is sent it.
    let to_laptop = "4f9e8d7c-6b5a-4938-8271-605f4e3d2c1b";
    host.send_host_command(&p.laptop.id(), to_laptop, b"for the laptop").unwrap();
    let request = format!("POST /_settings/devices/{}/revocation HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", p.laptop.id(), p.host.key);
    assert!(raw(p.addr, &request).starts_with("HTTP/1.1 200"));
    for e in [host.host_command(&p.laptop.id(), to_laptop).unwrap_err(), host.send_host_command(&p.laptop.id(), to_laptop, b"again").unwrap_err()] {
        assert_eq!((e.code().as_str(), e.status), ("no_device", 404), "{e:?}");
    }
    assert_eq!(host.put_presence(b"a", 30).unwrap().commands, vec![], "the laptop's own command went with it");
}

/// R-HOST-4: a device the chain removes takes its commands with it.
#[test]
fn r_host_4_a_removed_devices_commands_go() {
    let p = person();
    let host = p.host.client(&p.url);
    p.laptop.client(&p.url).send_host_command(&p.host.id(), ID, b"from the laptop").unwrap();
    let (_, removed) = p.chain.batch(&p.user, vec![Change::Remove(p.laptop.subject("laptop"))], p.host.device.id(), &p.host.signing(), T0 + 2).unwrap();
    let post = ListPost {
        entries: removed.entries.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        links: removed.links.iter().map(|l| hex(l)).collect(),
        wraps: removed.wraps.iter().map(|(d, w)| (d.to_string(), hex(w))).collect(),
        start_over: false,
    };
    host.append_device_list(&post).unwrap();
    assert_eq!(host.put_presence(b"a", 30).unwrap().commands, vec![]);
}

/// R-SYNC-1: commands are sync's, so a free workspace is refused all three
/// calls with the paid plan's fix.
#[test]
fn r_sync_1_a_free_workspace_is_refused_commands() {
    let p = person();
    // The stand-in's FREE_PLAN_KEY_MARKER: a key with `free` in it.
    let free = p.phone.client_as(&p.url, "krowk_sk_free_commands");
    for e in [
        free.send_host_command(&p.host.id(), ID, b"x").unwrap_err(),
        free.host_command(&p.host.id(), ID).unwrap_err(),
        free.answer_host_command(ID, b"x").unwrap_err(),
    ] {
        assert_eq!((e.code().as_str(), e.status), ("sync_requires_paid_plan", 402), "{e:?}");
    }
}

