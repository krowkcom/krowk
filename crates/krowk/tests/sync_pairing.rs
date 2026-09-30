//! Adding a device by a short code, end to end on two scratch homes against
//! the stand-in registry (R-E2E-3; canon, engineering/devices.md → Adding a
//! device): the laptop, already on the person's device list, runs `krowk
//! devices add` and shows a code; the desktop runs `krowk sync join`, types
//! it, and ends up holding the person's user key and a pin of the chain
//! that adds it — and nothing reaches the chain unless both sides confirmed.
//!
//! The laptop's list is made here with krowk-client, as `krowk sync init`
//! will make it (D6), so this tests pairing alone.

#![cfg(all(feature = "harness", unix))]

use krowk_client::device_chain::{Chain, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceSigner};
use krowk_client::keystore::Keystore;
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::{UserKey, UserKeys};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One person's two machines: two keys, one workspace (`person_for`). The
/// laptop's has just signed in, as `sync init` needs.
const LAPTOP: &str = "krowk_sk_pairing#laptop-fresh";
const DESKTOP: &str = "krowk_sk_pairing#desktop";

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-pair-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["laptop", "desktop"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    root.canonicalize().unwrap()
}

fn now() -> u64 {
    jiff::Timestamp::now().as_second() as u64
}

/// krowk with `home` as HOME and nothing else of this machine's, with the
/// debug build's stand-in for the person at the terminal on.
fn krowk(home: &Path, api: &str, token: &str, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", api)
        .env("KROWK_TOKEN", token)
        .env("KROWK_DEVICE_NAME", home.file_name().unwrap())
        .env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1")
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn keys(home: &Path) -> Keystore {
    Keystore::new(&home.join(".krowk"))
}

/// The laptop set up as `sync init` sets a device up: its keys, the chain's
/// seq 0 with a recovery kit, generation 1 of the user key, and the pin.
fn set_up(home: &Path, api: &str) -> UserKey {
    let store = keys(home);
    let device = store.device_key().unwrap();
    let signing = store.signing_key().unwrap();
    let kit = RecoveryKit::generate().device();
    let me = Subject { kind: Kind::Device, name: "laptop".into(), os: "Linux".into(), device: device.public(), signing: signing.public() };
    let recovery = Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: kit.key.public(), signing: kit.signing.public() };
    let (_, batch) = Chain::start(me, &signing, Some((recovery, &kit.signing)), None, now()).unwrap();
    let post = krowk_api::sync::ListPost {
        carried: None,
        entries: batch.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: vec![],
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over: false,
    };
    let client = krowk_api::Client::new(api, LAPTOP).signed_by(DeviceSigner::new(device.id(), store.signing_key().unwrap()).shared());
    client.init_device_list(&post).unwrap();
    store.save_user_keys(&UserKeys::new(batch.newest.clone(), []).unwrap()).unwrap();
    store.save_device_list(&batch.entries).unwrap();
    batch.newest
}

/// The registry's chain, verified from seq 0.
fn served(api: &str) -> Chain {
    let list = krowk_api::Client::new(api, DESKTOP).device_list_all().unwrap();
    let entries: Vec<SignedEntry> = list.entries.iter().map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).unwrap(), &e2e::unhex(&e.signatures).unwrap()).unwrap()).collect();
    Chain::verify(&entries, None).unwrap()
}

/// Every stderr line of a child, as it comes.
fn lines(child: &mut Child) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    rx
}

/// The code `devices add` shows, `XXXX-XXXX`.
fn code_shown(rx: &std::sync::mpsc::Receiver<String>) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(code) = line.trim().strip_prefix("Code: ") {
                return code.split_whitespace().next().unwrap().to_string();
            }
            seen.push_str(&line);
            seen.push('\n');
        }
    }
    panic!("`krowk devices add` showed no code:\n{seen}");
}

fn rest(rx: &std::sync::mpsc::Receiver<String>) -> String {
    rx.try_iter().collect::<Vec<_>>().join("\n")
}

/// `sync join` with `typed` as what the person types.
fn join(home: &Path, api: &str, typed: &str) -> std::process::Output {
    join_as(home, api, DESKTOP, typed)
}

fn join_as(home: &Path, api: &str, token: &str, typed: &str) -> std::process::Output {
    let mut b = krowk(home, api, token, &["sync", "join", "--json"]).spawn().unwrap();
    b.stdin.take().unwrap().write_all(format!("{typed}\n").as_bytes()).unwrap();
    wait(b, Duration::from_secs(60))
}

fn wait(mut child: Child, within: Duration) -> std::process::Output {
    let deadline = Instant::now() + within;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("a krowk did not finish in {within:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

/// R-E2E-3: the whole pairing. The desktop keeps the user key the laptop
/// holds and a pin of the chain that adds it, its key now speaks for it,
/// and the chain the registry serves is that chain.
#[test]
fn r_e2e_3_a_device_added_by_its_code_holds_the_user_key_and_a_pin_of_the_chain() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("add");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let key = set_up(&laptop, &api);

    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add", "--json"]).spawn().unwrap();
    let a_err = lines(&mut a);
    let code = code_shown(&a_err);
    assert!(code.len() == 9 && code.as_bytes()[4] == b'-', "{code}");
    // Typed as a person might: lower case, no dash.
    let b = join(&desktop, &api, &code.replace('-', "").to_lowercase());
    assert!(b.status.success(), "{}{}", String::from_utf8_lossy(&b.stdout), String::from_utf8_lossy(&b.stderr));
    let a = wait(a, Duration::from_secs(30));
    let said = rest(&a_err);
    assert!(a.status.success(), "{}\n{said}", String::from_utf8_lossy(&a.stdout));
    assert!(said.contains("Add 'desktop' (Linux) to your devices? [Y/n]"), "{said}");

    let store = keys(&desktop);
    assert_eq!(*store.user_keys().unwrap().unwrap().newest(), key, "the desktop holds the laptop's user key");
    let chain = served(&api);
    assert_eq!(chain.devices().iter().filter(|d| d.kind == Kind::Device).count(), 2);
    let pinned = store.device_list().unwrap().unwrap();
    assert_eq!(pinned.head(), chain.head(), "pinned at the head that adds it");
    let desktop_id = store.device().unwrap().unwrap().id();
    let signer = DeviceSigner::new(desktop_id, store.signing_key().unwrap()).shared();
    let wraps = krowk_api::Client::new(&api, DESKTOP).signed_by(signer).user_key().unwrap();
    assert_eq!(wraps.wraps.len(), 1, "the user key is wrapped to the desktop");
    assert_eq!(keys(&laptop).device_list().unwrap().unwrap().head().seq, 1, "the laptop pinned the entry it posted");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-E2E-3: a wrong code ends the pairing on both sides. The laptop adds
/// nothing and never asks; the desktop keeps nothing and says to get a new
/// code — it does not try again.
#[test]
fn r_e2e_3_a_wrong_code_adds_nothing_and_the_new_device_asks_for_a_new_one() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("wrong");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    set_up(&laptop, &api);
    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let a_err = lines(&mut a);
    let code = code_shown(&a_err);
    let wrong = if code.starts_with('2') { "3" } else { "2" }.to_string() + &code[1..];
    let b = join(&desktop, &api, &wrong);
    let err = String::from_utf8_lossy(&b.stderr);
    assert!(!b.status.success() && err.contains("pairing_failed") && err.contains("krowk devices add") && err.contains("a code works once"), "{err}");
    let a = wait(a, Duration::from_secs(30));
    let said = rest(&a_err);
    assert!(!a.status.success() && !said.contains("[Y/n]"), "the laptop asked nothing: {said}");
    assert_eq!(served(&api).head().seq, 0, "nothing reached the chain");
    assert!(keys(&desktop).user_keys().unwrap().is_none() && keys(&desktop).device_list().unwrap().is_none());
    let _ = std::fs::remove_dir_all(&r);
}

/// A proxy to the registry that, once it has carried a request whose line
/// holds `after`, drops every connection after it: a network gone the
/// moment that step was sent. What crossed it is kept.
fn dropping_proxy(registry: SocketAddr, after: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    proxy(registry, after, true)
}

/// A proxy that carries everything but loses the answer to the one request
/// whose line holds `which`: the step landed, and its sender never heard.
fn losing_proxy(registry: SocketAddr, which: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    proxy(registry, which, false)
}

fn proxy(registry: SocketAddr, after: &'static str, cut_after: bool) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, cut) = (seen.clone(), Arc::new(AtomicBool::new(false)));
    std::thread::spawn(move || {
        for mut client in listener.incoming().flatten() {
            if cut.load(Ordering::SeqCst) {
                drop(client);
                continue;
            }
            let (log, cut) = (log.clone(), cut.clone());
            std::thread::spawn(move || {
                // One request a connection: krowk-api sends one and reads the
                // answer to the end.
                let mut buf = vec![0u8; 1 << 20];
                let mut got = Vec::new();
                let head_end = loop {
                    let n = client.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    got.extend_from_slice(&buf[..n]);
                    if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&got[..head_end]).to_string();
                let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                while got.len() < head_end + len {
                    let n = client.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                let line = head.lines().next().unwrap_or_default().to_string();
                log.lock().unwrap().push(line.clone());
                // Closed after the answer, both ways, so the answer ends.
                let first = got.windows(2).position(|w| w == b"\r\n").unwrap() + 2;
                got.splice(first..first, b"Connection: close\r\n".iter().copied());
                let mut upstream = TcpStream::connect(registry).unwrap();
                upstream.write_all(&got).unwrap();
                let mut answer = Vec::new();
                let _ = upstream.read_to_end(&mut answer);
                if line.contains(after) {
                    if !cut_after {
                        return;
                    }
                    cut.store(true, Ordering::SeqCst);
                }
                let _ = client.write_all(&answer);
            });
        }
    });
    (addr, seen)
}

/// R-E2E-3, and the MUST from PR #198's review: a connection dropped after
/// the new device confirmed is a failure like any other. The desktop ends
/// it, says to get a new code from the laptop, keeps nothing, and never
/// starts again with the code it was given.
#[test]
fn r_e2e_3_a_connection_dropped_after_confirming_asks_for_a_new_code() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("dropped");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    set_up(&laptop, &api);
    let (proxy, seen) = dropping_proxy(registry.addr(), "/confirmation");
    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let a_err = lines(&mut a);
    let code = code_shown(&a_err);
    let b = join(&desktop, &format!("http://{proxy}/v1"), &code);
    let err = String::from_utf8_lossy(&b.stderr);
    assert!(!b.status.success() && err.contains("pairing_failed") && err.contains("a code works once") && err.contains("on 'laptop' run `krowk devices add` again"), "{err}");
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.iter().filter(|l| l.contains("/join")).count(), 1, "one start with the code, never a second: {calls:?}");
    assert!(calls.last().unwrap().contains("/confirmation"), "{calls:?}");
    assert!(keys(&desktop).user_keys().unwrap().is_none(), "nothing was kept");
    // The laptop is left waiting for an ack that will not come, and adds
    // nothing while it waits.
    let _ = a.kill();
    let _ = a.wait();
    assert_eq!(served(&api).head().seq, 0, "nothing reached the chain");
    let _ = std::fs::remove_dir_all(&r);
}

/// R-E2E-3: an ack whose answer is lost may have landed, and it did. The
/// desktop sends nothing again, sees the laptop's entry on the list, claims
/// its device and keeps its keys — not a machine on the list that holds
/// nothing.
#[test]
fn r_e2e_3_an_ack_whose_answer_was_lost_still_joins() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("lost-ack");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let key = set_up(&laptop, &api);
    let (proxy, seen) = losing_proxy(registry.addr(), "/acknowledgement");
    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let a_err = lines(&mut a);
    let code = code_shown(&a_err);
    let b = join(&desktop, &format!("http://{proxy}/v1"), &code);
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    assert!(wait(a, Duration::from_secs(30)).status.success(), "{}", rest(&a_err));
    assert_eq!(seen.lock().unwrap().iter().filter(|l| l.contains("/acknowledgement")).count(), 1, "the ack was not sent again");
    assert_eq!(*keys(&desktop).user_keys().unwrap().unwrap().newest(), key);
    assert_eq!(served(&api).head().seq, 1);
    let _ = std::fs::remove_dir_all(&r);
}

/// A device a rotation left behind catches up before it adds one: the
/// laptop holds generation 1, a phone is added and removed behind its back
/// (generation 2), and `devices add` takes up generation 2 from its wrap —
/// held to the chain's id — then hands the desktop generation 2 and the
/// link that opens generation 1.
#[test]
fn a_device_behind_a_rotation_catches_up_before_it_adds_one() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("behind");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let first = set_up(&laptop, &api);
    // The rotation, made with the laptop's own keys, as another of its
    // runs would have; its keystore is left at generation 1.
    let store = keys(&laptop);
    let (device, signing) = (store.device().unwrap().unwrap(), store.signing_key().unwrap());
    let client = krowk_api::Client::new(&api, LAPTOP).signed_by(DeviceSigner::new(device.id(), store.signing_key().unwrap()).shared());
    let phone = Subject { kind: Kind::Device, name: "phone".into(), os: "iOS".into(), device: e2e::DeviceKey::generate().public(), signing: e2e::SigningKey::generate().public() };
    let chain = served(&api);
    let pinned = Chain::verify(&chain_entries(&api), Some(chain.head())).unwrap();
    let (added, add) = pinned.batch(&first, vec![krowk_client::device_chain::Change::Add(phone.clone())], device.id(), &signing, now()).unwrap();
    client.append_device_list(&post(&add)).unwrap();
    let (_, remove) = added.batch(&first, vec![krowk_client::device_chain::Change::Remove(phone)], device.id(), &signing, now()).unwrap();
    client.append_device_list(&post(&remove)).unwrap();
    assert_eq!(store.user_keys().unwrap().unwrap().newest().generation(), 1);

    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let a_err = lines(&mut a);
    let code = code_shown(&a_err);
    let b = join(&desktop, &api, &code);
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    assert!(wait(a, Duration::from_secs(30)).status.success(), "{}", rest(&a_err));
    let laptop_keys = store.user_keys().unwrap().unwrap();
    assert_eq!(laptop_keys.newest().generation(), 2, "the laptop caught up");
    let desktop_keys = keys(&desktop).user_keys().unwrap().unwrap();
    assert_eq!(desktop_keys.newest(), laptop_keys.newest());
    assert_eq!(desktop_keys.open(1).unwrap(), first, "the desktop opens what was sealed before it joined");
    let _ = std::fs::remove_dir_all(&r);
}

/// ^C on the laptop while it waits ends the pairing on the registry at
/// once, rather than leaving it open for its ten minutes.
#[test]
fn an_interrupted_add_ends_its_pairing() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("interrupted");
    let laptop = r.join("laptop");
    set_up(&laptop, &api);
    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let a_err = lines(&mut a);
    code_shown(&a_err);
    let b = krowk_api::Client::new(&api, DESKTOP);
    let open = b.find_open_pairing().unwrap();
    // SAFETY: signalling a child this test spawned.
    unsafe { libc::kill(a.id() as i32, libc::SIGINT) };
    assert!(!wait(a, Duration::from_secs(10)).status.success());
    assert_eq!(b.find_open_pairing().unwrap_err().code(), "no_pairing", "{}", open.id);
    let store = keys(&laptop);
    let signer = DeviceSigner::new(store.device().unwrap().unwrap().id(), store.signing_key().unwrap()).shared();
    let gone = krowk_api::Client::new(&api, LAPTOP).signed_by(signer).show_pairing(&open.id).unwrap_err();
    assert_eq!(gone.code(), "pairing_gone");
    let _ = std::fs::remove_dir_all(&r);
}

fn chain_entries(api: &str) -> Vec<SignedEntry> {
    let list = krowk_api::Client::new(api, DESKTOP).device_list_all().unwrap();
    list.entries.iter().map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).unwrap(), &e2e::unhex(&e.signatures).unwrap()).unwrap()).collect()
}

fn post(batch: &krowk_client::device_chain::Batch) -> krowk_api::sync::ListPost {
    krowk_api::sync::ListPost {
        carried: None,
        entries: batch.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| e2e::hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over: false,
    }
}

/// A machine that got the reply — and with it the user key — but never
/// acknowledged it is put on the list anyway, so the person can see it and
/// remove it: a machine that may hold the key is never left unlisted.
#[test]
fn a_new_device_that_never_acknowledges_is_listed_so_it_can_be_removed() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("unacked");
    let laptop = r.join("laptop");
    set_up(&laptop, &api);
    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let a_err = lines(&mut a);
    let code = krowk_client::pairing::PairingCode::parse(&code_shown(&a_err)).unwrap();
    // B by hand: it runs the pairing to the reply, then ends it unacked.
    let b = krowk_api::Client::new(&api, DESKTOP);
    let user = b.verify_key().unwrap().user_id;
    let open = b.find_open_pairing().unwrap();
    let (key, signing) = (e2e::DeviceKey::generate(), e2e::SigningKey::generate());
    let me = krowk_client::pairing::NewDevice { device: key.public(), signing: signing.public(), name: "stranger".into(), os: "Linux".into() };
    let a_id = e2e::DeviceId::parse(&open.initiator_device.id).unwrap();
    let binding = krowk_client::pairing::Binding { kind: krowk_client::pairing::PeerKind::SamePersonDevice, user_id: user, a_device: a_id, b_device: key.id() };
    let (pb, hello) = krowk_client::pairing::PairB::start(binding, code, me).unwrap();
    b.pairing_step(&open.id, krowk_api::sync::PairingStep::Join, &hello).unwrap();
    let field = |f: fn(&krowk_api::sync::Pairing) -> &String| loop {
        let p = b.show_pairing(&open.id).unwrap();
        if !f(&p).is_empty() {
            break e2e::unhex(f(&p)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let (await_reply, confirm) = pb.receive_spake(&field(|p| &p.initiator_message)).unwrap();
    b.pairing_step(&open.id, krowk_api::sync::PairingStep::Confirmation, &confirm).unwrap();
    await_reply.receive_reply(&field(|p| &p.sealed_reply)).unwrap();
    b.end_pairing(&open.id).unwrap();

    let a = wait(a, Duration::from_secs(30));
    let said = rest(&a_err);
    assert!(!a.status.success() && said.contains("pairing_unconfirmed") && said.contains("krowk devices remove 'stranger'"), "{said}");
    let chain = served(&api);
    assert!(chain.devices().iter().any(|d| d.name == "stranger"), "listed, so it can be removed");
    let _ = std::fs::remove_dir_all(&r);
}

/// The code is never an argument: `sync join CODE` is refused, saying the
/// code is in the shell's history now, and `devices add` takes none.
#[test]
fn a_code_given_as_an_argument_is_refused() {
    let r = root("argv");
    let out = krowk(&r.join("desktop"), "http://127.0.0.1:9/v1", DESKTOP, &["sync", "join", "K7QF-9M3X"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("bad_argument") && err.contains("never as an argument"), "{err}");
    let out = krowk(&r.join("laptop"), "http://127.0.0.1:9/v1", LAPTOP, &["devices", "add", "desktop"]).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("bad_argument"));
    let _ = std::fs::remove_dir_all(&r);
}

/// A machine removed from the list cannot bring its old keys back: `sync
/// join` says so before any code is typed, rather than after the person
/// has said yes on the other device.
#[test]
fn a_removed_machine_is_told_to_join_with_new_keys() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let r = root("removed");
    let (laptop, desktop) = (r.join("laptop"), r.join("desktop"));
    let key = set_up(&laptop, &api);
    let mut a = krowk(&laptop, &api, LAPTOP, &["devices", "add"]).spawn().unwrap();
    let a_err = lines(&mut a);
    let code = code_shown(&a_err);
    assert!(join(&desktop, &api, &code).status.success());
    assert!(wait(a, Duration::from_secs(30)).status.success());
    let store = keys(&laptop);
    let (device, signing) = (store.device().unwrap().unwrap(), store.signing_key().unwrap());
    let chain = served(&api);
    let gone = chain.devices().iter().find(|d| d.name == "desktop").unwrap();
    let subject = Subject { kind: Kind::Device, name: gone.name.clone(), os: gone.os.clone(), device: gone.device, signing: gone.signing };
    let pinned = Chain::verify(&chain_entries(&api), Some(chain.head())).unwrap();
    let (_, remove) = pinned.batch(&key, vec![krowk_client::device_chain::Change::Remove(subject)], device.id(), &signing, now()).unwrap();
    krowk_api::Client::new(&api, LAPTOP).signed_by(DeviceSigner::new(device.id(), store.signing_key().unwrap()).shared()).append_device_list(&post(&remove)).unwrap();
    // Its key went with it; signed in again, it is told to start afresh.
    let again = join_as(&desktop, &api, "krowk_sk_pairing#desktop-again", "");
    let err = String::from_utf8_lossy(&again.stderr);
    assert!(!again.status.success() && err.contains("device_removed"), "{err}");
    let _ = std::fs::remove_dir_all(&r);
}
