//! Same session, two devices (Harness P1 ticket 19): a session running in
//! machine A's host daemon, synced by A's bridge through the stand-in
//! registry and the reference relay, attached from machine B — another
//! device, with keys of its own and the same account key — which watches
//! it, prompts it, answers its approvals, rides out A's network going away,
//! and queues a prompt while A is gone. Everything runs in this process:
//! the daemon on a thread, the registry and the relay on theirs, and a
//! proxy in front of the relay per device that records every byte and can
//! cut A off.

#![cfg(unix)]

#[path = "common/mock.rs"]
mod mock;
#[path = "common/scratch.rs"]
mod scratch;

use krowk_client::e2e::{self, AccountKey, DeviceKey, SigningKey};
use krowk_harness::daemon::{self, client::Client, server};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{ApprovalDecision, Command, LiveEvent, PermissionMode, StreamLine};
use krowk_harness::sync::{direct, host, viewer};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Text only the two devices may ever read: the prompt B types and the
/// answer A's model gives. The capture of the relay's traffic must not hold
/// it (R-E2E-1).
const PROMPT_MARKER: &str = "plaintext-marker-prompt-6d1f";
const ANSWER_MARKER: &str = "plaintext-marker-answer-93be";

/// A model that answers any prompt with the marker, except one asking for
/// the file, which it makes with `bash` — an approval — and then answers.
fn model(body: &serde_json::Value, _n: usize) -> mock::Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or_default();
    let has_result = last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    if !has_result && last.to_string().contains("a long answer") {
        let words: String = (0..160).map(|i| format!("word{i} ")).collect();
        return mock::Reply::paced(mock::text_stream(&words), Duration::from_millis(20));
    }
    if !has_result && last.to_string().contains("make the file") {
        return mock::Reply::sse(&mock::tool_use("toolu_01Touch", "bash", &serde_json::json!({"command": "touch approved.txt"})));
    }
    mock::Reply::paced(mock::text_stream(&format!("{ANSWER_MARKER} one two three four")), Duration::from_millis(20))
}

struct World {
    root: PathBuf,
    registry: krowk_devregistry::Running,
    api: String,
    account: AccountKey,
    /// Every byte either device sent the relay or got from it.
    seen: Arc<Mutex<Vec<u8>>>,
    /// While set, A's link to the relay is down and nothing gets through.
    cut: Arc<AtomicBool>,
    relay_a: String,
    relay_b: String,
    _mock: mock::Mock,
    mock_url: String,
    /// A's own way to the registry: `PASS`, `CUT` or `FAIL_WRITES`.
    reg_a: String,
    reg_mode: Arc<AtomicU8>,
    /// What B's proxy does with what the relay sends B: `PASS`, `CUT`,
    /// `DROP_ALL`, or `DROP_BATCHES` (stream batches only).
    b_mode: Arc<AtomicU8>,
    /// Stream batches B's proxy handed on.
    b_batches: Arc<AtomicUsize>,
    /// Under `DROP_BATCHES`, how many more to drop before passing again.
    b_drop: Arc<AtomicUsize>,
}

const PASS: u8 = 0;
const CUT: u8 = 1;
const FAIL_WRITES: u8 = 2;
const DROP_ALL: u8 = 2;
const DROP_BATCHES: u8 = 3;

struct Device {
    key: DeviceKey,
    signing: SigningKey,
}

fn signer(d: &Device) -> Arc<dyn krowk_api::client::RequestSigner> {
    e2e::DeviceSigner::new(d.key.id(), SigningKey::from_secret(&*d.signing.secret_bytes()).unwrap()).shared()
}

impl World {
    fn new(name: &str) -> World {
        let root = scratch::root(&format!("sync-{name}"));
        for d in ["home", "run", "repo/.git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        let api = format!("{}/v1", registry.url());
        let relay = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
        let roster = krowk_harness::relay::Roster::parse(&roster).unwrap();
        std::thread::spawn(move || krowk_harness::relay::run(relay, krowk_harness::relay::Config { roster, origin: None, limits: Default::default(), state: None, origins: Vec::new(), whois: None, pin: None }));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cut = Arc::new(AtomicBool::new(false));
        let relay_a = proxy(relay_addr, seen.clone(), cut.clone());
        let (b_mode, b_batches) = (Arc::new(AtomicU8::new(PASS)), Arc::new(AtomicUsize::new(0)));
        let b_drop = Arc::new(AtomicUsize::new(0));
        let relay_b = viewer_proxy(relay_addr, seen.clone(), b_mode.clone(), b_batches.clone(), b_drop.clone());
        let reg_mode = Arc::new(AtomicU8::new(PASS));
        let reg_a = format!("http://{}/v1", registry_proxy(registry.addr(), reg_mode.clone()));
        let m = mock::serve(model);
        let mock_url = m.url.clone();
        World { root, registry, api, account: AccountKey::generate(), seen, cut, relay_a: format!("ws://{relay_a}"), relay_b: format!("ws://{relay_b}"), _mock: m, mock_url, reg_a, reg_mode, b_mode, b_batches, b_drop }
    }

    /// A device's own registry client: its calls that act as the device
    /// signed by its key, as the registry requires.
    fn as_device(&self, d: &Device) -> Arc<krowk_api::Client> {
        Arc::new(krowk_api::Client::new(&self.api, "krowk_sk_sync_attach_0000000000000000").signed_by(signer(d)))
    }

    /// A's registry client, through A's own proxy, signed by A.
    fn a_client(&self, a: &Device) -> Arc<krowk_api::Client> {
        let mut c = krowk_api::Client::new(&self.reg_a, "krowk_sk_sync_attach_0000000000000000").signed_by(signer(a));
        c.sleep = |_| std::thread::sleep(Duration::from_millis(50));
        Arc::new(c)
    }

    fn client(&self) -> Arc<krowk_api::Client> {
        Arc::new(krowk_api::Client::new(&self.api, "krowk_sk_sync_attach_0000000000000000"))
    }

    /// A device of the workspace, registered with its signing key.
    fn device(&self, name: &str) -> Device {
        let d = Device { key: DeviceKey::generate(), signing: SigningKey::generate() };
        self.as_device(&d).register_device(&e2e::hex(&d.key.public().0), &e2e::hex(&d.signing.public().0), name, &self.account.id().to_string()).unwrap();
        d
    }

    fn env(&self) -> impl Fn(&str) -> String + Clone + Send + 'static {
        let (root, url) = (self.root.clone(), self.mock_url.clone());
        move |k| match k {
            "HOME" => root.join("home").display().to_string(),
            "XDG_RUNTIME_DIR" => root.join("run").display().to_string(),
            "ANTHROPIC_API_KEY" => "sk-test".into(),
            "ANTHROPIC_BASE_URL" => url.clone(),
            _ => String::new(),
        }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    /// Machine A's host daemon, on a thread of its own.
    fn serve(&self) {
        let (env, socket) = (self.env(), daemon::socket(&self.env()).unwrap());
        let credentials = self.root.join("home/.krowk/credentials.json");
        std::thread::spawn(move || {
            let factory: server::Factory = Box::new(move |cwd: &Path, answers: bool| {
                Ok(HostConfig {
                    sessions_dir: log::sessions_dir(&env).unwrap(),
                    cwd: cwd.to_path_buf(),
                    registry: Registry::resolve(&InstancesConfig::default(), &env),
                    krowk_version: "test".into(),
                    pricer: Arc::new(|_, _, _| None),
                    catalog: Arc::new(|_, _| None),
                    credentials: credentials.clone(),
                    trust: krowk_harness::trust::allow_all(),
                    publisher: None,
                    permissions: krowk_harness::permissions::Config { approvals: answers, ..Default::default() },
                    agents: krowk_harness::subagent::AgentsConfig::none(),
                })
            });
            server::run(server::Options { socket, idle: None, krowk_version: "test".into(), ..Default::default() }, factory)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(daemon::socket(&self.env()).unwrap()).is_err() {
            assert!(Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    async fn daemon(&self) -> Arc<Client> {
        Arc::new(Client::connect(&daemon::socket(&self.env()).unwrap(), &self.repo(), "test", true).await.ok().expect("the daemon answers"))
    }

    fn prompt(&self, session: Option<&str>, text: &str) -> Command {
        let model = Registry::resolve(&InstancesConfig::default(), &self.env()).parse_model("claude-sonnet-4-6").unwrap();
        Command::Prompt { session_id: session.map(String::from), text: text.into(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }
    }

    /// A's bridge for `session`, running until the handle is stopped.
    fn bridge(&self, a: &Device, session: &str, daemon: Arc<Client>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        self.bridge_with(a, session, daemon, host::LEASE_TTL, host::KEEP)
    }

    fn bridge_with(&self, a: &Device, session: &str, daemon: Arc<Client>, ttl: u64, keep: usize) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let o = host::Options {
            relay: self.relay_a.clone(),
            env: "development".into(),
            api: self.a_client(a),
            device: a.key.id(),
            signing: SigningKey::from_secret(&*a.signing.secret_bytes()).unwrap(),
            account: AccountKey::from_bytes(*self.account.as_bytes()),
            session: session.into(),
            title: "the title is sealed too".into(),
            cwd: self.repo().display().to_string(),
            ttl,
            keep,
            direct: None,
        };
        self.run_bridge(o, daemon)
    }

    /// A's bridge with a direct listener, reading this node from a fake
    /// tailscaled at `ts`; `stop` kills the listener.
    fn bridge_direct(&self, a: &Device, session: &str, daemon: Arc<Client>, ts: &FakeTailscale, same_user: bool, stop: watch::Receiver<bool>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
        let o = host::Options {
            relay: self.relay_a.clone(),
            env: "development".into(),
            api: self.a_client(a),
            device: a.key.id(),
            signing: SigningKey::from_secret(&*a.signing.secret_bytes()).unwrap(),
            account: AccountKey::from_bytes(*self.account.as_bytes()),
            session: session.into(),
            title: "the title is sealed too".into(),
            cwd: self.repo().display().to_string(),
            ttl: host::LEASE_TTL,
            keep: host::KEEP,
            direct: Some(direct::Config { socket: ts.socket.clone(), roster: krowk_harness::relay::Roster::parse(&roster).unwrap(), same_user, lan: false, stop: Some(stop) }),
        };
        self.run_bridge(o, daemon)
    }

    fn run_bridge(&self, o: host::Options, daemon: Arc<Client>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let (stop, stop_rx) = watch::channel(false);
        let (cp, cp_rx) = mpsc::unbounded_channel();
        (stop, cp, tokio::spawn(host::run(o, daemon, stop_rx, cp_rx)))
    }

    fn viewer(&self, b: &Device, session: &str) -> viewer::Options {
        viewer::Options {
            relay: self.relay_b.clone(),
            env: "development".into(),
            api: self.as_device(b),
            device: b.key.id(),
            signing: SigningKey::from_secret(&*b.signing.secret_bytes()).unwrap(),
            account: AccountKey::from_bytes(*self.account.as_bytes()),
            session: session.into(),
            known: None,
        }
    }

    /// Waits until the session's sealed index names a checkpoint and a head:
    /// the bridge has taken the session up and written it.
    async fn synced(&self, session: &str) {
        let (api, account, id) = (self.client(), AccountKey::from_bytes(*self.account.as_bytes()), session.to_string());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (api, id, account) = (api.clone(), id.clone(), AccountKey::from_bytes(*account.as_bytes()));
            let ready = tokio::task::spawn_blocking(move || -> Option<bool> {
                let s = api.show_sync_session(&id).ok()?;
                let key = e2e::unwrap_session_key(&e2e::unhex(&s.wrapped_key)?, &krowk_harness::daemon::ws::uuid(&id), &account).ok()?;
                let index = krowk_harness::sync::store::open_index(&key, &id, &s.sealed_index).ok()?;
                Some(s.lease.is_some() && index.checkpoint.is_some() && index.head.is_some())
            })
            .await
            .unwrap();
            if ready == Some(true) {
                return;
            }
            assert!(Instant::now() < deadline, "the bridge never synced the session");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn captured(&self) -> Vec<u8> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = &self.registry;
    }
}

/// A TCP proxy in front of the relay: records both ways, and while `cut`
/// is set drops every connection and refuses new ones — a network gone.
fn proxy(to: SocketAddr, seen: Arc<Mutex<Vec<u8>>>, cut: Arc<AtomicBool>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            if cut.load(Ordering::SeqCst) {
                drop(c);
                continue;
            }
            let Ok(up) = TcpStream::connect(to) else { continue };
            for (mut from, mut into) in [(c.try_clone().unwrap(), up.try_clone().unwrap()), (up, c)] {
                let (seen, cut) = (seen.clone(), cut.clone());
                std::thread::spawn(move || {
                    from.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                    let mut buf = [0u8; 16384];
                    loop {
                        if cut.load(Ordering::SeqCst) {
                            let _ = from.shutdown(std::net::Shutdown::Both);
                            let _ = into.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                        match from.read(&mut buf) {
                            Ok(0) => {
                                let _ = into.shutdown(std::net::Shutdown::Both);
                                return;
                            }
                            Ok(n) => {
                                seen.lock().unwrap().extend_from_slice(&buf[..n]);
                                if into.write_all(&buf[..n]).is_err() {
                                    return;
                                }
                            }
                            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                            Err(_) => {
                                let _ = into.shutdown(std::net::Shutdown::Both);
                                return;
                            }
                        }
                    }
                });
            }
        }
    });
    addr
}

/// A's registry proxy: `CUT` drops every connection, `FAIL_WRITES` answers
/// every write (a POST or PUT) 503 — a registry failing, not gone.
fn registry_proxy(to: SocketAddr, mode: Arc<AtomicU8>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            if mode.load(Ordering::SeqCst) == CUT {
                drop(c);
                continue;
            }
            let mode = mode.clone();
            std::thread::spawn(move || {
                // Each request on the connection is judged as it comes and
                // sent upstream on a connection of its own; the client may
                // keep this one alive.
                let mut buf = [0u8; 16384];
                let mut req = Vec::new();
                c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                loop {
                    let whole = loop {
                        if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
                            let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                            if req.len() >= end + 4 + len {
                                break Some((end, end + 4 + len));
                            }
                        }
                        match c.read(&mut buf) {
                            Ok(0) | Err(_) => break None,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    };
                    let Some((end, total)) = whole else { return };
                    let one: Vec<u8> = req.drain(..total).collect();
                    let m = mode.load(Ordering::SeqCst);
                    if m == CUT {
                        return;
                    }
                    if m == FAIL_WRITES && (one.starts_with(b"POST ") || one.starts_with(b"PUT ")) {
                        let _ = c.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                        return;
                    }
                    // Bytes, not text: a chunk upload's body is ciphertext.
                    let line = one.windows(2).position(|w| w == b"\r\n").unwrap_or(0);
                    let head = String::from_utf8_lossy(&one[..end]).to_ascii_lowercase();
                    let one = if head.contains("\r\nconnection:") { one } else { [&one[..line + 2], b"Connection: close\r\n", &one[line + 2..]].concat() };
                    let Ok(mut up) = TcpStream::connect(to) else { return };
                    if up.write_all(&one).is_err() {
                        return;
                    }
                    let mut resp = Vec::new();
                    let _ = up.read_to_end(&mut resp);
                    // The answer closes: the client opens another for its next.
                    if c.write_all(&resp).is_err() {
                        return;
                    }
                    let rhead = resp.windows(4).position(|w| w == b"\r\n\r\n").map(|e| String::from_utf8_lossy(&resp[..e]).to_ascii_lowercase()).unwrap_or_default();
                    if rhead.contains("connection: close") {
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// B's relay proxy: records both ways; what the relay sends B passes, is
/// cut, is dropped whole, or has its stream batches dropped, by `mode`.
fn viewer_proxy(to: SocketAddr, seen: Arc<Mutex<Vec<u8>>>, mode: Arc<AtomicU8>, batches: Arc<AtomicUsize>, drop_left: Arc<AtomicUsize>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            if mode.load(Ordering::SeqCst) == CUT {
                drop(c);
                continue;
            }
            let Ok(up) = TcpStream::connect(to) else { continue };
            for (down, (mut from, mut into)) in [(false, (c.try_clone().unwrap(), up.try_clone().unwrap())), (true, (up, c))] {
                let (seen, mode, batches, drop_left) = (seen.clone(), mode.clone(), batches.clone(), drop_left.clone());
                std::thread::spawn(move || {
                    let mut pend: Vec<u8> = Vec::new();
                    let mut shook = false;
                    from.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
                    let mut buf = [0u8; 16384];
                    loop {
                        if mode.load(Ordering::SeqCst) == CUT {
                            let _ = from.shutdown(std::net::Shutdown::Both);
                            let _ = into.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                        let n = match from.read(&mut buf) {
                            Ok(0) => {
                                let _ = into.shutdown(std::net::Shutdown::Both);
                                return;
                            }
                            Ok(n) => n,
                            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                            Err(_) => {
                                let _ = into.shutdown(std::net::Shutdown::Both);
                                return;
                            }
                        };
                        if !down {
                            seen.lock().unwrap().extend_from_slice(&buf[..n]);
                            if into.write_all(&buf[..n]).is_err() {
                                return;
                            }
                            continue;
                        }
                        // Relay to B: whole WebSocket frames (unmasked).
                        pend.extend_from_slice(&buf[..n]);
                        if !shook {
                            let Some(i) = pend.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                            let h: Vec<u8> = pend.drain(..i + 4).collect();
                            if into.write_all(&h).is_err() {
                                return;
                            }
                            shook = true;
                        }
                        while pend.len() >= 2 {
                            let (op, l7) = (pend[0] & 0x0f, (pend[1] & 0x7f) as usize);
                            let (hl, len) = match l7 {
                                126 if pend.len() >= 4 => (4, u16::from_be_bytes([pend[2], pend[3]]) as usize),
                                127 if pend.len() >= 10 => (10, u64::from_be_bytes(pend[2..10].try_into().unwrap()) as usize),
                                126 | 127 => break,
                                l => (2, l),
                            };
                            if pend.len() < hl + len {
                                break;
                            }
                            let f: Vec<u8> = pend.drain(..hl + len).collect();
                            let p = &f[hl..];
                            // A stream batch: kind 1 with a seq (a routed one's is 0).
                            let batch = op == 2 && p.len() >= 28 && p[1] == 1 && p[20..28] != [0; 8];
                            let m = mode.load(Ordering::SeqCst);
                            if m == DROP_ALL {
                                continue;
                            }
                            if batch && m == DROP_BATCHES && drop_left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
                                continue;
                            }
                            if batch {
                                batches.fetch_add(1, Ordering::SeqCst);
                            }
                            seen.lock().unwrap().extend_from_slice(&f);
                            if into.write_all(&f).is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        }
    });
    addr
}

/// Reads updates until `done` says so, or panics after `wait`. Every frame
/// handed on is timed, for R-LAG-7.
async fn until(v: &mut viewer::Viewer, wait: Duration, frames: &mut Vec<Instant>, mut done: impl FnMut(&viewer::Update) -> bool) -> Vec<viewer::Update> {
    let deadline = Instant::now() + wait;
    let mut all = Vec::new();
    loop {
        let left = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| panic!("timed out; saw {all:#?}"));
        let batch = tokio::time::timeout(left, v.updates.recv()).await.unwrap_or_else(|_| panic!("timed out; saw {all:#?}")).expect("the viewer runs");
        frames.push(Instant::now());
        let hit = batch.iter().any(&mut done);
        all.extend(batch);
        if hit {
            return all;
        }
    }
}

fn result_of(u: &viewer::Update) -> bool {
    matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::Result(_))))
}

fn logged(u: &viewer::Update) -> Vec<String> {
    match u {
        viewer::Update::Line(StreamLine::Log(e)) => vec![e.id.clone()],
        viewer::Update::Attached { events, .. } | viewer::Update::CaughtUp(events) => events.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

/// The ids of every event in A's log of `session`.
fn log_ids(w: &World, session: &str) -> Vec<String> {
    let dir = log::sessions_dir(&w.env()).unwrap().join(session);
    std::fs::read_to_string(dir.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["id"].as_str().unwrap().to_string()).collect()
}

async fn first_turn(w: &World) -> (Arc<Client>, String) {
    w.serve();
    let d = w.daemon().await;
    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let r = d.execute(w.prompt(None, "say hello"), tx).await.unwrap().unwrap();
    (d, r.session_id)
}

/// R-SYNC-1, R-SYNC-2, R-PERF-6, R-PERM-2, R-LAG-7, R-LAG-8, R-E2E-1: B
/// lists A's session from its sealed index and attaches from the checkpoint
/// in under 500 ms; a prompt typed on B runs on A, shown on B at once and
/// settled by A's ack; A's approval request reaches B and B's answer
/// unblocks A; B is handed at most one batch of updates per display frame;
/// and the relay's traffic, captured, holds neither B's prompt nor A's
/// answer.
#[tokio::test]
async fn r_sync_2_a_prompt_typed_on_b_runs_on_a_and_b_answers_its_approval() {
    let w = World::new("prompt");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;

    let (api, account) = (w.client(), AccountKey::from_bytes(*w.account.as_bytes()));
    let (listed, unreadable) = tokio::task::spawn_blocking(move || viewer::list(&api, &account)).await.unwrap().unwrap();
    assert_eq!(unreadable, 0);
    let s = listed.iter().find(|s| s.id == session).expect("B lists A's session");
    assert_eq!(s.index.title, "the title is sealed too");
    assert!(s.index.checkpoint.is_some() && s.index.head.is_some(), "{:?}", s.index);

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    println!("R-PERF-6 remote.attach: {:?}", v.attach_time);
    assert!(v.attach_time < Duration::from_millis(500), "R-PERF-6: attaching from the checkpoint took {:?}", v.attach_time);
    let mut frames = Vec::new();
    let got = until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    let viewer::Update::Attached { events, .. } = &got[0] else { panic!("{got:?}") };
    assert!(events.iter().any(|e| e.to_string().contains("say hello")), "the checkpoint holds the first turn");

    // B may ask nothing of another session, nor start one on A.
    for other in [None, Some("01a0ec7b-0000-7000-8000-000000000000")] {
        v.commands.send(w.prompt(other, "not this session")).unwrap();
        let got = until(&mut v, Duration::from_secs(5), &mut frames, |u| matches!(u, viewer::Update::Acked { .. })).await;
        assert!(got.iter().any(|u| matches!(u, viewer::Update::Acked { error: Some(e), .. } if e.contains("nothing else"))), "{got:?}");
    }

    // B's prompt runs under the session's settings, whatever it asks for.
    let mut unhinged = w.prompt(Some(&session), "asking for more than it may");
    if let Command::Prompt { permission_mode, model, .. } = &mut unhinged {
        *permission_mode = PermissionMode::Unhinged;
        *model = Some(Registry::resolve(&InstancesConfig::default(), &w.env()).parse_model("claude-haiku-4-5").unwrap());
    }
    v.commands.send(unhinged).unwrap();
    until(&mut v, Duration::from_secs(15), &mut frames, result_of).await;
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&session).join("events.jsonl")).unwrap();
    let started: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).filter(|e| e["type"] == "turn.started").collect();
    let (first, last) = (&started[0], started.last().unwrap());
    assert_eq!(last["permissionMode"], "default", "never the viewer's unhinged: {last}");
    assert_eq!(last["model"], first["model"], "never the viewer's model: {last}");

    // B types; A runs it.
    v.commands.send(w.prompt(Some(&session), &format!("make the file {PROMPT_MARKER}"))).unwrap();
    let got = until(&mut v, Duration::from_secs(15), &mut frames, |u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(_))))).await;
    let sent = got.iter().position(|u| matches!(u, viewer::Update::Sent { queued: false, .. })).expect("R-LAG-8: the prompt is shown at once");
    let acked = got.iter().position(|u| matches!(u, viewer::Update::Acked { error: None, .. })).expect("and settled by A's ack");
    assert!(sent < acked);
    let req = got.iter().find_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) = u { Some(r.clone()) } else { None }).unwrap();
    assert_eq!(req.tool, "bash");
    assert!(!w.repo().join("approved.txt").exists(), "A waits on the approval");
    v.commands.send(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow }).unwrap();
    until(&mut v, Duration::from_secs(15), &mut frames, result_of).await;
    assert!(w.repo().join("approved.txt").exists(), "R-PERM-2: B's answer unblocked A's turn");

    // The prefix check, live: told of a head past what the registry holds,
    // B refuses the log it is served.
    let mut o = w.viewer(&b, &session);
    let head = s.index.head.unwrap();
    o.known = Some(krowk_harness::sync::store::Head { index: head.index + 5, digest: [0; 32] });
    let e = viewer::attach(o).await.err().expect("a prefix is refused");
    assert!(e.contains("prefix"), "{e}");

    // R-LAG-7: never two hand-offs inside one display frame.
    let handed = v.handed.lock().unwrap().clone();
    assert!(handed.len() > 3, "{handed:?}");
    let tightest = handed.windows(2).map(|p| p[1] - p[0]).min().unwrap();
    assert!(tightest >= Duration::from_millis(12), "two frames {tightest:?} apart");

    // R-E2E-1: the relay carried the session, and never a word of it.
    let cap = w.captured();
    assert!(cap.len() > 4096, "the capture saw the traffic ({} bytes)", cap.len());
    for marker in [PROMPT_MARKER, ANSWER_MARKER, "say hello", "approved.txt"] {
        assert!(!cap.windows(marker.len()).any(|x| x == marker.as_bytes()), "{marker} crossed the relay in the clear");
    }
}

/// R-OFF-2: A's network is gone for 30 seconds while A runs a turn by
/// itself; once it is back, sync resumes with nothing more than waiting,
/// and B holds every event A's log holds.
#[tokio::test]
async fn r_off_2_sync_resumes_by_itself_after_a_30_second_cut_with_no_event_lost() {
    let w = World::new("cut");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for u in until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await {
        ids.extend(logged(&u));
    }

    w.cut.store(true, Ordering::SeqCst);
    // Work goes on at A.
    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    d.execute(w.prompt(Some(&session), "while the network is gone"), tx).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    w.cut.store(false, Ordering::SeqCst);

    // What the live stream brought and what the chunks did, together, once.
    let want = log_ids(&w, &session);
    let deadline = Instant::now() + Duration::from_secs(20);
    while want.iter().any(|i| !ids.contains(i)) && Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(500), v.updates.recv()).await {
            ids.extend(batch.iter().flat_map(logged));
        }
    }
    let missing: Vec<_> = want.iter().filter(|i| !ids.contains(i)).collect();
    assert!(missing.is_empty(), "B never got {missing:?}");
    let mut dedup = ids.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), ids.len(), "no event arrived twice");
}

/// R-HAND-4: with A's bridge gone, B attaches from the chunks read-only, a
/// prompt it types is queued rather than run, and it runs once A is back.
#[tokio::test]
async fn r_hand_4_with_a_offline_b_is_read_only_and_its_queued_prompt_runs_when_a_returns() {
    let w = World::new("queue");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (stop, _cp, bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    stop.send(true).unwrap();
    bridge.await.unwrap().unwrap();

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(false))).await;
    v.commands.send(w.prompt(Some(&session), "queued while A was away")).unwrap();
    until(&mut v, Duration::from_secs(5), &mut frames, |u| matches!(u, viewer::Update::Sent { queued: true, .. })).await;
    let before = log_ids(&w, &session).len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(log_ids(&w, &session).len(), before, "nothing ran while A was away");

    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    until(&mut v, Duration::from_secs(20), &mut frames, result_of).await;
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&session).join("events.jsonl")).unwrap();
    assert!(log.contains("queued while A was away"), "the queued prompt ran on A");
}

/// The session's events as the registry holds them, read at rest by a
/// device that has never seen it: what every later attach gets.
async fn at_rest_ids(w: &World, session: &str) -> Result<Vec<String>, String> {
    let (api, account, id) = (w.client(), AccountKey::from_bytes(*w.account.as_bytes()), session.to_string());
    tokio::task::spawn_blocking(move || {
        let s = api.show_sync_session(&id).map_err(|e| e.to_string())?;
        let key = e2e::unwrap_session_key(&e2e::unhex(&s.wrapped_key).unwrap(), &krowk_harness::daemon::ws::uuid(&id), &account).map_err(|e| e.to_string())?;
        let index = krowk_harness::sync::store::open_index(&key, &id, &s.sealed_index)?;
        let a = krowk_harness::sync::store::attach(&api, &key, &id, index, None)?;
        Ok(a.events.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect())
    })
    .await
    .unwrap()
}

/// Waits until what the registry holds is A's whole log.
async fn stored_whole(w: &World, session: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let want = log_ids(w, session);
        match at_rest_ids(w, session).await {
            Ok(got) if got == want => return,
            Ok(got) if Instant::now() > deadline => panic!("the registry holds {} of A's {} events: {:?}", got.len(), want.len(), want.iter().filter(|i| !got.contains(i)).collect::<Vec<_>>()),
            Err(e) if Instant::now() > deadline => panic!("the log no longer reads: {e}"),
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

async fn turn(d: &Client, w: &World, session: &str, text: &str) {
    let (tx, mut rx) = mpsc::channel(4096);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    d.execute(w.prompt(Some(session), text), tx).await.unwrap().unwrap();
}

/// R-OFF-2, R-SYNC-2: A's registry is gone for 12 seconds while a turn
/// ends, and then fails every write with a 503 while another does. The
/// chunks are put again, the same ones under the same indexes, once it is
/// back: the log reads whole on any device, and a second bridge takes the
/// session up.
#[tokio::test]
async fn r_off_2_a_registry_gone_or_failing_across_a_turn_end_loses_nothing() {
    let w = World::new("regcut");
    let a = w.device("machine-a");
    let (d, session) = first_turn(&w).await;
    let (stop, _cp, bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;

    w.reg_mode.store(CUT, Ordering::SeqCst);
    turn(&d, &w, &session, "a turn while the registry is gone").await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    w.reg_mode.store(PASS, Ordering::SeqCst);
    stored_whole(&w, &session, Duration::from_secs(15)).await;

    w.reg_mode.store(FAIL_WRITES, Ordering::SeqCst);
    turn(&d, &w, &session, "a turn while the registry fails").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    w.reg_mode.store(PASS, Ordering::SeqCst);
    stored_whole(&w, &session, Duration::from_secs(15)).await;

    turn(&d, &w, &session, "a turn after").await;
    stored_whole(&w, &session, Duration::from_secs(15)).await;
    stop.send(true).unwrap();
    bridge.await.unwrap().unwrap();
    // The next holder reads the whole chain and writes on from it.
    let (stop, _cp, mut bridge) = w.bridge(&a, &session, w.daemon().await);
    tokio::select! {
        () = w.synced(&session) => {}
        r = &mut bridge => panic!("the second bridge ended before it synced: {r:?}"),
    }
    stop.send(true).unwrap();
    bridge.await.unwrap().expect("a second bridge takes the session up");
}

/// R-OFF-2: the registry gone longer than the lease's TTL, across a turn
/// end: the lease lapses, the bridge takes it again under a new fence when
/// the registry is back, and the chunk it could not write goes in then.
#[tokio::test]
async fn r_off_2_a_lease_lapsed_while_the_registry_was_gone_is_taken_again_and_nothing_is_lost() {
    let w = World::new("lapse");
    let a = w.device("machine-a");
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge_with(&a, &session, w.daemon().await, 10, host::KEEP);
    w.synced(&session).await;
    w.reg_mode.store(CUT, Ordering::SeqCst);
    turn(&d, &w, &session, "a turn while the lease lapses").await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    w.reg_mode.store(PASS, Ordering::SeqCst);
    stored_whole(&w, &session, Duration::from_secs(20)).await;
    turn(&d, &w, &session, "a turn under the new lease").await;
    stored_whole(&w, &session, Duration::from_secs(15)).await;
}

/// R-SYNC-2: while A's lease lapsed, another device took it. A's bridge
/// writes nothing more and ends, saying the lease moved.
#[tokio::test]
async fn r_sync_2_a_bridge_whose_lease_another_device_took_stops() {
    let w = World::new("lost");
    let (a, c) = (w.device("machine-a"), w.device("machine-c"));
    let (_d, session) = first_turn(&w).await;
    let (_stop, _cp, bridge) = w.bridge_with(&a, &session, w.daemon().await, 10, host::KEEP);
    w.synced(&session).await;
    w.reg_mode.store(CUT, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(11)).await;
    let (api, id, dev) = (w.as_device(&c), session.clone(), c.key.id().to_string());
    tokio::task::spawn_blocking(move || api.acquire_lease(&id, &dev, 60, "development")).await.unwrap().expect("C takes the lapsed lease");
    w.reg_mode.store(PASS, Ordering::SeqCst);
    let ended = tokio::time::timeout(Duration::from_secs(20), bridge).await.expect("the bridge stops").unwrap();
    assert!(ended.as_ref().is_err_and(|e| e.contains("lease")), "it says the lease moved: {ended:?}");
}

/// R-LAG-7, R-LAG-4 over the relay: a session of well over 16 batches —
/// the relay's window — reaches B whole, because B acknowledges what it
/// applies.
#[tokio::test]
async fn r_lag_7_a_viewer_acks_so_a_long_answer_reaches_it_whole() {
    let w = World::new("long");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    let before = w.b_batches.load(Ordering::SeqCst);
    let dd = d.clone();
    let prompt = w.prompt(Some(&session), "a long answer, please");
    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel(4096);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let _ = dd.execute(prompt, tx).await;
    });
    let got = until(&mut v, Duration::from_secs(30), &mut frames, result_of).await;
    let batches = w.b_batches.load(Ordering::SeqCst) - before;
    assert!(batches > 100, "only {batches} batches");
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ItemDelta { .. }))) ), "the typing arrived");
    let text: String = got.iter().filter_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ItemDelta { delta, .. })) = u && let krowk_harness::protocol::Delta::Text { text } = delta { Some(text.clone()) } else { None }).collect();
    assert!(text.contains("word0 ") && text.contains("word159 "), "the whole answer: {} chars", text.len());
}

/// R-LAG-8: a prompt whose ack was lost — B's downlink dropped, then cut —
/// is sent again after the reconnect, and A runs it once.
#[tokio::test]
async fn r_lag_8_a_prompt_sent_again_after_a_lost_ack_runs_once() {
    let w = World::new("dup");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    w.b_mode.store(DROP_ALL, Ordering::SeqCst);
    v.commands.send(w.prompt(Some(&session), "run-once-marker-77")).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    w.b_mode.store(CUT, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    w.b_mode.store(PASS, Ordering::SeqCst);
    until(&mut v, Duration::from_secs(15), &mut frames, |u| matches!(u, viewer::Update::Acked { .. })).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&session).join("events.jsonl")).unwrap();
    let n = log.lines().filter(|l| l.contains("run-once-marker-77") && l.contains("userText")).count();
    assert_eq!(n, 1, "the prompt ran {n} times");
}

/// R-OFF-2: the relay drops three of B's stream batches mid-turn — fewer
/// than its window, so what follows still flows. B sees the gap, asks A,
/// and ends up with every event A's log holds.
#[tokio::test]
async fn r_off_2_batches_the_relay_dropped_show_as_a_gap_and_are_caught_up() {
    let w = World::new("drop");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let mut ids: Vec<String> = until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await.iter().flat_map(logged).collect();
    w.b_drop.store(3, Ordering::SeqCst);
    w.b_mode.store(DROP_BATCHES, Ordering::SeqCst);
    turn(&d, &w, &session, "a turn the relay drops some of").await;
    let mut gap = false;
    let want = log_ids(&w, &session);
    let deadline = Instant::now() + Duration::from_secs(15);
    while want.iter().any(|i| !ids.contains(i)) && Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(500), v.updates.recv()).await {
            gap |= batch.iter().any(|u| matches!(u, viewer::Update::Gap));
            ids.extend(batch.iter().flat_map(logged));
        }
    }
    assert!(gap, "the gap showed");
    let missing: Vec<_> = want.iter().filter(|i| !ids.contains(i)).collect();
    assert!(missing.is_empty(), "B never got {missing:?}");
}

/// R-OFF-2: A's link to the relay is down past what the bridge keeps, so
/// it cannot carry its stream on and starts another; B, told to resync,
/// is welcomed onto the new stream and caught up by A, losing nothing.
#[tokio::test]
async fn r_off_2_a_new_stream_after_a_long_cut_loses_nothing() {
    let w = World::new("newstream");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge_with(&a, &session, w.daemon().await, host::LEASE_TTL, 4);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let mut ids: Vec<String> = until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await.iter().flat_map(logged).collect();
    // Some batches on the first stream, so the relay holds a count.
    turn(&d, &w, &session, "a turn on the first stream").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    w.cut.store(true, Ordering::SeqCst);
    turn(&d, &w, &session, "a long answer while A is cut off").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    w.cut.store(false, Ordering::SeqCst);
    let want = log_ids(&w, &session);
    let deadline = Instant::now() + Duration::from_secs(20);
    while want.iter().any(|i| !ids.contains(i)) && Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(500), v.updates.recv()).await {
            ids.extend(batch.iter().flat_map(logged));
        }
    }
    let missing: Vec<_> = want.iter().filter(|i| !ids.contains(i)).collect();
    assert!(missing.is_empty(), "B never got {missing:?}");
}

/// A stand-in `tailscaled`: its LocalAPI on a unix socket, answering
/// `status` with this machine on 127.0.0.1 as tailnet user 1 and a peer
/// tagged `tag:krowk-host`, and `whois` with whichever user `whois` holds.
struct FakeTailscale {
    socket: PathBuf,
    whois: Arc<std::sync::atomic::AtomicU64>,
}

impl FakeTailscale {
    fn start(root: &Path) -> FakeTailscale {
        let socket = root.join("ts.sock");
        let l = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let whois = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let w = whois.clone();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&raw).to_string();
                let body = if req.starts_with("GET /localapi/v0/status") {
                    serde_json::json!({"BackendState": "Running", "Self": {"HostName": "a", "DNSName": "localhost.", "TailscaleIPs": ["127.0.0.1"], "Addrs": [], "UserID": 1, "Online": true},
                        "Peer": {"k": {"HostName": "b", "DNSName": "b.tail.ts.net.", "TailscaleIPs": ["100.64.0.2"], "Online": true, "Tags": ["tag:krowk-host"]}}}).to_string()
                } else if req.starts_with("GET /localapi/v0/whois?addr=127.0.0.1:") {
                    serde_json::json!({"Node": {"User": w.load(Ordering::SeqCst)}, "UserProfile": {"ID": w.load(Ordering::SeqCst), "LoginName": "someone@example.com"}}).to_string()
                } else {
                    let _ = c.write_all(b"HTTP/1.0 404 Not Found\r\n\r\nno such page");
                    continue;
                };
                let _ = write!(c, "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}");
            }
        });
        FakeTailscale { socket, whois }
    }
}

fn direct_path(u: &viewer::Update) -> bool {
    matches!(u, viewer::Update::Path { path, .. } if path.starts_with("direct"))
}

fn relay_path(u: &viewer::Update) -> bool {
    matches!(u, viewer::Update::Path { path, .. } if path == "relay")
}

/// Round trips of a command to A and its ack, the median.
async fn ack_rtt(v: &mut viewer::Viewer, session: &str, frames: &mut Vec<Instant>) -> Duration {
    let mut rtts = Vec::new();
    for _ in 0..15 {
        let t = Instant::now();
        v.commands.send(Command::Interrupt { session_id: session.into() }).unwrap();
        until(v, Duration::from_secs(5), frames, |u| matches!(u, viewer::Update::Acked { .. })).await;
        rtts.push(t.elapsed());
    }
    rtts.sort();
    rtts[rtts.len() / 2]
}

/// R-NET-1, R-NET-2: A reads its tailnet address from tailscaled, listens
/// there and names it to B inside the welcome, sealed — the relay never
/// sees the address — and B moves onto it, after which B's live frames no
/// longer cross the relay. `krowk hosts`' reading of the peers (R-NET-4)
/// rides the same fake LocalAPI.
#[tokio::test]
async fn r_net_2_the_session_moves_to_the_direct_path_and_live_frames_leave_the_relay() {
    let w = World::new("direct");
    let ts = FakeTailscale::start(&w.root);
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_direct(&a, &session, w.daemon().await, &ts, true, kill_rx);
    w.synced(&session).await;

    let status = krowk_harness::sync::tailscale::status(&ts.socket).unwrap();
    assert_eq!(status.hosts().iter().map(|h| h.host_name.as_str()).collect::<Vec<_>>(), ["b"], "R-NET-4");

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let got = until(&mut v, Duration::from_secs(15), &mut frames, direct_path).await;
    let via = got.iter().find_map(|u| if let viewer::Update::Path { via: Some(via), .. } = u { Some(via.clone()) } else { None }).unwrap();
    assert!(via.starts_with("ws://127.0.0.1:") || via.starts_with("ws://localhost:"), "{via}");
    assert!(got.iter().any(relay_path), "B came by the relay first: {got:?}");

    // A turn, live, with B on the direct path: not a byte of it to B by the relay.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = w.b_batches.load(Ordering::SeqCst);
    v.commands.send(w.prompt(Some(&session), "over the direct path")).unwrap();
    let got = until(&mut v, Duration::from_secs(15), &mut frames, result_of).await;
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Line(_))), "the turn streamed to B");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.b_batches.load(Ordering::SeqCst), before, "live frames still went to B through the relay");

    // R-NET-1: the candidates went sealed.
    let port = via.rsplit(':').next().unwrap().to_string();
    let cap = w.captured();
    let needle = format!("127.0.0.1:{port}");
    assert!(!cap.windows(needle.len()).any(|x| x == needle.as_bytes()), "the direct address crossed the relay in the clear");
}

/// R-NET-2: the direct listener killed mid-turn, B falls back to the relay
/// by itself, and holds every event A's log holds, none twice. Records the
/// command round trip over each path.
#[tokio::test]
async fn r_net_2_killing_the_direct_listener_falls_back_to_the_relay_with_no_gap() {
    let w = World::new("fallback");
    let ts = FakeTailscale::start(&w.root);
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_direct(&a, &session, w.daemon().await, &ts, false, kill_rx);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for u in until(&mut v, Duration::from_secs(15), &mut frames, direct_path).await {
        ids.extend(logged(&u));
    }
    let direct_rtt = ack_rtt(&mut v, &session, &mut frames).await;

    v.commands.send(w.prompt(Some(&session), "the direct path goes mid-turn")).unwrap();
    let mut got = until(&mut v, Duration::from_secs(15), &mut frames, |u| matches!(u, viewer::Update::Line(_))).await;
    kill.send(true).unwrap();
    got.extend(until(&mut v, Duration::from_secs(15), &mut frames, relay_path).await);
    // The turn may have ended before the relay's welcome, or in its frame.
    if !got.iter().any(result_of) {
        got.extend(until(&mut v, Duration::from_secs(20), &mut frames, result_of).await);
    }
    ids.extend(got.iter().flat_map(logged));
    let relay_rtt = ack_rtt(&mut v, &session, &mut frames).await;
    println!("R-NET-2 latency (command round trip, median of 15, loopback): direct {direct_rtt:?}, relay {relay_rtt:?}");

    let want = log_ids(&w, &session);
    let deadline = Instant::now() + Duration::from_secs(10);
    while want.iter().any(|i| !ids.contains(i)) && Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(300), v.updates.recv()).await {
            ids.extend(batch.iter().flat_map(logged));
        }
    }
    let missing: Vec<_> = want.iter().filter(|i| !ids.contains(i)).collect();
    assert!(missing.is_empty(), "B never got {missing:?}");
    let mut dedup = ids.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), ids.len(), "no event arrived twice");
}

/// R-NET-3: with the same-user check on, tailscaled naming the far end as
/// another tailnet user turns the direct connection away, and B stays on
/// the relay, where its ticket and keys still let it in.
#[tokio::test]
async fn r_net_3_with_whois_on_another_tailnet_user_is_refused_and_stays_on_the_relay() {
    let w = World::new("whois");
    let ts = FakeTailscale::start(&w.root);
    ts.whois.store(2, Ordering::SeqCst);
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_direct(&a, &session, w.daemon().await, &ts, true, kill_rx);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(15), &mut frames, relay_path).await;
    let deadline = Instant::now() + viewer::PROBE + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(200), v.updates.recv()).await {
            assert!(!batch.iter().any(direct_path), "another tailnet user's connection was let in: {batch:?}");
        }
    }
    v.commands.send(w.prompt(Some(&session), "still by the relay")).unwrap();
    until(&mut v, Duration::from_secs(15), &mut frames, result_of).await;
}
