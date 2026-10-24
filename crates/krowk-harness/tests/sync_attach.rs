//! Same session, two devices (Harness P1 ticket 19): a session running in
//! machine A's host daemon, synced by A's bridge through the stand-in
//! registry and the reference relay, attached from machine B — another
//! device, with keys of its own and the same user key — which watches
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

use krowk_client::e2e::{self, DeviceKey, SessionKey, SigningKey};
use krowk_client::device_chain::{Batch, Chain, Change, Kind, Subject};
use krowk_client::session_record;
use krowk_client::user_key::{UserKey, UserKeys};

/// When the test device list is made, by its devices' clocks.
const T0: u64 = 1_790_000_000;
/// The person's key, which each device holds one of (`tok#name`).
const TOKEN: &str = "krowk_sk_sync_attach_0000000000000000";
use krowk_harness::daemon::{self, client::Client, server};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{ApprovalDecision, ApprovalRequest, Command, Item, LiveEvent, LogBody, PermissionMode, StreamLine};
use krowk_harness::sync::{auto, direct, host, viewer};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
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
    // A subagent's own call: a slow answer, stopped part way.
    if body["system"].to_string().contains("You are a subagent") {
        let words: String = (0..400).map(|i| format!("childword{i} ")).collect();
        return mock::Reply::paced(mock::text_stream(&words), Duration::from_millis(20));
    }
    if !has_result && last.to_string().contains("start a slow subagent") {
        return mock::Reply::sse(&mock::tool_use("toolu_01Kid", "subagent", &serde_json::json!({"description": "a slow look", "prompt": "look slowly"})));
    }
    // Fifteen seconds of typing: long enough for fifty sessions, started
    // together on a loaded machine, to stream at once.
    if !has_result && last.to_string().contains("a very long answer") {
        let words: String = (0..750).map(|i| format!("word{i} ")).collect();
        return mock::Reply::paced(mock::text_stream(&words), Duration::from_millis(20));
    }
    // Eight seconds of typing: well over a hundred of the host's batches,
    // one a frame, even where the frame's timer wakes late (40 ms, not 16,
    // on a macOS runner).
    if !has_result && last.to_string().contains("an answer of many batches") {
        let words: String = (0..400).map(|i| format!("word{i} ")).collect();
        return mock::Reply::paced(mock::text_stream(&words), Duration::from_millis(20));
    }
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
    /// The person's device list, with each device the test makes on it,
    /// and the user key it leaves current, which every device holds.
    list: Mutex<Option<(Chain, UserKey)>>,
    /// The first device, which adds the others.
    first: Mutex<Option<Device>>,
    /// Every byte either device sent the relay or got from it.
    seen: Arc<Mutex<Vec<u8>>>,
    /// While set, A's link to the relay is down and nothing gets through.
    cut: Arc<AtomicBool>,
    /// What A's proxy does meanwhile: `PASS`, `FREEZE` (nothing forwarded
    /// either way, every connection kept open: A stopped, as by SIGSTOP), or
    /// `GHOST` (the relay's refusal said on A's open connection, which is
    /// then frozen alone).
    a_mode: Arc<AtomicU8>,
    /// Connections A has opened to the relay: each a join.
    a_conns: Arc<AtomicUsize>,
    /// A's connections to its relay open now.
    a_live: Arc<AtomicUsize>,
    relay_a: String,
    relay_b: String,
    _mock: mock::Mock,
    mock_url: String,
    /// A's own way to the registry: `PASS`, `CUT` or `FAIL_WRITES`.
    reg_a: String,
    reg_mode: Arc<AtomicU8>,
    /// Every request A's registry proxy handed on: its request line, and
    /// the lease fence its answer named, if any.
    reg_log: RegLog,
    /// The same requests, each with when it was handed on and the status
    /// its answer gave.
    reg_times: RegTimes,
    /// The worst lateness of A's daemon's thread, in µs, since last zeroed
    /// (`server::Options::lateness`).
    late: Arc<AtomicU64>,
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
const FREEZE: u8 = 4;
const GHOST: u8 = 5;
/// A registry proxy's mode: the second relay ticket asked for — a viewer's
/// race for a direct path, which starts as the relay's welcome comes —
/// waits until the mode changes, so the race goes on when the test says.
/// Every other ticket passes: a viewer whose relay connection drops meanwhile
/// joins again as it would, rather than hanging on a ticket.
const HOLD_TICKETS: u8 = 3;
/// A registry proxy's mode: a notification asked for is held, unanswered,
/// until the mode changes — a registry that stalls on it — and every other
/// request passes.
const HOLD_NOTIFY: u8 = 6;
/// A registry proxy's mode: a notification asked for is refused 503
/// `push_unconfigured` — a registry without its APNs key — and every other
/// request passes.
const REFUSE_NOTIFY: u8 = 7;

struct Device {
    key: DeviceKey,
    signing: SigningKey,
    /// The key it holds, which speaks for it.
    token: String,
}

impl Device {
    fn new(name: &str) -> Device {
        Device { key: DeviceKey::generate(), signing: SigningKey::generate(), token: format!("{TOKEN}#{name}") }
    }

    fn copy(&self) -> Device {
        Device { key: DeviceKey::from_secret(&*self.key.secret_bytes()).unwrap(), signing: SigningKey::from_secret(&*self.signing.secret_bytes()).unwrap(), token: self.token.clone() }
    }
}

fn signer(d: &Device) -> Arc<dyn krowk_api::client::RequestSigner> {
    e2e::DeviceSigner::new(d.key.id(), SigningKey::from_secret(&*d.signing.secret_bytes()).unwrap()).shared()
}

/// A device list post of `b`, as the chain made it.
fn post(b: &Batch) -> krowk_api::sync::ListPost {
    krowk_api::sync::ListPost {
        entries: b.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: b.links.iter().map(|l| e2e::hex(l)).collect(),
        wraps: b.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over: false,
    }
}

impl World {
    fn new(name: &str) -> World {
        World::with_relay(name, Default::default())
    }

    /// A world whose reference relay runs under `limits`.
    fn with_relay(name: &str, limits: krowk_harness::relay::Limits) -> World {
        let root = scratch::root(&format!("sync-{name}"));
        for d in ["home", "run", "repo/.git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        let api = format!("{}/v1", registry.url());
        let relay_addr = relay(limits);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cut = Arc::new(AtomicBool::new(false));
        let (a_mode, a_conns) = (Arc::new(AtomicU8::new(PASS)), Arc::new(AtomicUsize::new(0)));
        let a_live = Arc::new(AtomicUsize::new(0));
        let relay_a = proxy(relay_addr, seen.clone(), cut.clone(), a_mode.clone(), a_conns.clone(), a_live.clone());
        let (b_mode, b_batches) = (Arc::new(AtomicU8::new(PASS)), Arc::new(AtomicUsize::new(0)));
        let b_drop = Arc::new(AtomicUsize::new(0));
        let relay_b = viewer_proxy(relay_addr, seen.clone(), b_mode.clone(), b_batches.clone(), b_drop.clone());
        let reg_mode = Arc::new(AtomicU8::new(PASS));
        let reg_log: RegLog = Arc::default();
        let reg_times: RegTimes = Arc::default();
        let reg_a = format!("http://{}/v1", registry_proxy(registry.addr(), reg_mode.clone(), reg_log.clone(), reg_times.clone()));
        let m = mock::serve(model);
        let mock_url = m.url.clone();
        World { root, registry, api, list: Mutex::new(None), first: Mutex::new(None), seen, cut, a_mode, a_conns, a_live, relay_a: format!("ws://{relay_a}"), relay_b: format!("ws://{relay_b}"), _mock: m, mock_url, reg_a, reg_mode, reg_log, reg_times, late: Arc::default(), b_mode, b_batches, b_drop }
    }

    /// A device's own registry client: on its own key, its calls that act
    /// as the device signed by its key, as the registry requires.
    fn as_device(&self, d: &Device) -> Arc<krowk_api::Client> {
        Arc::new(krowk_api::Client::new(&self.api, &d.token).signed_by(signer(d)))
    }

    /// A's registry client, through A's own proxy, signed by A.
    fn a_client(&self, a: &Device) -> Arc<krowk_api::Client> {
        let mut c = krowk_api::Client::new(&self.reg_a, &a.token).signed_by(signer(a));
        c.sleep = |_| std::thread::sleep(Duration::from_millis(50));
        Arc::new(c)
    }

    /// The user keys every device holds: the list's generation 1.
    fn keys(&self) -> UserKeys {
        UserKeys::new(self.list.lock().unwrap().as_ref().expect("a device first").1.clone(), []).unwrap()
    }

    /// The user key every device holds.
    fn user_key(&self) -> UserKey {
        self.list.lock().unwrap().as_ref().expect("a device first").1.clone()
    }

    /// The device list, verified, with every device made so far on it.
    fn chain(&self) -> Chain {
        self.list.lock().unwrap().as_ref().expect("a device first").0.clone()
    }

    /// `d` on the person's device list, here and in the registry: the first
    /// device starts it, which binds its key to it, and each later one is
    /// added by the first and then claims its own key, as `sync join` does.
    fn enlist(&self, d: &Device, name: &str) {
        let subject = Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: d.key.public(), signing: d.signing.public() };
        let mut list = self.list.lock().unwrap();
        let mut first = self.first.lock().unwrap();
        let next = match (list.take(), first.as_ref()) {
            (Some((chain, user)), Some(f)) => {
                let (chain, batch) = chain.batch(&user, vec![Change::Add(subject)], f.key.id(), &f.signing, T0 + 1).unwrap();
                self.as_device(f).append_device_list(&post(&batch)).unwrap();
                self.as_device(d).claim_key_device().unwrap();
                (chain, user)
            }
            _ => {
                let (chain, start) = Chain::start(subject, &d.signing, None, T0).unwrap();
                self.as_device(d).init_device_list(&post(&start)).unwrap();
                *first = Some(d.copy());
                (chain, start.newest)
            }
        };
        *list = Some(next);
    }

    /// The registry as the first device reads it: session reads are signed.
    fn client(&self) -> Arc<krowk_api::Client> {
        self.as_device(self.first.lock().unwrap().as_ref().expect("a device first"))
    }

    /// A device of the person's, on their list with a key of its own.
    fn device(&self, name: &str) -> Device {
        let d = Device::new(name);
        self.enlist(&d, name);
        d
    }

    fn env(&self) -> impl Fn(&str) -> String + Clone + Send + Sync + 'static {
        self.env_at("home", "run")
    }

    /// Another machine's environment: a home and a runtime directory of its
    /// own, so a daemon of its own.
    fn env_at(&self, home: &str, run: &str) -> impl Fn(&str) -> String + Clone + Send + Sync + 'static {
        let (root, url, home, run) = (self.root.clone(), self.mock_url.clone(), home.to_string(), run.to_string());
        let _ = std::fs::create_dir_all(root.join(&home));
        let _ = std::fs::create_dir_all(root.join(&run));
        move |k| match k {
            "HOME" => root.join(&home).display().to_string(),
            "XDG_RUNTIME_DIR" => root.join(&run).display().to_string(),
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
        self.serve_env(self.env(), "home");
    }

    /// The host daemon of the machine `env` describes, its home `home`.
    fn serve_env(&self, env: impl Fn(&str) -> String + Clone + Send + Sync + 'static, home: &str) {
        self.serve_with(env, home, InstancesConfig::default());
    }

    /// `serve_env`, the daemon's instances `instances`.
    fn serve_with(&self, env: impl Fn(&str) -> String + Clone + Send + Sync + 'static, home: &str, instances: InstancesConfig) {
        let _ = self.serve_hooked(env, home, instances, None, None);
    }

    /// `serve_with`, telling `turns` of each session's own turn starting and
    /// ending, and reporting `bridged` in its status: the hooks the
    /// automatic sync's supervisor rides (ticket 92). Its thread is named
    /// `DAEMON_THREAD`.
    /// `server::run`'s thread, which ends as the daemon does.
    fn serve_hooked(&self, env: impl Fn(&str) -> String + Clone + Send + Sync + 'static, home: &str, instances: InstancesConfig, turns: Option<mpsc::UnboundedSender<server::TurnEvent>>, bridged: Option<Arc<Mutex<BTreeSet<String>>>>) -> std::thread::JoinHandle<()> {
        let socket = daemon::socket(&env).unwrap();
        let late = self.late.clone();
        let credentials = self.root.join(home).join(".krowk/credentials.json");
        let wait = env.clone();
        let thread = std::thread::Builder::new().name(DAEMON_THREAD.into()).spawn(move || {
            let factory: server::Factory = Box::new(move |cwd: &Path, answers: bool| {
                Ok(HostConfig {
                    sessions_dir: log::sessions_dir(&env).unwrap(),
                    cwd: cwd.to_path_buf(),
                    registry: Registry::resolve(&instances, &env),
                    krowk_version: "test".into(),
                    pricer: Arc::new(|_, _, _| None),
                    catalog: Arc::new(|_, _| None),
                    credentials: credentials.clone(),
                    trust: krowk_harness::trust::allow_all(),
                    publisher: None,
                    permissions: krowk_harness::permissions::Config { approvals: answers, ..Default::default() },
                    agents: krowk_harness::subagent::AgentsConfig::none(),
                    session: Default::default(),
                })
            });
            let _ = server::run(server::Options { socket, idle: None, krowk_version: "test".into(), turns, bridged, lateness: Some(late), ..Default::default() }, factory);
        }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(daemon::socket(&wait).unwrap()).is_err() {
            assert!(Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(5));
        }
        thread
    }

    /// The worst A's daemon's thread was late since the last call, and
    /// zeroed for the next.
    fn lateness(&self) -> Duration {
        Duration::from_micros(self.late.swap(0, Ordering::Relaxed))
    }

    async fn daemon(&self) -> Arc<Client> {
        Arc::new(Client::connect(&daemon::socket(&self.env()).unwrap(), &self.repo(), "test", true).await.ok().expect("the daemon answers"))
    }

    fn prompt(&self, session: Option<&str>, text: &str) -> Command {
        let model = Registry::resolve(&InstancesConfig::default(), &self.env()).parse_model("claude-sonnet-4-6").unwrap();
        Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }
    }

    /// A's bridge for `session`, running until the handle is stopped.
    fn bridge(&self, a: &Device, session: &str, daemon: Arc<Client>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        self.bridge_with(a, session, daemon, host::LEASE_TTL, host::KEEP, host::APPROVAL_WAIT)
    }

    fn bridge_with(&self, a: &Device, session: &str, daemon: Arc<Client>, ttl: u64, keep: usize, approval_wait: Duration) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let o = host::Options {
            relay: self.relay_a.clone(),
            env: "development".into(),
            api: self.a_client(a),
            device: a.key.id(),
            signing: SigningKey::from_secret(&*a.signing.secret_bytes()).unwrap(),
            keys: self.keys(),
            chain: self.chain(),
            session: session.into(),
            title: "the title is sealed too".into(),
            cwd: self.repo().display().to_string(),
            sessions: log::sessions_dir(&self.env()).unwrap(),
            ttl,
            keep,
            direct: None,
            tailnet: None,
            handed: None,
            approval_wait,
            stale: None,
            supervised: None,
        };
        self.run_bridge(o, daemon)
    }

    /// A's bridge with a direct listener, reading this node from a fake
    /// tailscaled at `ts`; `stop` kills the listener.
    fn bridge_direct(&self, a: &Device, session: &str, daemon: Arc<Client>, ts: &FakeTailscale, same_user: bool, stop: watch::Receiver<bool>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let config = direct::Config { socket: ts.socket.clone().into(), roster: ticket_roster(), same_user, lan: false, lan_at: None, stop: Some(stop) };
        let tailnet = krowk_harness::sync::tailscale::status(&ts.socket.clone().into()).ok().and_then(|s| s.tailnet());
        self.bridge_with_direct(a, session, daemon, config, tailnet)
    }

    /// A's bridge offering the LAN at 127.0.0.1 with no Tailscale on the
    /// machine (ticket 27): nothing answers at tailscaled's socket.
    fn bridge_lan(&self, a: &Device, session: &str, daemon: Arc<Client>, stop: watch::Receiver<bool>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        self.bridge_lan_at(a, session, daemon, stop, Arc::new(|| vec![std::net::Ipv4Addr::LOCALHOST.into()]))
    }

    /// `bridge_lan`, its LAN addresses read from `at` as they move.
    fn bridge_lan_at(&self, a: &Device, session: &str, daemon: Arc<Client>, stop: watch::Receiver<bool>, at: direct::LanAt) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let config = direct::Config { socket: self.root.join("no-tailscaled.sock").into(), roster: ticket_roster(), same_user: false, lan: true, lan_at: Some(at), stop: Some(stop) };
        self.bridge_with_direct(a, session, daemon, config, None)
    }

    fn bridge_with_direct(&self, a: &Device, session: &str, daemon: Arc<Client>, config: direct::Config, tailnet: Option<krowk_harness::sync::tailscale::Tailnet>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let o = host::Options {
            relay: self.relay_a.clone(),
            env: "development".into(),
            api: self.a_client(a),
            device: a.key.id(),
            signing: SigningKey::from_secret(&*a.signing.secret_bytes()).unwrap(),
            keys: self.keys(),
            chain: self.chain(),
            session: session.into(),
            title: "the title is sealed too".into(),
            cwd: self.repo().display().to_string(),
            sessions: log::sessions_dir(&self.env()).unwrap(),
            ttl: host::LEASE_TTL,
            keep: host::KEEP,
            direct: Some(config),
            tailnet,
            handed: None,
            approval_wait: host::APPROVAL_WAIT,
            stale: None,
            supervised: None,
        };
        self.run_bridge(o, daemon)
    }

    fn run_bridge(&self, o: host::Options, daemon: Arc<Client>) -> (watch::Sender<bool>, mpsc::UnboundedSender<()>, tokio::task::JoinHandle<Result<(), String>>) {
        let (stop, stop_rx) = watch::channel(false);
        let (cp, cp_rx) = mpsc::unbounded_channel();
        // A bridge that ends with an error says it on stderr, which a test
        // waiting on what the bridge should have done shows when it fails.
        let run = async move {
            let r = host::run(o, daemon, stop_rx, cp_rx).await;
            if let Err(e) = &r {
                eprintln!("the bridge ended: {e}");
            }
            r
        };
        (stop, cp, tokio::spawn(run))
    }

    fn viewer(&self, b: &Device, session: &str) -> viewer::Options {
        viewer::Options {
            relay: self.relay_b.clone(),
            env: "development".into(),
            api: self.as_device(b),
            device: b.key.id(),
            signing: SigningKey::from_secret(&*b.signing.secret_bytes()).unwrap(),
            keys: self.keys(),
            chain: self.chain(),
            session: session.into(),
            known: None,
        }
    }

    /// Waits until the session's sealed index names a checkpoint and a head:
    /// the bridge has taken the session up and written it.
    async fn synced(&self, session: &str) {
        let (api, id) = (self.client(), session.to_string());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (api, id, keys, chain) = (api.clone(), id.clone(), self.keys(), self.chain());
            // What the registry last said, for a failure to name.
            let ready = tokio::task::spawn_blocking(move || -> Result<bool, String> {
                let s = api.show_sync_session(&id).map_err(|e| e.to_string())?;
                let key = krowk_harness::sync::store::open_session_key(&s, &id, &keys, &chain, session_record::Signer::EverHeld)?;
                let index = krowk_harness::sync::store::open_index(&key, &id, &s.sealed_index)?;
                match (s.lease.is_some(), index.checkpoint.is_some(), index.head.is_some()) {
                    (true, true, true) => Ok(true),
                    (lease, checkpoint, head) => Err(format!("lease {lease}, checkpoint {checkpoint}, head {head}")),
                }
            })
            .await
            .unwrap();
            if ready == Ok(true) {
                return;
            }
            assert!(Instant::now() < deadline, "the bridge never synced the session: the registry's last word {ready:?}");
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

/// The relay the devices reach: the reference relay, in this process, or
/// another at `KROWK_SYNC_RELAY` (`127.0.0.1:8787`, the hosted relay under
/// `wrangler dev`, trusting the stand-in registry's ticket key), so the
/// same scenarios hold the Worker to what the bridge and the viewer need.
fn relay(limits: krowk_harness::relay::Limits) -> SocketAddr {
    if let Ok(at) = std::env::var("KROWK_SYNC_RELAY") {
        return at.parse().expect("KROWK_SYNC_RELAY is an address, 127.0.0.1:8787");
    }
    let relay = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = relay.local_addr().unwrap();
    let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
    let roster = krowk_harness::relay::Roster::parse(&roster).unwrap();
    std::thread::spawn(move || krowk_harness::relay::run(relay, krowk_harness::relay::Config { roster, origin: None, limits, state: None, origins: Vec::new(), whois: None, pin: None }));
    addr
}

/// A TCP proxy in front of the relay: records both ways, and while `cut`
/// is set drops every connection and refuses new ones — a network gone.
/// Under `FREEZE` it forwards nothing and keeps every connection open;
/// under `GHOST` it tells A on the connection open now, as the relay
/// would, that its link was let go — and then forwards nothing more on it,
/// the close never coming, while a new connection passes.
fn proxy(to: SocketAddr, seen: Arc<Mutex<Vec<u8>>>, cut: Arc<AtomicBool>, mode: Arc<AtomicU8>, conns: Arc<AtomicUsize>, live: Arc<AtomicUsize>) -> SocketAddr {
    // Counts a connection gone as the client's side of it ends.
    struct Live(Arc<AtomicUsize>);
    impl Drop for Live {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            if cut.load(Ordering::SeqCst) {
                drop(c);
                continue;
            }
            let Ok(up) = TcpStream::connect(to) else { continue };
            conns.fetch_add(1, Ordering::SeqCst);
            live.fetch_add(1, Ordering::SeqCst);
            let mut gone = Some(Live(live.clone()));
            // This connection alone, once a ghost: what joins after it passes.
            let ghost = Arc::new(AtomicBool::new(false));
            for (down, (mut from, mut into)) in [(false, (c.try_clone().unwrap(), up.try_clone().unwrap())), (true, (up, c))] {
                let (seen, cut, mode, ghost) = (seen.clone(), cut.clone(), mode.clone(), ghost.clone());
                let _gone = if down { None } else { gone.take() };
                std::thread::spawn(move || {
                    let _gone = _gone;
                    from.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                    let mut buf = [0u8; 16384];
                    loop {
                        if cut.load(Ordering::SeqCst) {
                            let _ = from.shutdown(std::net::Shutdown::Both);
                            let _ = into.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                        match mode.load(Ordering::SeqCst) {
                            _ if ghost.load(Ordering::SeqCst) => {
                                std::thread::sleep(Duration::from_millis(20));
                                continue;
                            }
                            FREEZE => {
                                std::thread::sleep(Duration::from_millis(20));
                                continue;
                            }
                            GHOST if down => {
                                // The relay's refusal, as a WebSocket server
                                // frame: binary, unmasked, its length in two
                                // bytes past 125.
                                let e = krowk_harness::relay::control(&serde_json::json!({"type": "error", "code": "replaced", "message": "another connection of the lease holder took the channel", "fix": "nothing to do if that was this device reconnecting"}));
                                let mut f = vec![0x82, 126];
                                f.extend_from_slice(&(e.len() as u16).to_be_bytes());
                                f.extend_from_slice(&e);
                                let _ = into.write_all(&f);
                                ghost.store(true, Ordering::SeqCst);
                                mode.store(PASS, Ordering::SeqCst);
                                continue;
                            }
                            _ => {}
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

/// What A's registry proxy handed on: each request's line, and the lease
/// fence its answer named.
type RegLog = Arc<Mutex<Vec<(String, Option<u64>)>>>;

/// What A's registry proxy handed on, timed: when, the request's line, and
/// its answer's status.
type RegTimes = Arc<Mutex<Vec<(Instant, String, u16)>>>;

/// The fence a registry answer's body names, if it names one.
fn fence_in(resp: &[u8]) -> Option<u64> {
    let body = &resp[resp.windows(4).position(|w| w == b"\r\n\r\n")? + 4..];
    let text = String::from_utf8_lossy(body);
    let at = text.find("\"fence\":")? + "\"fence\":".len();
    text[at..].trim_start().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// A registry proxy: `CUT` drops every connection, `FAIL_WRITES` answers
/// every write (a POST or PUT) 503 — a registry failing, not gone —
/// `HOLD_TICKETS` holds the second relay ticket asked for, `HOLD_NOTIFY`
/// holds each notification until the mode moves on, and `REFUSE_NOTIFY`
/// refuses each `push_unconfigured`. Each request it hands on is kept in
/// `log`.
fn registry_proxy(to: SocketAddr, mode: Arc<AtomicU8>, log: RegLog, timed: RegTimes) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let tickets = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            if mode.load(Ordering::SeqCst) == CUT {
                drop(c);
                continue;
            }
            let (mode, tickets, log, timed) = (mode.clone(), tickets.clone(), log.clone(), timed.clone());
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
                    let asked = String::from_utf8_lossy(&one[..one.windows(2).position(|w| w == b"\r\n").unwrap_or(0)]).to_string();
                    let notify = asked.starts_with("POST ") && asked.contains("/notification ");
                    // What becomes of a notification is settled before it is
                    // logged as handed on, by the mode it came in under (or,
                    // held, the one that let it go): a test that sees it
                    // logged and moves on does not change its fate.
                    let mut fate = m;
                    if notify && fate == HOLD_NOTIFY {
                        log.lock().unwrap().push((format!("HELD {asked}"), None));
                        while fate == HOLD_NOTIFY {
                            std::thread::sleep(Duration::from_millis(10));
                            fate = mode.load(Ordering::SeqCst);
                        }
                    }
                    // A notification is logged as it is handed on, not once
                    // answered: the stand-in records it before it answers.
                    if notify {
                        log.lock().unwrap().push((asked, None));
                    }
                    if notify && fate == REFUSE_NOTIFY {
                        // The registry's envelope: `push_unconfigured` is a 503
                        // the client never retries. A flat `"error"` would read
                        // as a bare 503, and be retried.
                        let body = r#"{"error":{"code":"push_unconfigured","message":"this registry cannot send pushes"}}"#;
                        let _ = write!(c, "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        return;
                    }
                    if m == FAIL_WRITES && (one.starts_with(b"POST ") || one.starts_with(b"PUT ")) {
                        let _ = c.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                        return;
                    }
                    let first = String::from_utf8_lossy(&one[..one.windows(2).position(|w| w == b"\r\n").unwrap_or(0)]).to_string();
                    if first.starts_with("GET ") && first.contains("/relay_ticket") && tickets.fetch_add(1, Ordering::SeqCst) == 1 {
                        while mode.load(Ordering::SeqCst) == HOLD_TICKETS {
                            std::thread::sleep(Duration::from_millis(10));
                        }
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
                    if !notify {
                        log.lock().unwrap().push((first.clone(), fence_in(&resp)));
                        let status = String::from_utf8_lossy(&resp[..resp.len().min(12)]).split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                        timed.lock().unwrap().push((Instant::now(), first.clone(), status));
                    }
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
/// D8b: a session record lists and attaches only when a device on the
/// verified device list signed it. One the registry holds unsigned, or
/// signed by a device the person never listed — a registry's own, sealed
/// under the person's key by a removed device, say — is left out of the
/// listing and refused on attach, before any key is unwrapped.
#[tokio::test]
async fn d8b_only_a_record_a_listed_device_signed_opens() {
    let w = World::new("records");
    let b = w.device("machine-b");
    let (keys, chain) = (w.keys(), w.chain());
    let publish = |id: &str, title: &str, signer: &Device, signed: bool| {
        let raw = krowk_harness::daemon::ws::uuid(id);
        let key = SessionKey::generate();
        let index = krowk_harness::sync::store::Index { title: title.into(), ..Default::default() };
        let sealed = e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).unwrap()));
        let wrapped = e2e::seal_session_key(&key, &raw, &keys, chain.generation()).unwrap();
        let api = w.as_device(&b);
        if signed {
            let sig = session_record::sign(&raw, &wrapped, session_record::SEAL_USER, keys.newest(), &signer.signing).unwrap();
            api.put_sync_session(id, &e2e::hex(&wrapped), Some((&e2e::hex(&sig), &signer.key.id().to_string())), Some(&sealed), None).unwrap();
        } else {
            api.put_sync_session(id, &e2e::hex(&wrapped), None, Some(&sealed), None).unwrap();
        }
    };
    let stranger = Device::new("stranger");
    publish("01a0ec7b-2222-7000-8000-0000000000d1", "signed by b", &b, true);
    publish("01a0ec7b-2222-7000-8000-0000000000d2", "unsigned", &b, false);
    publish("01a0ec7b-2222-7000-8000-0000000000d3", "signed by a stranger", &stranger, true);

    let api = w.client();
    let (listed, unreadable) = tokio::task::spawn_blocking(move || viewer::list(&api, &keys, &chain)).await.unwrap().unwrap();
    assert_eq!(listed.iter().map(|l| l.index.title.as_str()).collect::<Vec<_>>(), ["signed by b"]);
    assert_eq!(unreadable, 2);
    let refused = viewer::attach(w.viewer(&b, "01a0ec7b-2222-7000-8000-0000000000d2")).await.err().expect("an unsigned record does not attach");
    assert!(refused.contains("names no signer"), "{refused}");
    let refused = viewer::attach(w.viewer(&b, "01a0ec7b-2222-7000-8000-0000000000d3")).await.err().expect("a stranger's record does not attach");
    assert!(refused.contains("never held"), "{refused}");
}

#[tokio::test]
async fn r_sync_2_a_prompt_typed_on_b_runs_on_a_and_b_answers_its_approval() {
    let w = World::new("prompt");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;

    let (api, keys, chain) = (w.client(), w.keys(), w.chain());
    let (listed, unreadable) = tokio::task::spawn_blocking(move || viewer::list(&api, &keys, &chain)).await.unwrap().unwrap();
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

    // Nor write a rule into A's project (D11): a viewer allows a call once
    // or for the session, and A refuses the rest before looking it up.
    v.commands.send(Command::Approve { session_id: session.clone(), request_id: "any".into(), decision: ApprovalDecision::AllowProject, answers: Vec::new() }).unwrap();
    let got = until(&mut v, Duration::from_secs(5), &mut frames, |u| matches!(u, viewer::Update::Acked { .. })).await;
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Acked { error: Some(e), .. } if e.contains("made on the host"))), "{got:?}");

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
    v.commands.send(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() }).unwrap();
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

/// An answering client — the terminal — prompts "make the file" in
/// `session`, with A's bridge on it denying late after a second; returns
/// the client, its turn (running on the runtime) and the request it saw.
async fn ask_at_the_terminal(w: &World, session: &str) -> (Arc<Client>, tokio::task::JoinHandle<()>, ApprovalRequest) {
    let t = w.daemon().await;
    let (tx, mut rx) = mpsc::channel(1024);
    let (run, cmd) = (t.clone(), w.prompt(Some(session), "make the file"));
    let turn = tokio::spawn(async move {
        let _ = run.execute(cmd, tx).await;
    });
    let req = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(StreamLine::Live(LiveEvent::ApprovalRequested(r))) = rx.recv().await {
                break r;
            }
        }
    })
    .await
    .expect("the terminal is asked");
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    (t, turn, req)
}

/// The tool results logged in A's log of `session`.
fn tool_results(w: &World, session: &str) -> Vec<(String, bool)> {
    let events = log::read_events(&log::sessions_dir(&w.env()).unwrap().join(session).join(log::EVENTS_FILE)).unwrap();
    events.into_iter().filter_map(|e| match e.body {
        LogBody::ItemCompleted { item: Item::ToolResult { output, is_error, .. }, .. } => Some((output, is_error)),
        _ => None,
    }).collect()
}

/// R-HOST-1, R-SYNC-3: the terminal that started a turn closes with its
/// question on screen, and A's bridge — an answering client too, so the
/// daemon's own denial never fires — is the session's one answering client
/// left. With no viewer there either, the bridge denies the request once
/// it has waited, rather than leaving the turn waiting on nobody.
#[tokio::test]
async fn r_host_1_a_terminal_that_leaves_with_a_question_open_is_not_kept_waiting_by_the_bridge() {
    let w = World::new("unattended");
    let a = w.device("machine-a");
    let (d, session) = first_turn(&w).await;
    let wait = Duration::from_secs(1);
    let (_stop, _cp, _bridge) = w.bridge_with(&a, &session, w.daemon().await, host::LEASE_TTL, host::KEEP, wait);
    w.synced(&session).await;
    let (t, turn, req) = ask_at_the_terminal(&w, &session).await;
    assert_eq!(req.tool, "bash");
    // The terminal goes, its turn's command with it.
    turn.abort();
    drop((t, d));
    let left = Instant::now();
    // The bridge sweeps once a second. It sees the request has waited out
    // `wait`, asks the daemon, and believes that nobody else answers only
    // once the daemon has said so for as long again (`deny_late`): two
    // waits and up to two sweeps, under four seconds on Linux and macOS
    // alike, held below. The denied turn then ends with the model's paced
    // answer; the deadline is only how long the test looks for that end.
    let deadline = left + Duration::from_secs(10);
    let mut denied = None;
    let results = loop {
        let events = log::read_events(&log::sessions_dir(&w.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
        if denied.is_none() && events.iter().any(|e| matches!(e.body, LogBody::ItemCompleted { item: Item::ToolResult { is_error: true, .. }, .. })) {
            denied = Some(left.elapsed());
        }
        if events.iter().filter(|e| matches!(e.body, LogBody::TurnCompleted { .. })).count() >= 2 {
            break tool_results(&w, &session);
        }
        assert!(Instant::now() < deadline, "the turn still waits on the question");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // The bridge's doing, after its wait: not the daemon's, which would
    // have denied it the moment the terminal went; and no later than its
    // two waits and two sweeps, with a second and a half to spare.
    let denied = denied.expect("the turn ended with its request denied");
    assert!(denied >= Duration::from_millis(500), "denied {denied:?} after the terminal went");
    assert!(denied <= wait * 2 + Duration::from_millis(3500), "denied {denied:?} after the terminal went, a sweep or more late");
    assert!(matches!(results.as_slice(), [(_, true)]), "bash was denied: {results:?}");
    assert!(!w.repo().join("approved.txt").exists());
}

/// R-PERM-2, R-HOST-1: while the terminal that started the turn still
/// follows the session, the bridge leaves its question to it — still
/// waiting well past the bridge's wait — and a viewer B attached can
/// still answer it.
#[tokio::test]
async fn r_perm_2_a_question_the_terminal_still_follows_waits_and_a_viewer_can_answer_it() {
    let w = World::new("attended");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge_with(&a, &session, w.daemon().await, host::LEASE_TTL, host::KEEP, Duration::from_secs(1));
    w.synced(&session).await;
    // The terminal asking is the one other answering client.
    drop(d);
    let (_t, turn, req) = ask_at_the_terminal(&w, &session).await;
    // A status that failed would leave it waiting too: r_host_1's test
    // above is the one that needs it to work and count the bridge alone.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!turn.is_finished() && tool_results(&w, &session).is_empty(), "the question still waits on the terminal");

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    v.commands.send(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() }).unwrap();
    tokio::time::timeout(Duration::from_secs(15), turn).await.expect("B's answer ends the turn").unwrap();
    assert!(matches!(tool_results(&w, &session).as_slice(), [(_, false)]), "bash ran");
    assert!(w.repo().join("approved.txt").exists(), "B's answer unblocked the terminal's turn");
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
async fn at_rest(w: &World, session: &str) -> Result<Vec<serde_json::Value>, String> {
    let (api, keys, chain, id) = (w.client(), w.keys(), w.chain(), session.to_string());
    tokio::task::spawn_blocking(move || {
        let s = api.show_sync_session(&id).map_err(|e| e.to_string())?;
        let key = krowk_harness::sync::store::open_session_key(&s, &id, &keys, &chain, session_record::Signer::EverHeld)?;
        let index = krowk_harness::sync::store::open_index(&key, &id, &s.sealed_index)?;
        Ok(krowk_harness::sync::store::attach(&api, &key, &id, index, None)?.events)
    })
    .await
    .unwrap()
}

async fn at_rest_ids(w: &World, session: &str) -> Result<Vec<String>, String> {
    Ok(at_rest(w, session).await?.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect())
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
    let (_stop, _cp, _bridge) = w.bridge_with(&a, &session, w.daemon().await, 10, host::KEEP, host::APPROVAL_WAIT);
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
    let (_stop, _cp, bridge) = w.bridge_with(&a, &session, w.daemon().await, 10, host::KEEP, host::APPROVAL_WAIT);
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
    let prompt = w.prompt(Some(&session), "an answer of many batches, please");
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
    assert!(text.contains("word0 ") && text.contains("word399 "), "the whole answer: {} chars", text.len());
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

/// R-SYNC-2: a viewer on the host's own device — `krowk sync attach` on
/// machine A while A hosts — joins and leaves, and the host stays on the
/// relay: B, watching from another device, is never told the host went,
/// and a prompt B types afterwards is sent, not queued, and runs on A.
#[tokio::test]
async fn r_sync_2_a_viewer_on_the_hosts_own_device_leaving_leaves_the_host_on_the_relay() {
    let w = World::new("samedev");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut vb = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut vb, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    for _ in 0..2 {
        let mut own = viewer::attach(w.viewer(&a, &session)).await.unwrap();
        until(&mut own, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
        drop(own);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    vb.commands.send(w.prompt(Some(&session), "after A's own viewer left")).unwrap();
    let got = until(&mut vb, Duration::from_secs(20), &mut frames, result_of).await;
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Sent { queued: false, .. })), "B still sees the host: {got:?}");
    assert!(!got.iter().any(|u| matches!(u, viewer::Update::Host(false))), "B was told the host went: {got:?}");
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&session).join("events.jsonl")).unwrap();
    assert!(log.contains("after A's own viewer left"), "B's prompt ran on A");
}

/// R-SYNC-2, R-OFF-2: the relay lets A's link go — `replaced`, here — and
/// its close never reaches A, the connection left open and silent, as a
/// Worker's edge leaves one whose heartbeats it goes on answering. A joins
/// again at once, well inside the 30 seconds a dead link takes to show, and
/// a prompt B types then runs.
#[tokio::test]
async fn r_off_2_a_host_whose_link_the_relay_let_go_joins_again_at_once() {
    let w = World::new("ghost");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    let joins = w.a_conns.load(Ordering::SeqCst);
    w.a_mode.store(GHOST, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.a_conns.load(Ordering::SeqCst) == joins {
        assert!(Instant::now() < deadline, "A never joined the relay again");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    v.commands.send(w.prompt(Some(&session), "after the relay let A go")).unwrap();
    until(&mut v, Duration::from_secs(20), &mut frames, result_of).await;
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&session).join("events.jsonl")).unwrap();
    assert_eq!(log.lines().filter(|l| l.contains("after the relay let A go") && l.contains("userText")).count(), 1);
}

/// R-OFF-2, R-LAG-8: A stops — SIGSTOP, a laptop asleep — past the time a
/// relay keeps a silent link, with a prompt B sent while A still looked
/// present in flight to it; the relay's copy of it is lost with A's link.
/// When A runs again it joins again, B — told the host is back — sends
/// the prompt again, and A runs it, once.
#[tokio::test]
async fn r_off_2_a_prompt_sent_to_a_host_that_stopped_runs_once_when_it_is_back() {
    let limits = krowk_harness::relay::Limits { heartbeat: Duration::from_secs(2), ..Default::default() };
    let w = World::with_relay("frozen", limits);
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    w.a_mode.store(FREEZE, Ordering::SeqCst);
    v.commands.send(w.prompt(Some(&session), "sent-while-a-was-stopped")).unwrap();
    let got = until(&mut v, Duration::from_secs(5), &mut frames, |u| matches!(u, viewer::Update::Sent { .. })).await;
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Sent { queued: false, .. })), "B believed the host present: {got:?}");
    // Past the reference relay's three silent heartbeats; the hosted relay
    // keeps the link, and the relay's copy is lost with it all the same.
    tokio::time::sleep(Duration::from_secs(8)).await;
    // A runs again, and finds its link gone with what was in flight on it.
    w.cut.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.a_mode.store(PASS, Ordering::SeqCst);
    w.cut.store(false, Ordering::SeqCst);
    until(&mut v, Duration::from_secs(30), &mut frames, result_of).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&session).join("events.jsonl")).unwrap();
    let n = log.lines().filter(|l| l.contains("sent-while-a-was-stopped") && l.contains("userText")).count();
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
    let (_stop, _cp, _bridge) = w.bridge_with(&a, &session, w.daemon().await, host::LEASE_TTL, 4, host::APPROVAL_WAIT);
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

/// The stand-in registry's ticket key, as a host's direct listener holds it.
fn ticket_roster() -> krowk_harness::relay::Roster {
    krowk_harness::relay::Roster::parse(&format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()))).unwrap()
}

/// A stand-in `tailscaled`: its LocalAPI on a unix socket, answering
/// `status` with this machine on 127.0.0.1 as tailnet user 1 and a peer,
/// and `whois` with whichever user `whois` holds.
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
                        "Peer": {"k": {"HostName": "b", "DNSName": "b.tail.ts.net.", "TailscaleIPs": ["100.64.0.2"], "Online": true}}}).to_string()
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
/// longer cross the relay. A names itself and its tailnet node in the
/// sealed index, for `krowk hosts` (R-NET-4).
#[tokio::test]
async fn r_net_2_the_session_moves_to_the_direct_path_and_live_frames_leave_the_relay() {
    let w = World::new("direct");
    let ts = FakeTailscale::start(&w.root);
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_direct(&a, &session, w.daemon().await, &ts, true, kill_rx);
    w.synced(&session).await;

    // R-NET-4: A names itself in the sealed index, with its tailnet node,
    // which is how `krowk hosts` finds it with no tag on it.
    let (api, id, keys, chain) = (w.client(), session.clone(), w.keys(), w.chain());
    let index = tokio::task::spawn_blocking(move || {
        let s = api.show_sync_session(&id).unwrap();
        let key = krowk_harness::sync::store::open_session_key(&s, &id, &keys, &chain, session_record::Signer::EverHeld).unwrap();
        krowk_harness::sync::store::open_index(&key, &id, &s.sealed_index).unwrap()
    })
    .await
    .unwrap();
    let kept = index.host.expect("R-NET-4: the index names its host");
    assert_eq!(kept.device, a.key.id().to_string());
    assert_eq!(kept.tailnet.map(|t| (t.name, t.dns_name)), Some(("a".to_string(), "localhost".to_string())));

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

/// R-NET-2 (todo 22d): B attaches with a prompt still queued, the turn
/// streams to it over the relay, and only then — mid-session, an approval
/// waiting — does B's race for the direct path get its ticket and move.
/// The direct listener replays from the cursor B had when the race began,
/// so its first window is all batches B already opened by the relay. B
/// still acks them, so the stream never stalls there; every event of A's
/// log reaches B exactly once, and the approval request, which both
/// welcomes name and the stream carries, is shown once.
#[tokio::test]
async fn r_net_2_moving_to_the_direct_path_mid_session_stalls_nothing_and_shows_each_approval_once() {
    let w = World::new("direct-stall");
    let ts = FakeTailscale::start(&w.root);
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_direct(&a, &session, w.daemon().await, &ts, false, kill_rx);
    w.synced(&session).await;
    let hold = Arc::new(AtomicU8::new(HOLD_TICKETS));
    // A failing assertion lets the held ticket go too: the runtime waits
    // for its blocking tasks as it shuts down, and would sit out the test's
    // timeout rather than report.
    struct Release(Arc<AtomicU8>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(PASS, Ordering::SeqCst);
        }
    }
    let _release = Release(hold.clone());
    let reg_b = format!("http://{}/v1", registry_proxy(w.registry.addr(), hold.clone(), Arc::default(), Arc::default()));
    let mut o = w.viewer(&b, &session);
    o.api = Arc::new(krowk_api::Client::new(&reg_b, &b.token).signed_by(signer(&b)));
    let mut v = viewer::attach(o).await.unwrap();
    let mut frames = Vec::new();
    v.commands.send(w.prompt(Some(&session), "a long answer, queued")).unwrap();
    // The long answer streams for about 3.3 seconds, on schedule wherever
    // it runs (the mock makes up a late wake); the wait is only how long
    // the test gives up after.
    let mut got = until(&mut v, Duration::from_secs(90), &mut frames, result_of).await;
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Sent { queued: true, .. })), "the prompt waited for the host: {got:?}");
    assert!(!got.iter().any(direct_path), "the race waits on its ticket");
    v.commands.send(w.prompt(Some(&session), "make the file")).unwrap();
    got.extend(until(&mut v, Duration::from_secs(30), &mut frames, |u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(_))))).await);

    // The race goes on, and B moves with the approval waiting.
    hold.store(PASS, Ordering::SeqCst);
    got.extend(until(&mut v, Duration::from_secs(30), &mut frames, direct_path).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(batch) = v.updates.try_recv() {
        got.extend(batch);
    }
    let asked: Vec<_> = got.iter().filter_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) = u { Some(r.clone()) } else { None }).collect();
    assert_eq!(asked.len(), 1, "the approval request was shown {} times", asked.len());
    let req = asked[0].clone();
    v.commands.send(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() }).unwrap();
    got.extend(until(&mut v, Duration::from_secs(30), &mut frames, result_of).await);
    assert!(w.repo().join("approved.txt").exists(), "B's answer unblocked A's turn");

    let mut ids: Vec<String> = got.iter().flat_map(logged).collect();
    let want = log_ids(&w, &session);
    let deadline = Instant::now() + Duration::from_secs(10);
    while want.iter().any(|i| !ids.contains(i)) && Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(300), v.updates.recv()).await {
            got.extend(batch.clone());
            ids.extend(batch.iter().flat_map(logged));
        }
    }
    let missing: Vec<_> = want.iter().filter(|i| !ids.contains(i)).collect();
    assert!(missing.is_empty(), "B never got {} of A's {} events", missing.len(), want.len());
    let mut dedup = ids.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), ids.len(), "no event arrived twice");
    let asked = got.iter().filter(|u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(_))))).count();
    assert_eq!(asked, 1, "the approval request was shown once");
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

/// R-NET-3: being on the tailnet admits nobody. A connection to the direct
/// listener's address — from this tailnet user, so whois passes — joins
/// only with a ticket under a key the registry publishes and a challenge
/// signed by the key that ticket names: none, one signed by anyone else,
/// and the device's own presented under another key are each refused.
#[tokio::test]
async fn r_net_3_on_the_tailnet_without_the_registrys_ticket_nobody_joins_the_direct_path() {
    let w = World::new("tailnet");
    let ts = FakeTailscale::start(&w.root);
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_direct(&a, &session, w.daemon().await, &ts, true, kill_rx);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let got = until(&mut v, Duration::from_secs(15), &mut Vec::new(), direct_path).await;
    let via = got.iter().find_map(|u| if let viewer::Update::Path { via: Some(via), .. } = u { Some(via.clone()) } else { None }).unwrap();

    let (api, id, device) = (w.as_device(&b), session.clone(), b.key.id().to_string());
    let real = tokio::task::spawn_blocking(move || api.relay_ticket(&id, &device, "development").unwrap().relay_ticket).await.unwrap();
    let keys = [(krowk_devregistry::TICKET_KID, krowk_devregistry::ticket_public_key())];
    let forged = krowk_client::relay_ticket::verify(&real, &keys, krowk_client::relay_ticket::now()).unwrap().sign(&[0x77; 32]);
    let stranger = SigningKey::from_secret(&[0x42; 32]).unwrap();
    for (what, ticket, signing, code) in [("no ticket", "", &b.signing, "bad_ticket"), ("a ticket the registry did not sign", &forged, &b.signing, "bad_ticket"), ("B's ticket under another key", &real, &stranger, "bad_signature")] {
        match join_direct(&via, &session, &b, ticket, signing).await {
            Ok(()) => panic!("{what}: joined the direct path"),
            Err(e) => assert!(e.contains(code), "{what}: {e}"),
        }
    }
    join_direct(&via, &session, &b, &real, &b.signing).await.expect("B's own ticket and key join, so each refusal above was the ticket's or the key's");
}

/// R-NET-2, R-NET-3 (ticket 27): with no Tailscale on A, A offers the LAN
/// alone and advertises it over mDNS under the session's tag; B moves onto
/// it — "direct over LAN" — and none of a live turn's bytes reach B by the
/// relay. Being on the LAN admits nobody: no ticket, a ticket the registry
/// did not sign, and B's ticket under another key are each refused there,
/// and B's own ticket and key join.
#[tokio::test]
async fn r_net_2_without_tailscale_the_session_moves_to_the_lan_and_r_net_3_only_a_ticket_joins_there() {
    let w = World::new("lan");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (_kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_lan(&a, &session, w.daemon().await, kill_rx);
    w.synced(&session).await;

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let got = until(&mut v, Duration::from_secs(15), &mut frames, direct_path).await;
    let (path, via) = got.iter().find_map(|u| if let viewer::Update::Path { path, via: Some(via) } = u { Some((path.clone(), via.clone())) } else { None }).unwrap();
    assert_eq!(path, "direct over LAN");
    assert!(via.starts_with("ws://127.0.0.1:"), "{via}");

    // The listener is found over mDNS by the session's tag alone.
    let (tx, mut found) = mpsc::unbounded_channel();
    let tag_of = krowk_harness::daemon::ws::uuid(&session);
    tokio::task::spawn_blocking(move || krowk_harness::sync::mdns::browse(tag_of, Duration::from_secs(3), true, tx)).await.unwrap();
    let mut advertised = Vec::new();
    while let Ok(c) = found.try_recv() {
        advertised.push(c.url);
    }
    assert!(advertised.contains(&via), "A's listener {via} was not found over mDNS: {advertised:?}");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = w.b_batches.load(Ordering::SeqCst);
    v.commands.send(w.prompt(Some(&session), "over the LAN")).unwrap();
    let got = until(&mut v, Duration::from_secs(15), &mut frames, result_of).await;
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Line(_))), "the turn streamed to B");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.b_batches.load(Ordering::SeqCst), before, "live frames still went to B through the relay");

    let (api, id, device) = (w.as_device(&b), session.clone(), b.key.id().to_string());
    let real = tokio::task::spawn_blocking(move || api.relay_ticket(&id, &device, "development").unwrap().relay_ticket).await.unwrap();
    let keys = [(krowk_devregistry::TICKET_KID, krowk_devregistry::ticket_public_key())];
    let forged = krowk_client::relay_ticket::verify(&real, &keys, krowk_client::relay_ticket::now()).unwrap().sign(&[0x77; 32]);
    let stranger = SigningKey::from_secret(&[0x42; 32]).unwrap();
    for (what, ticket, signing, code) in [("no ticket", "", &b.signing, "bad_ticket"), ("a ticket the registry did not sign", &forged, &b.signing, "bad_ticket"), ("B's ticket under another key", &real, &stranger, "bad_signature")] {
        match join_direct(&via, &session, &b, ticket, signing).await {
            Ok(()) => panic!("{what}: joined the LAN path"),
            Err(e) => assert!(e.contains(code), "{what}: {e}"),
        }
    }
    join_direct(&via, &session, &b, &real, &b.signing).await.expect("B's own ticket and key join, so each refusal above was the ticket's or the key's");
}

/// R-NET-2 (ticket 27): A's LAN address moves from 127.0.0.2 to 127.0.0.3
/// after A took the session up. A follows the move: it listens on the new
/// address and no longer on the old, advertises only the new one over mDNS,
/// and names it in the welcomes it gives after, so B, attaching now, moves
/// onto it. A viewer welcomed before the move reaches it through mDNS once
/// its welcome's address fails; that ordering is the race's, and the
/// finding is `sync/mdns.rs`' own test. Linux only: macOS answers on
/// 127.0.0.1 alone.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn r_net_2_a_host_whose_lan_address_moves_listens_offers_and_advertises_the_new_one() {
    let w = World::new("lan-moves");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let moved = Arc::new(AtomicBool::new(false));
    let m = moved.clone();
    let at: direct::LanAt = Arc::new(move || vec![if m.load(Ordering::SeqCst) { [127, 0, 0, 3] } else { [127, 0, 0, 2] }.into()]);
    let (_kill, kill_rx) = watch::channel(false);
    let (_stop, _cp, _bridge) = w.bridge_lan_at(&a, &session, w.daemon().await, kill_rx, at);
    w.synced(&session).await;
    moved.store(true, Ordering::SeqCst);
    // Past A's next look at its addresses, and the old one's goodbye.
    tokio::time::sleep(direct::LAN_CHECK + Duration::from_secs(2)).await;

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let got = until(&mut v, Duration::from_secs(30), &mut Vec::new(), direct_path).await;
    let via = got.iter().find_map(|u| if let viewer::Update::Path { via: Some(via), .. } = u { Some(via.clone()) } else { None }).expect("B went direct");
    assert!(via.starts_with("ws://127.0.0.3:"), "B moved onto {via}, not the address A moved to");
    let port = via.rsplit(':').next().unwrap();
    assert!(std::net::TcpStream::connect(format!("127.0.0.2:{port}")).is_err(), "A still listens on the address it lost");
    let (tx, mut found) = mpsc::unbounded_channel();
    let tag_of = krowk_harness::daemon::ws::uuid(&session);
    tokio::task::spawn_blocking(move || krowk_harness::sync::mdns::browse(tag_of, Duration::from_secs(3), true, tx)).await.unwrap();
    let mut advertised = Vec::new();
    while let Ok(c) = found.try_recv() {
        advertised.push(c.url);
    }
    assert!(advertised.contains(&via) && !advertised.iter().any(|u| u.starts_with("ws://127.0.0.2:")), "mDNS should name the new address alone: {advertised:?}");
    v.commands.send(w.prompt(Some(&session), "after the move")).unwrap();
    until(&mut v, Duration::from_secs(15), &mut Vec::new(), result_of).await;
}

/// Joins the direct listener at `via` as `b`'s viewer, with `ticket` and a
/// challenge signed by `signing`.
async fn join_direct(via: &str, session: &str, b: &Device, ticket: &str, signing: &SigningKey) -> Result<(), String> {
    let j = krowk_harness::sync::Join { relay: via, session, env: "development", ticket, device: b.key.id(), signing, role: e2e::RELAY_ROLE_VIEWER, extra: serde_json::json!({}) };
    krowk_harness::sync::join(j).await.map(|_| ())
}


/// R-OFF-2: the relay drops every batch of a whole turn to B, and B sees
/// the gap only when a later turn's batch arrives. The catch-up asks from
/// the newest event B held before that batch, so the dropped turn's events
/// arrive too.
#[tokio::test]
async fn r_off_2_a_whole_turn_the_relay_dropped_arrives_when_a_later_turn_shows_the_gap() {
    let w = World::new("dropturn");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let mut ids: Vec<String> = until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await.iter().flat_map(logged).collect();
    w.b_drop.store(usize::MAX, Ordering::SeqCst);
    w.b_mode.store(DROP_BATCHES, Ordering::SeqCst);
    turn(&d, &w, &session, "turn-whose-batches-drop").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    w.b_mode.store(PASS, Ordering::SeqCst);
    turn(&d, &w, &session, "a later turn").await;
    let want = log_ids(&w, &session);
    let deadline = Instant::now() + Duration::from_secs(15);
    while want.iter().any(|i| !ids.contains(i)) && Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(500), v.updates.recv()).await {
            ids.extend(batch.iter().flat_map(logged));
        }
    }
    let missing: Vec<_> = want.iter().filter(|i| !ids.contains(i)).collect();
    assert!(missing.is_empty(), "B never got {} of A's {} events", missing.len(), want.len());
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// R-HAND-1, R-HAND-2, R-HAND-3: a session moves from A to B mid-task.
/// B asks A over the E2E link; A checkpoints, ships its base commit, its
/// uncommitted change and the untracked file the session made as a
/// transport artifact, and hands B its lease. B applies them into a fresh
/// worktree of its own clone and hosts the session from there. C, attached
/// throughout, sees the host go and come back, and its next prompt runs on
/// B, in B's worktree. The transport is spent once read, and a free
/// workspace may not store one.
#[tokio::test]
async fn r_hand_1_a_session_moves_to_b_mid_task_with_its_work_and_c_stays_attached() {
    use krowk_harness::sync::handoff;
    let w = World::new("handoff");
    let (a, b, c) = (w.device("machine-a"), w.device("machine-b"), w.device("machine-c"));
    let repo = w.repo();
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("tracked.txt"), "one\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    // Untracked before the session started: not the session's to move.
    std::fs::write(repo.join("old.txt"), "from before").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let (_d, session) = first_turn(&w).await;
    // The session's work, uncommitted.
    std::fs::write(repo.join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::create_dir_all(repo.join("notes")).unwrap();
    std::fs::write(repo.join("notes/new.txt"), "made mid-task").unwrap();
    let (_stop, _cp, a_bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;

    let mut cv = viewer::attach(w.viewer(&c, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut cv, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;

    // B: a clone of the repository, a home and a daemon of its own.
    let b_repo = w.root.join("repo-b");
    git(&w.root, &["clone", "-q", repo.to_str().unwrap(), b_repo.to_str().unwrap()]);
    let b_env = w.env_at("home-b", "run-b");
    w.serve_env(b_env.clone(), "home-b");

    let mut bv = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    bv.handoff.send(()).unwrap();
    let got = until(&mut bv, Duration::from_secs(30), &mut frames, |u| matches!(u, viewer::Update::Handoff(_))).await;
    let handed = got.into_iter().find_map(|u| if let viewer::Update::Handoff(r) = u { Some(r) } else { None }).unwrap().expect("A hands the session over");
    drop(bv);
    let ended = tokio::time::timeout(Duration::from_secs(20), a_bridge).await.expect("A's bridge stops").unwrap();
    assert!(ended.as_ref().is_err_and(|e| e.starts_with(host::HANDED_OVER)), "{ended:?}");

    // B takes it up, as `krowk sync take` does.
    let (api, keys, chain, id, slug, b_env2, from, me, wt_root, b_repo2) = (w.as_device(&b), w.keys(), w.chain(), session.clone(), handed.transport.clone(), b_env.clone(), handed.from_device.clone(), b.key.id().to_string(), w.root.join("worktrees-b"), b_repo.clone());
    let key = handoff::key_from_hex(&handed.key).unwrap();
    let applied = tokio::task::spawn_blocking(move || {
        let sealed = api.read_transport(&slug).unwrap();
        let plain = e2e::open_transport(&sealed, &krowk_harness::daemon::ws::uuid(&id), &key).unwrap();
        let bundle = handoff::Bundle::decode(&plain, &id).unwrap();
        let applied = handoff::apply(&bundle, &b_repo2, &wt_root).unwrap();
        api.spend_transport(&slug).unwrap();
        let gone = api.read_transport(&slug).unwrap_err();
        assert_eq!(gone.status, 404, "R-HAND-2: a transport is read once, and gone when spent");
        let s = api.show_sync_session(&id).unwrap();
        let k = krowk_harness::sync::store::open_session_key(&s, &id, &keys, &chain, session_record::Signer::EverHeld).unwrap();
        let index = krowk_harness::sync::store::open_index(&k, &id, &s.sealed_index).unwrap();
        let events = krowk_harness::sync::store::attach(&api, &k, &id, index, None).unwrap().events;
        handoff::restore_log(&log::sessions_dir(&b_env2).unwrap(), &id, &events, &applied.cwd, &me, &from).unwrap();
        applied
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(applied.cwd.join("tracked.txt")).unwrap(), "one\ntwo\n", "the uncommitted change moved");
    assert_eq!(std::fs::read_to_string(applied.cwd.join("notes/new.txt")).unwrap(), "made mid-task", "the untracked file the session made moved");
    assert!(!applied.cwd.join("old.txt").exists(), "an untracked file from before the session did not");

    let b_daemon = Arc::new(Client::connect(&daemon::socket(&b_env).unwrap(), &applied.cwd, "test", true).await.ok().expect("B's daemon answers"));
    let o = host::Options {
        relay: w.relay_a.clone(),
        env: "development".into(),
        api: w.as_device(&b),
        device: b.key.id(),
        signing: SigningKey::from_secret(&*b.signing.secret_bytes()).unwrap(),
        keys: w.keys(),
        chain: w.chain(),
        session: session.clone(),
        title: String::new(),
        cwd: applied.cwd.display().to_string(),
        sessions: log::sessions_dir(&b_env).unwrap(),
        ttl: host::LEASE_TTL,
        keep: host::KEEP,
        direct: None,
        tailnet: None,
        handed: Some(handed),
        approval_wait: host::APPROVAL_WAIT,
        stale: None,
        supervised: None,
    };
    let (_b_stop, _b_cp, _b_bridge) = w.run_bridge(o, b_daemon);

    // C never let go: the host went, came back, and runs C's prompt on B.
    until(&mut cv, Duration::from_secs(30), &mut frames, |u| matches!(u, viewer::Update::Host(false))).await;
    until(&mut cv, Duration::from_secs(30), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    cv.commands.send(w.prompt(Some(&session), "make the file")).unwrap();
    let got = until(&mut cv, Duration::from_secs(20), &mut frames, |u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(_))))).await;
    let req = got.iter().find_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) = u { Some(r.clone()) } else { None }).unwrap();
    cv.commands.send(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() }).unwrap();
    until(&mut cv, Duration::from_secs(20), &mut frames, result_of).await;
    assert!(applied.cwd.join("approved.txt").exists(), "the turn ran on B, in B's worktree");
    assert!(!repo.join("approved.txt").exists(), "not on A");
    let b_log = std::fs::read_to_string(log::sessions_dir(&b_env).unwrap().join(&session).join("events.jsonl")).unwrap();
    assert!(b_log.contains("session.moved"), "B logged the move");

    // R-HAND-3: a free workspace stores no transport.
    let free = krowk_api::Client::new(&w.api, "krowk_sk_free_0000000000000000000000").signed_by(signer(&b));
    let refused = tokio::task::spawn_blocking(move || free.put_transport("01a0ec7b-3333-7000-8000-000000000001", b"x", "t")).await.unwrap().unwrap_err();
    assert_eq!(refused.code(), "sync_requires_paid_plan", "{refused:?}");
}

/// The ids of a session's events, `of` alone, in the order they came.
fn ids_of(events: &[serde_json::Value], of: &str) -> Vec<String> {
    events.iter().filter(|e| e["sessionId"] == of).filter_map(|e| e["id"].as_str().map(String::from)).collect()
}

/// Waits until the parent's chunks hold the logs of `sessions` whole, each
/// in its log's order, and returns what they hold. The logs are read again
/// each time, so an event logged late is waited for rather than missed; and
/// the wait is judged on a read begun after `wait` has passed, never on one
/// begun before it, so a read the registry is slow to answer is not taken
/// for chunks missing.
async fn chunks_hold(w: &World, session: &str, sessions: &[&str], wait: Duration) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + wait;
    loop {
        let late = Instant::now() > deadline;
        let rest = at_rest(w, session).await.unwrap_or_default();
        let short: Vec<_> = sessions.iter().map(|s| (*s, log_ids(w, s))).filter(|(s, ids)| ids_of(&rest, s) != *ids).collect();
        if short.is_empty() {
            return rest;
        }
        assert!(!late, "the chunks hold, of the logs {:?}, only {:?}", short, short.iter().map(|(s, _)| ids_of(&rest, s)).collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// R-SUB-12, R-SUB-6: B stops a subagent of the session it follows, by
/// the child's id, while the parent's turn — started at A's terminal, in
/// another directory than the bridge's — goes on and ends. B can stop
/// nothing else that way: another session on A, an id it made up, and a
/// steer of the child are refused. And the child's logged events are
/// written into the parent's chunks: the parent's branch walk leaves them
/// out, so its replay is its own log's, and the child is rebuilt from them.
#[tokio::test]
async fn r_sub_12_a_viewer_stops_the_sessions_own_child_and_nothing_else() {
    let w = World::new("child");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let other = d.execute(w.prompt(None, "another session"), tx).await.unwrap().unwrap().session_id;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;

    // The person at A's terminal, in a subdirectory: the child runs on that
    // directory's host, not on the bridge's.
    let sub = w.repo().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let terminal = Client::connect(&daemon::socket(&w.env()).unwrap(), &sub, "test", true).await.ok().expect("the daemon answers");
    let ask = w.prompt(Some(&session), "start a slow subagent");
    let parent_turn = tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel(4096);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        terminal.execute(ask, tx).await.map(|r| r.unwrap().status)
    });
    let started = |u: &viewer::Update| match u {
        viewer::Update::Line(StreamLine::Log(e)) if e.session_id == session => match &e.body {
            krowk_harness::protocol::LogBody::SubagentStarted { subagent_session_id, .. } => Some(subagent_session_id.clone()),
            _ => None,
        },
        _ => None,
    };
    let got = until(&mut v, Duration::from_secs(15), &mut frames, |u| started(u).is_some()).await;
    let child = got.iter().find_map(started).unwrap();
    until(&mut v, Duration::from_secs(15), &mut frames, |u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ItemDelta { session_id, .. })) if *session_id == child)).await;
    // A client in the bridge's directory following the child's own id: the
    // hub that makes is on that directory's host, where the child runs
    // nothing, and the interrupt still goes to the parent's.
    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let follower = w.daemon().await;
    follower.attach(&child, None, tx).await.unwrap();

    let mut ack = async |c: Command| {
        v.commands.send(c).unwrap();
        let got = until(&mut v, Duration::from_secs(5), &mut frames, |u| matches!(u, viewer::Update::Acked { .. })).await;
        got.into_iter().find_map(|u| if let viewer::Update::Acked { error, .. } = u { Some(error) } else { None }).unwrap()
    };
    for refused in [other.as_str(), "01a0ec7b-0000-7000-8000-00000000beef"] {
        let e = ack(Command::Interrupt { session_id: refused.into() }).await;
        assert!(e.as_deref().is_some_and(|e| e.contains("nothing else")), "{refused}: {e:?}");
    }
    let e = ack(Command::Steer { session_id: child.clone(), text: "not you".into(), images: Vec::new() }).await;
    assert!(e.as_deref().is_some_and(|e| e.contains("nothing else")), "a steer of the child: {e:?}");
    assert_eq!(ack(Command::Interrupt { session_id: child.clone() }).await, None, "the session's own child stops");

    let status = tokio::time::timeout(Duration::from_secs(15), parent_turn).await.expect("the parent's turn ends").unwrap().unwrap();
    assert_eq!(status, krowk_harness::protocol::TurnStatus::Completed, "the parent's turn goes on past its stopped child");
    let child_log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&child).join("events.jsonl")).unwrap();
    assert!(child_log.contains("\"status\":\"interrupted\""), "the child ended interrupted: {child_log}");
    assert!(!child_log.contains("childword399"), "short of its end");
    let other_log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&other).join("events.jsonl")).unwrap();
    assert!(!other_log.contains("interrupted"), "the other session was never reached");

    // R-SUB-6: the child's events ride the parent's chunks.
    let rest = chunks_hold(&w, &session, &[&session, &child], Duration::from_secs(30)).await;
    let (parent_ids, child_ids) = (log_ids(&w, &session), log_ids(&w, &child));
    assert!(rest.iter().all(|e| e["sessionId"] == session || e["sessionId"] == child), "nothing of another session");
    let events: Vec<krowk_harness::protocol::LogEvent> = rest.iter().map(|e| serde_json::from_value(e.clone()).unwrap()).collect();
    let walk = |head: &String| log::branch(&events, head).iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    assert_eq!(walk(parent_ids.last().unwrap()), parent_ids, "the parent's replay is its own log's, the child's events left out");
    assert_eq!(walk(child_ids.last().unwrap()), child_ids, "the child is rebuilt from the parent's chunks");
}

/// The fake `claude` beside the Claude Code tests, as an instance whose
/// sessions play `scenario`: logged in, with a config directory of its own.
fn fake_claude(w: &World, scenario: &Path) -> InstancesConfig {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude");
    let bin = w.root.join("bin/claude");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(fixtures.join("fake-claude"), &bin).unwrap();
    let config = w.root.join("claude-config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("fake-login"), "").unwrap();
    let mut env = std::collections::BTreeMap::new();
    env.insert("FAKE_CLAUDE_SCENARIO".to_string(), scenario.display().to_string());
    env.insert("FAKE_CLAUDE_LOG".to_string(), w.root.join("fake-claude.log").display().to_string());
    let kind = krowk_harness::instances::InstanceKind::ClaudeCode { binary: Some(bin.display().to_string()), config_dir: Some(config.display().to_string()), env, args: Vec::new(), api_key_env: None, effort: None };
    InstancesConfig { instances: [("claude".to_string(), kind)].into_iter().collect(), ..Default::default() }
}

/// R-SUB-12, R-SUB-9: a Claude Code agent's approval is asked under its
/// child session, and a viewer answers it as it answered the session's own
/// before the agent had a session: B prompts, the recorded sleeper agent
/// asks for its Bash, and B's answer under the child's id reaches Claude
/// Code. An answer naming another session, or an id the session's log
/// never named, is refused before it is looked up.
#[tokio::test]
async fn r_sub_12_a_viewer_answers_a_claude_agents_approval_and_nothing_elses() {
    let w = World::new("claude-ask");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let raw = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/recorded/agents_parallel.txt")).unwrap();
    let printed: Vec<serde_json::Value> = raw.lines().filter_map(|l| l.strip_prefix("<< ")).map(|j| serde_json::from_str(j).unwrap()).collect();
    let init = printed.iter().position(|m| m["subtype"] == "init").unwrap();
    let mut scenario = String::from("{\"type\":\"assistant\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"msg_hi\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}},\"parent_tool_use_id\":null,\"session_id\":\"{{SESSION}}\"}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"hello\",\"session_id\":\"{{SESSION}}\"}\n---\n");
    for m in &printed[init + 1..] {
        match m["type"].as_str() {
            Some("control_request") if m["request"]["subtype"] == "can_use_tool" => scenario.push_str(&format!("@ask {}\n", m["request"])),
            Some("control_request" | "control_response") => {}
            _ if m["isReplay"] == true => {}
            _ => scenario.push_str(&format!("{m}\n")),
        }
    }
    let path = w.root.join("agents.jsonl");
    std::fs::write(&path, scenario).unwrap();
    w.serve_with(w.env(), "home", fake_claude(&w, &path));
    let d = w.daemon().await;
    let claude = krowk_harness::protocol::ModelRef { instance: "claude".into(), model: "sonnet".into() };
    let ask = |session: Option<&str>, text: &str| Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model: Some(claude.clone()), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
    let (tx, mut rx) = mpsc::channel(4096);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let first = d.execute(ask(None, "say hello"), tx).await.unwrap().unwrap();
    let session = first.session_id;
    let (tx, mut rx) = mpsc::channel(4096);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let other = d.execute(ask(None, "another session"), tx).await.unwrap().unwrap().session_id;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;

    v.commands.send(ask(Some(&session), "run two agents")).unwrap();
    let asked = |u: &viewer::Update| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(_))));
    let got = until(&mut v, Duration::from_secs(20), &mut frames, asked).await;
    let req = got.iter().find_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) = u { Some(r.clone()) } else { None }).unwrap();
    assert_ne!(req.session_id, session, "the agent's own session asks");
    let parent = log::read_events(&log::sessions_dir(&w.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
    assert!(parent.iter().any(|e| matches!(&e.body, krowk_harness::protocol::LogBody::SubagentStarted { subagent_session_id, .. } if *subagent_session_id == req.session_id)), "one of the session's own children");

    let mut ack = async |c: Command| {
        v.commands.send(c).unwrap();
        let got = until(&mut v, Duration::from_secs(5), &mut frames, |u| matches!(u, viewer::Update::Acked { .. })).await;
        got.into_iter().find_map(|u| if let viewer::Update::Acked { error, .. } = u { Some(error) } else { None }).unwrap()
    };
    let answer = |session_id: &str| Command::Approve { session_id: session_id.into(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() };
    for refused in [other.as_str(), "01a0ec7b-0000-7000-8000-00000000beef"] {
        let e = ack(answer(refused)).await;
        assert!(e.as_deref().is_some_and(|e| e.contains("nothing else")), "{refused}: {e:?}");
    }
    assert_eq!(ack(answer(&req.session_id)).await, None, "the session's own child's approval is answered");
    until(&mut v, Duration::from_secs(20), &mut frames, result_of).await;
    let fake = std::fs::read_to_string(w.root.join("fake-claude.log")).unwrap();
    assert!(fake.lines().any(|l| l.starts_with("answer ") && l.contains(r#""behavior":"allow","updatedInput":{"command":"sleep 30"#)), "Claude Code was answered: {fake}");
}

/// R-SUB-12, R-SUB-10: a viewer stops one of a Claude Code session's
/// agents by its child session's id, as it stops a native child: the
/// command reaches the daemon running the turn through the bridge, the
/// daemon routes it to the host whose turn runs the child, and that host
/// sends Claude Code `stop_task` for that agent alone. The child ends
/// interrupted; the parent's turn goes on to its own result.
#[tokio::test]
async fn r_sub_12_a_viewer_stops_one_claude_agent() {
    let w = World::new("claude-stop");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let raw = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/recorded/agents_parallel.txt")).unwrap();
    let printed: Vec<serde_json::Value> = raw.lines().filter_map(|l| l.strip_prefix("<< ")).map(|j| serde_json::from_str(j).unwrap()).collect();
    let sleeper = printed.iter().find(|m| m["subtype"] == "task_started" && m["description"] == "sleeper").unwrap()["task_id"].as_str().unwrap().to_string();
    let init = printed.iter().position(|m| m["subtype"] == "init").unwrap();
    let mut scenario = String::from("{\"type\":\"assistant\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"msg_hi\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}},\"parent_tool_use_id\":null,\"session_id\":\"{{SESSION}}\"}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"hello\",\"session_id\":\"{{SESSION}}\"}\n---\n");
    // Where the recording stopped the sleeper, the fake waits for krowk to.
    for m in &printed[init + 1..] {
        let said_stopped = m["task_id"] == sleeper.as_str() && (m["patch"]["status"] == "killed" || m["status"] == "stopped");
        match m["type"].as_str() {
            _ if said_stopped && m["subtype"] == "task_updated" => scenario.push_str("@await-stop\n"),
            _ if said_stopped => {}
            Some("control_request" | "control_response") => {}
            _ if m["isReplay"] == true => {}
            _ => scenario.push_str(&format!("{m}\n")),
        }
    }
    let path = w.root.join("agents.jsonl");
    std::fs::write(&path, scenario).unwrap();
    w.serve_with(w.env(), "home", fake_claude(&w, &path));
    let d = w.daemon().await;
    let claude = krowk_harness::protocol::ModelRef { instance: "claude".into(), model: "sonnet".into() };
    let ask = |session: Option<&str>, text: &str| Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model: Some(claude.clone()), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
    let (tx, mut rx) = mpsc::channel(4096);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let session = d.execute(ask(None, "say hello"), tx).await.unwrap().unwrap().session_id;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;

    v.commands.send(ask(Some(&session), "run two agents")).unwrap();
    let started = |u: &viewer::Update| match u {
        viewer::Update::Line(StreamLine::Log(e)) => match &e.body {
            krowk_harness::protocol::LogBody::SubagentStarted { subagent_session_id, backend_id: Some(t), .. } if *t == sleeper => Some(subagent_session_id.clone()),
            _ => None,
        },
        _ => None,
    };
    let got = until(&mut v, Duration::from_secs(20), &mut frames, |u| started(u).is_some()).await;
    let child = got.iter().find_map(started).unwrap();
    v.commands.send(Command::Interrupt { session_id: child.clone() }).unwrap();
    let got = until(&mut v, Duration::from_secs(15), &mut frames, |u| matches!(u, viewer::Update::Acked { .. })).await;
    let error = got.into_iter().find_map(|u| if let viewer::Update::Acked { error, .. } = u { Some(error) } else { None }).unwrap();
    assert_eq!(error, None, "the session's own Claude Code agent stops");
    let got = until(&mut v, Duration::from_secs(20), &mut frames, result_of).await;
    let status = got.iter().find_map(|u| match u {
        viewer::Update::Line(StreamLine::Live(LiveEvent::Result(r))) => Some(r.status),
        _ => None,
    });
    assert_eq!(status, Some(krowk_harness::protocol::TurnStatus::Completed), "the parent's turn goes on");
    let fake = std::fs::read_to_string(w.root.join("fake-claude.log")).unwrap();
    let stops: Vec<&str> = fake.lines().filter_map(|l| l.strip_prefix("stop_task ")).collect();
    assert_eq!(stops, [sleeper.as_str()], "that agent alone: {fake}");
    let child_log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&child).join("events.jsonl")).unwrap();
    assert!(child_log.contains("\"status\":\"interrupted\""), "the child ended interrupted: {child_log}");
}

/// R-SUB-6, R-SUB-7: Claude Code's agents, written as child sessions, ride
/// the parent's chunks as krowk's own children do — the recorded parallel
/// turn's two agents, on a session A's bridge syncs: every event of each
/// child's log is in the chunks, the parent's replay is its own log's, and
/// each child is rebuilt from them, its prompt first. What moves a session
/// moves them with it.
#[tokio::test]
async fn r_sub_6_claude_codes_agents_ride_the_parents_chunks_as_krowks_own_children_do() {
    let w = World::new("claude-kids");
    let a = w.device("machine-a");
    // A first turn to sync from, then the recording's turn after `init`,
    // its control lines and replayed prompt left out.
    let raw = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/recorded/agents_parallel.txt")).unwrap();
    let printed: Vec<serde_json::Value> = raw.lines().filter_map(|l| l.strip_prefix("<< ")).map(|j| serde_json::from_str(j).unwrap()).collect();
    let init = printed.iter().position(|m| m["subtype"] == "init").unwrap();
    let mut scenario = String::from("{\"type\":\"assistant\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"msg_hi\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}},\"parent_tool_use_id\":null,\"session_id\":\"{{SESSION}}\"}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"hello\",\"session_id\":\"{{SESSION}}\"}\n---\n");
    for m in printed[init + 1..].iter().filter(|m| !matches!(m["type"].as_str(), Some("control_request" | "control_response")) && m["isReplay"] != true) {
        scenario.push_str(&format!("{m}\n"));
    }
    let path = w.root.join("agents.jsonl");
    std::fs::write(&path, scenario).unwrap();
    w.serve_with(w.env(), "home", fake_claude(&w, &path));
    let d = w.daemon().await;
    let claude = krowk_harness::protocol::ModelRef { instance: "claude".into(), model: "sonnet".into() };
    let ask = |session: Option<&str>, text: &str| Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model: Some(claude.clone()), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
    let run = async |c: Command| {
        let (tx, mut rx) = mpsc::channel(4096);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        d.execute(c, tx).await.unwrap().unwrap()
    };
    let first = run(ask(None, "say hello")).await;
    assert_eq!(first.status, krowk_harness::protocol::TurnStatus::Completed, "{first:?}");
    let session = first.session_id;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let second = run(ask(Some(&session), "run two agents")).await;
    assert_eq!(second.status, krowk_harness::protocol::TurnStatus::Completed, "{second:?}");

    let parent = log::read_events(&log::sessions_dir(&w.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
    let kids: Vec<String> = parent.iter().filter_map(|e| match &e.body {
        krowk_harness::protocol::LogBody::SubagentStarted { subagent_session_id, ran_by: krowk_harness::protocol::RanBy::Claude, backend_id: Some(_), .. } => Some(subagent_session_id.clone()),
        _ => None,
    }).collect();
    assert_eq!(kids.len(), 2, "two children of Claude Code's");
    let all: Vec<&str> = std::iter::once(session.as_str()).chain(kids.iter().map(String::as_str)).collect();
    let rest = chunks_hold(&w, &session, &all, Duration::from_secs(30)).await;
    let parent_ids = log_ids(&w, &session);
    let events: Vec<krowk_harness::protocol::LogEvent> = rest.iter().map(|e| serde_json::from_value(e.clone()).unwrap()).collect();
    let walk = |head: &String| log::branch(&events, head).iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    assert_eq!(walk(parent_ids.last().unwrap()), parent_ids, "the parent's replay is its own log's, the children's events left out");
    for k in &kids {
        let ids = log_ids(&w, k);
        assert_eq!(walk(ids.last().unwrap()), ids, "{k} is rebuilt from the parent's chunks");
        let first_item = events.iter().filter(|e| e.session_id == *k).find_map(|e| match &e.body {
            krowk_harness::protocol::LogBody::ItemCompleted { item, .. } => Some(item.clone()),
            _ => None,
        });
        assert!(matches!(first_item, Some(krowk_harness::protocol::Item::UserText { .. })), "its prompt first: {first_item:?}");
    }
}

/// R-SUB-6, R-SUB-7 through the daemon: a Claude Code agent run in the
/// background goes on after its turn, and what it does between turns —
/// its items and its end, written by the session's writer — reaches the
/// parent's followers and the bridge's chunks as the parent's session's,
/// as its lines in a turn do. Its child's log, whole, is in the chunks,
/// its turn completed last, so a viewer, a move or a rebuild does not show
/// it running forever.
#[tokio::test]
async fn r_sub_6_a_claude_background_agents_lines_between_turns_reach_followers_and_chunks() {
    let w = World::new("claude-bg-kid");
    let a = w.device("machine-a");
    let fixture = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/background_agent.jsonl")).unwrap();
    let hello = "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"msg_hi\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}},\"parent_tool_use_id\":null,\"session_id\":\"{{SESSION}}\"}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"hello\",\"session_id\":\"{{SESSION}}\"}\n---\n";
    let path = w.root.join("background.jsonl");
    std::fs::write(&path, format!("{hello}{fixture}")).unwrap();
    w.serve_with(w.env(), "home", fake_claude(&w, &path));
    let d = w.daemon().await;
    let claude = krowk_harness::protocol::ModelRef { instance: "claude".into(), model: "sonnet".into() };
    let ask = |session: Option<&str>, text: &str| Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model: Some(claude.clone()), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None };
    let run = async |c: Command| {
        let (tx, mut rx) = mpsc::channel(4096);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        d.execute(c, tx).await.unwrap().unwrap()
    };
    let first = run(ask(None, "say hello")).await;
    let session = first.session_id;
    let (_stop, _cp, _bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut watch = d.watch();
    let second = run(ask(Some(&session), "survey the repo in the background")).await;
    assert_eq!(second.status, krowk_harness::protocol::TurnStatus::Completed, "{second:?}");
    let parent = log::read_events(&log::sessions_dir(&w.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
    let kid = parent.iter().find_map(|e| match &e.body {
        krowk_harness::protocol::LogBody::SubagentStarted { subagent_session_id, backend_id: Some(_), .. } => Some(subagent_session_id.clone()),
        _ => None,
    });
    let kid = kid.expect("the background agent is a child");
    // The follower: the child's end, between turns.
    let ended = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match watch.recv().await {
                Ok(StreamLine::Log(e)) if e.session_id == kid && matches!(e.body, krowk_harness::protocol::LogBody::TurnCompleted { .. }) => return e,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("{e}"),
            }
        }
    });
    let ended = ended.await.expect("the parent's follower gets the child's turn.completed");
    assert!(matches!(ended.body, krowk_harness::protocol::LogBody::TurnCompleted { status: krowk_harness::protocol::TurnStatus::Completed, .. }));
    // The chunks: the child's log whole, its end last.
    assert_eq!(log_ids(&w, &kid).last(), Some(&ended.id));
    chunks_hold(&w, &session, &[&kid], Duration::from_secs(30)).await;
}

/// The name of the thread A's daemon runs on in these tests.
const DAEMON_THREAD: &str = "test-daemon";

/// Each `build` call: its session, when, and the thread it ran on.
type Built = Arc<Mutex<Vec<(String, Instant, Option<String>)>>>;

/// How a test's supervisor runs.
struct AutoOpts {
    /// The first `build` panics.
    panic_first: bool,
    /// `KROWK_SYNC_IDLE`, in whole seconds; the default window when None.
    idle: Option<Duration>,
    /// The switch: off as `KROWK_SYNC_AUTO=0` says it, while unset.
    on: Arc<AtomicBool>,
    /// Every `build` refuses with this `(code, message)`.
    refuse: Option<(String, String)>,
    paid_pause: Duration,
    /// How often the policy is read again while sessions are parked.
    parked_recheck: Duration,
}

impl Default for AutoOpts {
    fn default() -> AutoOpts {
        AutoOpts { panic_first: false, idle: None, on: Arc::new(AtomicBool::new(true)), refuse: None, paid_pause: auto::PAID_PAUSE, parked_recheck: auto::PARKED_RECHECK }
    }
}

/// A's daemon with the automatic sync's supervisor beside it (ticket 94):
/// no bridge started by hand.
struct Auto {
    handle: Option<auto::Handle>,
    /// The daemon's `server::run`, on its thread.
    daemon: Option<std::thread::JoinHandle<()>>,
    bridged: Arc<Mutex<BTreeSet<String>>>,
    built: Built,
    /// The daemon's turn hook, which a test may tell of a turn itself.
    turns: mpsc::UnboundedSender<server::TurnEvent>,
    /// Sessions whose checks read the policy: each check starts with it.
    checks: Arc<AtomicUsize>,
}

impl Auto {
    fn built(&self, session: &str) -> Vec<Instant> {
        self.built.lock().unwrap().iter().filter(|(s, ..)| s == session).map(|(_, at, _)| *at).collect()
    }

    /// `Handle::stop`, off the runtime: it blocks.
    async fn stop(&mut self, grace: Duration) {
        let h = self.handle.take().expect("running");
        tokio::task::spawn_blocking(move || h.stop(grace)).await.unwrap();
    }
}

impl World {
    /// A's daemon, hooked, and its supervisor, whose `build` makes A's
    /// bridges as `bridge_with` does — panicking the first time it is
    /// called when `panic_first`.
    fn auto(&self, a: &Device, panic_first: bool) -> Auto {
        self.auto_with(a, AutoOpts { panic_first, ..AutoOpts::default() })
    }

    /// `auto`, as `o` says.
    fn auto_with(&self, a: &Device, o: AutoOpts) -> Auto {
        let AutoOpts { panic_first, idle, on, refuse, paid_pause, parked_recheck } = o;
        let (turns, turns_rx) = mpsc::unbounded_channel();
        let bridged: Arc<Mutex<BTreeSet<String>>> = Arc::default();
        let daemon = self.serve_hooked(self.env(), "home", InstancesConfig::default(), Some(turns.clone()), Some(bridged.clone()));
        let built: Built = Arc::default();
        let (seen, a2, api, relay, keys, chain, sessions) = (built.clone(), a.copy(), self.a_client(a), self.relay_a.clone(), self.user_key(), self.chain(), log::sessions_dir(&self.env()).unwrap());
        let first = AtomicBool::new(panic_first);
        let build: auto::Build = Box::new(move |session: &str, cwd: &Path| {
            seen.lock().unwrap().push((session.to_string(), Instant::now(), std::thread::current().name().map(String::from)));
            if first.swap(false, Ordering::SeqCst) {
                panic!("the test's build panics, once");
            }
            if let Some(no) = &refuse {
                return Err(no.clone());
            }
            Ok(host::Options {
                relay: relay.clone(),
                env: "development".into(),
                api: api.clone(),
                device: a2.key.id(),
                signing: SigningKey::from_secret(&*a2.signing.secret_bytes()).unwrap(),
                keys: UserKeys::new(keys.clone(), []).unwrap(),
                chain: chain.clone(),
                session: session.into(),
                title: "the title is sealed too".into(),
                cwd: cwd.display().to_string(),
                sessions: sessions.clone(),
                ttl: host::LEASE_TTL,
                keep: host::KEEP,
                direct: None,
                tailnet: None,
                handed: None,
                approval_wait: host::APPROVAL_WAIT,
                stale: None,
                supervised: None,
            })
        });
        let env = self.env();
        let checks = Arc::new(AtomicUsize::new(0));
        let counted = checks.clone();
        let c = auto::Config {
            socket: daemon::socket(&env).unwrap(),
            sessions: log::sessions_dir(&env).unwrap(),
            version: "test".into(),
            krowk_home: Some(self.root.join("home/.krowk")),
            build,
            // The switch and the idle window as the environment says them.
            policy: Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let on = on.load(Ordering::SeqCst);
                auto::policy(
                    &|k| match k {
                        "KROWK_SYNC_AUTO" if !on => "0".into(),
                        "KROWK_SYNC_IDLE" => idle.map(|d| d.as_secs().to_string()).unwrap_or_default(),
                        _ => String::new(),
                    },
                    None,
                )
            }),
            ready: Box::new(|| Ok(())),
            bridged: bridged.clone(),
            paid_pause,
            parked_recheck,
        };
        Auto { handle: Some(auto::Supervisor::spawn(c, turns_rx)), daemon: Some(daemon), bridged, built, turns, checks }
    }

    /// Waits until B lists `session`, or panics at `by`.
    async fn listed_on(&self, b: &Device, session: &str, by: Instant) {
        loop {
            let (api, keys, chain) = (self.as_device(b), self.keys(), self.chain());
            let listed = tokio::task::spawn_blocking(move || viewer::list(&api, &keys, &chain)).await.unwrap();
            if listed.as_ref().is_ok_and(|(l, _)| l.iter().any(|s| s.id == session)) {
                return;
            }
            assert!(Instant::now() < by, "B never listed {session}: {:?}", listed.map(|(l, n)| (l.into_iter().map(|s| s.id).collect::<Vec<_>>(), n)));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// A turn of a new session (`None`) or `session`, run to its end by `d`.
async fn prompted(w: &World, d: &Client, session: Option<&str>, text: &str) -> String {
    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    d.execute(w.prompt(session, text), tx).await.unwrap().unwrap().session_id
}

/// R-SYNC-3: a session prompted at A's terminal, with no `krowk sync`
/// command, is bridged by A's supervisor the moment its turn starts and
/// listed on B within 5 seconds; B follows it live (R-SYNC-2: the bridge
/// is its one writer, and B's view is the bridge's). R-LAG-9: the bridge's
/// blocking work — its `build` — runs off the daemon's thread. Stopping the
/// supervisor ends the bridge and gives its lease back: nothing is left
/// bridged and the registry holds no lease.
#[tokio::test]
async fn r_sync_3_a_session_prompted_with_no_sync_command_is_listed_on_b_within_5_seconds() {
    let w = World::new("auto");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let mut auto = w.auto(&a, false);
    let d = w.daemon().await;
    let sent = Instant::now();
    let session = prompted(&w, &d, None, "say hello").await;
    w.listed_on(&b, &session, sent + Duration::from_secs(5)).await;
    println!("R-SYNC-3 listed on B {:?} after the prompt", sent.elapsed());
    assert_eq!(auto.built(&session).len(), 1, "one bridge for the session");
    assert!(auto.bridged.lock().unwrap().contains(&session), "the status names it bridged");
    let on = auto.built.lock().unwrap()[0].2.clone();
    assert_ne!(on.as_deref(), Some(DAEMON_THREAD), "R-LAG-9: never on the daemon's thread");

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    let again = {
        let (d, w2, s) = (d.clone(), w.prompt(Some(&session), "say it again"), session.clone());
        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::channel(1024);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            d.execute(w2, tx).await.unwrap().unwrap();
            s
        })
    };
    let got = until(&mut v, Duration::from_secs(15), &mut frames, result_of).await;
    assert!(got.iter().any(|u| format!("{u:?}").contains("say it again")), "the second turn's own result reached B: {got:?}");
    again.await.unwrap();
    assert_eq!(auto.built(&session).len(), 1, "a second turn rides the same bridge");

    auto.stop(Duration::from_secs(10)).await;
    assert!(auto.bridged.lock().unwrap().is_empty(), "nothing bridged after stop");
    let (api, id) = (w.client(), session.clone());
    let s = tokio::task::spawn_blocking(move || api.show_sync_session(&id)).await.unwrap().unwrap();
    assert!(s.lease.is_none(), "the stopped bridge gave its lease back: {:?}", s.lease);
}

/// R-SYNC-3: one bridge's panic is its session's alone. The first `build`
/// (session X's) panics; session Y, prompted after it, is bridged and
/// listed on B all the same, and X is built again once its backoff has
/// run out, and listed too.
#[tokio::test]
async fn r_sync_3_one_bridges_panic_leaves_the_others() {
    let w = World::new("auto-panic");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let mut auto = w.auto(&a, true);
    let d = w.daemon().await;
    let x = prompted(&w, &d, None, "say hello").await;
    // X's build first, so the panic is X's.
    let deadline = Instant::now() + Duration::from_secs(10);
    while auto.built(&x).is_empty() {
        assert!(Instant::now() < deadline, "X was never built");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let y = prompted(&w, &d, None, "say hello").await;
    w.listed_on(&b, &y, Instant::now() + Duration::from_secs(10)).await;
    let by = Instant::now() + auto::BACKOFF_FIRST + Duration::from_secs(10);
    w.listed_on(&b, &x, by).await;
    let calls = auto.built(&x);
    assert_eq!(calls.len(), 2, "built once, panicked, and built again: {calls:?}");
    assert!(calls[1] - calls[0] >= auto::BACKOFF_FIRST - Duration::from_millis(100), "again only after its backoff: {:?}", calls[1] - calls[0]);
    assert_eq!(auto.built(&y).len(), 1);
    auto.stop(Duration::from_secs(10)).await;
    assert!(auto.bridged.lock().unwrap().is_empty());
}

/// R-SYNC-3: a subagent's session rides its parent's bridge and a session
/// in a directory with `.krowk/no-sync` stays on the machine — neither gets
/// a `build` call. A plain session's turn after them is bridged.
#[tokio::test]
async fn r_sync_3_a_child_session_and_an_opted_out_directory_are_never_built() {
    let w = World::new("auto-out");
    let a = w.device("machine-a");
    let mut auto = w.auto(&a, false);

    // A turn in a directory that opts out.
    let private = w.root.join("private");
    std::fs::create_dir_all(private.join(".krowk")).unwrap();
    std::fs::write(private.join(".krowk/no-sync"), "").unwrap();
    let there = Arc::new(Client::connect(&daemon::socket(&w.env()).unwrap(), &private, "test", true).await.ok().expect("the daemon answers"));
    let out = prompted(&w, &there, None, "say hello").await;

    // A child's turn, as the hook would tell of one: its log names a parent.
    let (parent, kid) = ("01a0ec7b-0000-7000-8000-0000000000e1", "01a0ec7b-0000-7000-8000-0000000000e2");
    let dir = log::sessions_dir(&w.env()).unwrap().join(kid);
    std::fs::create_dir_all(&dir).unwrap();
    let started = krowk_harness::protocol::LogEvent {
        id: "01a0ec7b-0000-7000-8000-0000000000e3".into(),
        parent_id: None,
        session_id: kid.into(),
        time_ms: 1,
        body: krowk_harness::protocol::LogBody::SessionStarted { cwd: w.repo().display().to_string(), krowk_version: "test".into(), protocol_version: 1, parent_session_id: Some(parent.into()), agent: None },
    };
    std::fs::write(dir.join(log::EVENTS_FILE), format!("{}\n", serde_json::to_string(&started).unwrap())).unwrap();
    auto.turns.send(server::TurnEvent { session: kid.into(), running: true }).unwrap();

    // A plain session after both: built.
    let d = w.daemon().await;
    let plain = prompted(&w, &d, None, "say hello").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while auto.built(&plain).is_empty() || auto.checks.load(Ordering::SeqCst) < 3 {
        assert!(Instant::now() < deadline, "the plain session was never built, or the others never checked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Their checks have begun; what is left of them is a log line and a few stats.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(auto.built(&out).is_empty(), "an opted-out directory's session is never built");
    assert!(auto.built(kid).is_empty(), "a child's session is never built");
    assert!(!auto.bridged.lock().unwrap().contains(&out));
    auto.stop(Duration::from_secs(10)).await;
}

/// Whether the registry holds a lease on `session` now; its status when it
/// does not answer with the session (404: it never heard of it).
async fn leased(w: &World, session: &str) -> Result<bool, u16> {
    let (api, id) = (w.client(), session.to_string());
    tokio::task::spawn_blocking(move || api.show_sync_session(&id)).await.unwrap().map(|s| s.lease.is_some()).map_err(|e| e.status)
}

/// Waits until `session` is bridged and leased, or panics at `by`.
async fn taken_up(w: &World, auto: &Auto, session: &str, by: Instant) {
    while !(auto.bridged.lock().unwrap().contains(session) && leased(w, session).await == Ok(true)) {
        assert!(Instant::now() < by, "{session} was never taken up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until `session` is bridged no more and its lease given back, or
/// panics at `by`; when it was.
async fn let_go(w: &World, auto: &Auto, session: &str, by: Instant) -> Instant {
    loop {
        let lease = leased(w, session).await;
        if lease == Ok(false) && !auto.bridged.lock().unwrap().contains(session) {
            return Instant::now();
        }
        assert!(Instant::now() < by, "{session} was never let go: lease {lease:?}, bridged {:?}", auto.bridged.lock().unwrap());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until the supervisor has read its policy `n` times, then a moment
/// for what follows from it.
async fn checked(auto: &Auto, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while auto.checks.load(Ordering::SeqCst) < n {
        assert!(Instant::now() < deadline, "the supervisor never checked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// R-SYNC-3, R-SYNC-1: a session in a repository with `.krowk/no-sync` runs
/// whole turns and never leaves the machine — A's registry proxy hands on no
/// request naming it, A opens no relay connection, and the registry has
/// never heard of it.
#[tokio::test]
async fn r_sync_3_an_opted_out_session_reaches_neither_the_registry_nor_the_relay() {
    let w = World::new("auto-private");
    let a = w.device("machine-a");
    std::fs::create_dir_all(w.repo().join(".krowk")).unwrap();
    std::fs::write(w.repo().join(".krowk/no-sync"), "").unwrap();
    let mut auto = w.auto(&a, false);
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    prompted(&w, &d, Some(&session), "say it again").await;
    checked(&auto, 2).await;
    assert!(auto.built(&session).is_empty(), "never built");
    let named: Vec<String> = w.reg_log.lock().unwrap().iter().filter(|(line, _)| line.contains(&session)).map(|(line, _)| line.clone()).collect();
    assert!(named.is_empty(), "no registry request names the session: {named:?}");
    assert_eq!(w.a_conns.load(Ordering::SeqCst), 0, "no relay connection");
    assert_eq!(leased(&w, &session).await, Err(404), "the registry never heard of it");
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-3: with a 2-second idle window, a bridge whose turn has ended is
/// let go — its stream sealed and its lease given back — within 5 seconds,
/// and not before the window. A turn longer than the window is never let
/// go while it runs. The next prompt takes the session up again, its lease
/// held again. R-OFF-2: a session let go is synced again by its next turn,
/// with no command.
#[tokio::test]
async fn r_sync_3_an_idle_bridge_lets_go_and_the_next_prompt_takes_it_up_again() {
    let w = World::new("auto-idle");
    let a = w.device("machine-a");
    let mut auto = w.auto_with(&a, AutoOpts { idle: Some(Duration::from_secs(2)), ..AutoOpts::default() });
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    let ended = Instant::now();
    w.synced(&session).await;
    let gone = let_go(&w, &auto, &session, ended + Duration::from_secs(5)).await;
    assert!(gone - ended >= Duration::from_millis(1500), "not before the window: {:?}", gone - ended);
    println!("R-SYNC-3 let go {:?} after its turn ended", gone - ended);
    assert!(auto.bridged.lock().unwrap().is_empty());

    // A turn longer than the window: taken up again, and held throughout.
    let long = {
        let (d, p) = (d.clone(), w.prompt(Some(&session), "a long answer"));
        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::channel(1024);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            d.execute(p, tx).await.unwrap().unwrap();
        })
    };
    let started = Instant::now();
    taken_up(&w, &auto, &session, started + Duration::from_secs(10)).await;
    while !long.is_finished() {
        assert!(auto.bridged.lock().unwrap().contains(&session), "let go mid-turn, {:?} into it", started.elapsed());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    long.await.unwrap();
    let ran = started.elapsed();
    assert!(ran > Duration::from_secs(3), "the turn outlasted the window: {ran:?}");
    assert_eq!(leased(&w, &session).await, Ok(true), "held as the long turn ended");
    assert_eq!(auto.built(&session).len(), 2, "taken up once more, by the prompt");
    let_go(&w, &auto, &session, Instant::now() + Duration::from_secs(5)).await;
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-3: a session bridged, then its repository opted out with
/// `.krowk/no-sync`: its next turn stops the bridge and gives the lease
/// back, and while the opt-out stands no turn bridges it again.
#[tokio::test]
async fn r_sync_3_a_directory_opted_out_while_bridged_stops_and_releases() {
    let w = World::new("auto-optout");
    let a = w.device("machine-a");
    let mut auto = w.auto(&a, false);
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    w.synced(&session).await;
    std::fs::create_dir_all(w.repo().join(".krowk")).unwrap();
    std::fs::write(w.repo().join(".krowk/no-sync"), "").unwrap();
    let sent = Instant::now();
    prompted(&w, &d, Some(&session), "say it again").await;
    let gone = let_go(&w, &auto, &session, sent + Duration::from_secs(10)).await;
    println!("R-SYNC-3 stopped {:?} after the prompt", gone - sent);
    let checks = auto.checks.load(Ordering::SeqCst);
    prompted(&w, &d, Some(&session), "say it once more").await;
    checked(&auto, checks + 1).await;
    assert_eq!(auto.built(&session).len(), 1, "not bridged again while the opt-out stands");
    assert_eq!(leased(&w, &session).await, Ok(false));
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-3: automatic sync turned off while two sessions are bridged
/// (`KROWK_SYNC_AUTO=0` here): the next turn of either stops every bridge,
/// each giving its lease back, and no turn bridges one again while it is off.
#[tokio::test]
async fn r_sync_3_sync_turned_off_while_bridged_stops_every_bridge() {
    let w = World::new("auto-off");
    let a = w.device("machine-a");
    let on = Arc::new(AtomicBool::new(true));
    let mut auto = w.auto_with(&a, AutoOpts { on: on.clone(), ..AutoOpts::default() });
    let d = w.daemon().await;
    let (x, y) = (prompted(&w, &d, None, "say hello").await, prompted(&w, &d, None, "say hello").await);
    w.synced(&x).await;
    w.synced(&y).await;
    on.store(false, Ordering::SeqCst);
    let sent = Instant::now();
    prompted(&w, &d, Some(&x), "say it again").await;
    let_go(&w, &auto, &x, sent + Duration::from_secs(10)).await;
    let_go(&w, &auto, &y, sent + Duration::from_secs(10)).await;
    let checks = auto.checks.load(Ordering::SeqCst);
    prompted(&w, &d, Some(&y), "say it again").await;
    checked(&auto, checks + 1).await;
    assert_eq!((auto.built(&x).len(), auto.built(&y).len()), (1, 1), "neither bridged again while off");
    assert!(auto.bridged.lock().unwrap().is_empty());
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-3, R-HAND-4 (ticket 116): with a 3-second idle window, a session
/// B watches — a viewer that has said hello, so one that can prompt — is
/// never let go, however long B watches with nothing running. Once B goes,
/// the window runs again from then, and the session is let go after it.
#[tokio::test]
async fn r_sync_3_a_session_a_device_watches_is_not_let_go_at_its_idle_deadline() {
    let w = World::new("auto-watched");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let mut auto = w.auto_with(&a, AutoOpts { idle: Some(Duration::from_secs(3)), ..AutoOpts::default() });
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    // Let go before B came, B's coming takes it up again: either way it is
    // held from here.
    taken_up(&w, &auto, &session, Instant::now() + Duration::from_secs(10)).await;
    let builds = auto.built(&session).len();
    let watched = Instant::now();
    while watched.elapsed() < Duration::from_secs(7) {
        assert!(auto.bridged.lock().unwrap().contains(&session), "let go {:?} into B's watch", watched.elapsed());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(leased(&w, &session).await, Ok(true), "held while B watched");
    assert_eq!(auto.built(&session).len(), builds, "one bridge throughout");

    drop(v);
    let left = Instant::now();
    let gone = let_go(&w, &auto, &session, left + Duration::from_secs(9)).await;
    assert!(gone - left >= Duration::from_millis(2500), "a whole window after B left: {:?}", gone - left);
    println!("R-SYNC-3 let go {:?} after B left", gone - left);
    auto.stop(Duration::from_secs(10)).await;
}

/// The turns `session`'s log on A holds.
fn turns_logged(w: &World, session: &str) -> usize {
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(session).join(log::EVENTS_FILE)).unwrap();
    log.lines().filter(|l| l.contains("\"turn.started\"")).count()
}

/// R-HAND-4, R-SYNC-3 (ticket 116): a session let go as idle keeps its
/// relay link, parked. B opens it and prompts at once: B's joining takes
/// the session up again, with no turn at A, and the prompt — typed before
/// the bridge was back, held by B until the new bridge welcomed it — runs
/// once on A and is answered on B. Let go again once B has gone, B coming
/// back takes it up again.
#[tokio::test]
async fn r_hand_4_a_prompt_from_b_takes_a_session_let_go_up_again_and_is_answered() {
    let w = World::new("auto-wake");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    // The window as long as the bridge woken needs to take the lease up,
    // join the relay and see B's hello, with room to spare.
    let mut auto = w.auto_with(&a, AutoOpts { idle: Some(Duration::from_secs(3)), ..AutoOpts::default() });
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    let ended = Instant::now();
    w.synced(&session).await;
    let_go(&w, &auto, &session, ended + Duration::from_secs(6)).await;
    assert_eq!(auto.built(&session).len(), 1);

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let sent = Instant::now();
    v.commands.send(w.prompt(Some(&session), "asked from B once A had let it go")).unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(20), &mut frames, result_of).await;
    println!("R-HAND-4 B's prompt answered {:?} after it was sent", sent.elapsed());
    assert_eq!(auto.built(&session).len(), 2, "taken up again by B, once");
    let log = std::fs::read_to_string(log::sessions_dir(&w.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
    assert!(log.contains("asked from B once A had let it go"), "B's prompt ran on A");
    assert_eq!(turns_logged(&w, &session), 2, "and once");
    assert_eq!(leased(&w, &session).await, Ok(true));

    drop(v);
    let_go(&w, &auto, &session, Instant::now() + Duration::from_secs(9)).await;
    let v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    taken_up(&w, &auto, &session, Instant::now() + Duration::from_secs(10)).await;
    assert_eq!(auto.built(&session).len(), 3);
    drop(v);
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-3 (ticket 116): a viewer never syncs a session the policy
/// refuses. A session let go as idle, then opted out with
/// `.krowk/no-sync`: B opening it and prompting has the supervisor read the
/// policy again, which refuses it — it is never built again nor leased,
/// its parked link is let go so B sees no host, and B's prompt never runs.
#[tokio::test]
async fn r_sync_3_a_session_opted_out_once_let_go_is_never_brought_back_by_a_viewer() {
    let w = World::new("auto-wake-out");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let mut auto = w.auto_with(&a, AutoOpts { idle: Some(Duration::from_secs(2)), ..AutoOpts::default() });
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    let ended = Instant::now();
    w.synced(&session).await;
    let_go(&w, &auto, &session, ended + Duration::from_secs(5)).await;
    std::fs::create_dir_all(w.repo().join(".krowk")).unwrap();
    std::fs::write(w.repo().join(".krowk/no-sync"), "").unwrap();

    let checks = auto.checks.load(Ordering::SeqCst);
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    v.commands.send(w.prompt(Some(&session), "asked of a session opted out")).unwrap();
    checked(&auto, checks + 1).await;
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(false))).await;
    assert_eq!(auto.built(&session).len(), 1, "never built again");
    assert!(auto.bridged.lock().unwrap().is_empty());
    assert_eq!(leased(&w, &session).await, Ok(false), "never leased again");
    assert_eq!(turns_logged(&w, &session), 1, "B's prompt never ran");
    drop(v);
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-3 (ticket 116): automatic sync turned off while a session is
/// parked, and no turn or viewer to ask for the policy: it is read again on
/// its own (`Config::parked_recheck`, a second here, on the next beat), and
/// the parked link let go. B opening the session then finds no host, and
/// nothing reads the policy again: a link still parked would have been
/// woken by B, and its wake checked. Nothing is built.
#[tokio::test]
async fn r_sync_3_sync_turned_off_lets_a_parked_session_off_the_relay() {
    let w = World::new("auto-park-off");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let on = Arc::new(AtomicBool::new(true));
    let mut auto = w.auto_with(&a, AutoOpts { idle: Some(Duration::from_secs(2)), on: on.clone(), parked_recheck: Duration::from_secs(1), ..AutoOpts::default() });
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    let ended = Instant::now();
    w.synced(&session).await;
    let_go(&w, &auto, &session, ended + Duration::from_secs(5)).await;
    on.store(false, Ordering::SeqCst);
    let checks = auto.checks.load(Ordering::SeqCst);
    let deadline = Instant::now() + krowk_harness::sync::PING + Duration::from_secs(5);
    while auto.checks.load(Ordering::SeqCst) == checks {
        assert!(Instant::now() < deadline, "the policy was never read again while the session was parked");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The parked link let go by that read, with no viewer near: A has no
    // connection to its relay left.
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.a_live.load(Ordering::SeqCst) > 0 {
        assert!(Instant::now() < deadline, "the parked link stayed on the relay with sync off");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let read = auto.checks.load(Ordering::SeqCst);
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(false))).await;
    // A wake's check is asked for after its link has gone: a moment for one.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(auto.checks.load(Ordering::SeqCst), read, "B woke nothing: the link was let go by the policy read");
    assert_eq!(auto.built(&session).len(), 1, "never built again");
    assert_eq!(leased(&w, &session).await, Ok(false));
    drop(v);
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-3 (ticket 116): a viewer keeping a session with no turn does not
/// keep it from the policy. Sync turned off while B watches: the policy is
/// read again on its own (`Config::parked_recheck`, on the next beat), and
/// the bridge stops and gives its lease back, B still there. Nothing parks.
#[tokio::test]
async fn r_sync_3_sync_turned_off_stops_a_session_a_device_watches() {
    let w = World::new("auto-watch-off");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let on = Arc::new(AtomicBool::new(true));
    let mut auto = w.auto_with(&a, AutoOpts { idle: Some(Duration::from_secs(3)), on: on.clone(), parked_recheck: Duration::from_secs(1), ..AutoOpts::default() });
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    taken_up(&w, &auto, &session, Instant::now() + Duration::from_secs(10)).await;
    on.store(false, Ordering::SeqCst);
    let off = Instant::now();
    let_go(&w, &auto, &session, off + krowk_harness::sync::PING + Duration::from_secs(5)).await;
    println!("R-SYNC-3 a watched session stopped {:?} after sync was turned off", off.elapsed());
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.a_live.load(Ordering::SeqCst) > 0 {
        assert!(Instant::now() < deadline, "its relay link stayed, parked, with sync off");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(v);
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-2: one writer. A manual `krowk sync host` holds the session; two
/// prompts follow with automatic sync on. The supervisor tries once — the
/// registry refuses it the lease (`lease_held`) — and leaves the session
/// alone for a minute: `build` is called at most once, the lease's fence
/// never moves, and the manual bridge keeps streaming the turns to B.
#[tokio::test]
async fn r_sync_2_a_manual_host_keeps_its_lease_and_auto_sync_stays_off_it() {
    let w = World::new("auto-manual");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let on = Arc::new(AtomicBool::new(false));
    let mut auto = w.auto_with(&a, AutoOpts { on: on.clone(), ..AutoOpts::default() });
    let d = w.daemon().await;
    // The session made with automatic sync off, then hosted by hand.
    let session = prompted(&w, &d, None, "say hello").await;
    checked(&auto, 1).await;
    assert!(auto.built(&session).is_empty());
    let (stop, _cp, manual) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let fences = || w.reg_log.lock().unwrap().iter().filter_map(|(_, f)| *f).collect::<Vec<u64>>();
    let held = *fences().last().expect("the manual bridge's lease named its fence");
    on.store(true, Ordering::SeqCst);

    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    let t0 = Instant::now();
    for text in ["say it again", "say it once more"] {
        let turn = {
            let (d, p) = (d.clone(), w.prompt(Some(&session), text));
            tokio::spawn(async move {
                let (tx, mut rx) = mpsc::channel(1024);
                tokio::spawn(async move { while rx.recv().await.is_some() {} });
                d.execute(p, tx).await.unwrap().unwrap();
            })
        };
        let mut seen = false;
        until(&mut v, Duration::from_secs(15), &mut frames, |u| {
            seen |= format!("{u:?}").contains(text);
            seen && result_of(u)
        })
        .await;
        turn.await.unwrap();
    }
    // The supervisor's one try has been refused by now.
    let deadline = Instant::now() + Duration::from_secs(10);
    while auto.built(&session).is_empty() || auto.bridged.lock().unwrap().contains(&session) {
        assert!(Instant::now() < deadline, "the supervisor never tried, or its try never ended");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(t0.elapsed() < auto::LEASE_HELD_WAIT);
    assert_eq!(auto.built(&session).len(), 1, "build called once in a minute");
    assert!(fences().iter().all(|f| *f == held), "the fence never moved from {held}: {:?}", fences());
    assert!(!manual.is_finished(), "the manual bridge still runs");
    assert_eq!(leased(&w, &session).await, Ok(true));
    stop.send(true).unwrap();
    manual.await.unwrap().unwrap();
    auto.stop(Duration::from_secs(10)).await;
}

/// R-SYNC-1: a workspace without a paid plan. The first `build` is refused
/// with `sync_requires_paid_plan`; for the pause that follows (an hour,
/// shortened here through `Config::paid_pause`) no turn of any session is
/// built, and nothing reaches the relay. After it, the next turn asks again.
#[tokio::test]
async fn r_sync_1_a_free_workspace_stops_auto_sync_for_an_hour() {
    assert_eq!(auto::PAID_PAUSE, Duration::from_secs(60 * 60));
    let w = World::new("auto-free");
    let a = w.device("machine-a");
    let pause = Duration::from_secs(6);
    let refuse = ("sync_requires_paid_plan".to_string(), "syncing sessions between devices needs a paid plan — upgrade this workspace to Pro at https://app.krowk.com".to_string());
    let mut auto = w.auto_with(&a, AutoOpts { refuse: Some(refuse), paid_pause: pause, ..AutoOpts::default() });
    let d = w.daemon().await;
    let first = prompted(&w, &d, None, "say hello").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while auto.built(&first).is_empty() {
        assert!(Instant::now() < deadline, "never built");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let refused = Instant::now();
    tokio::time::sleep(Duration::from_millis(300)).await;
    prompted(&w, &d, Some(&first), "say it again").await;
    let other = prompted(&w, &d, None, "say hello").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(refused.elapsed() < pause, "the test ran past its pause");
    assert_eq!(auto.built.lock().unwrap().len(), 1, "no build while paused");
    assert_eq!(w.a_conns.load(Ordering::SeqCst), 0, "nothing sent to the relay");

    tokio::time::sleep((refused + pause).saturating_duration_since(Instant::now()) + Duration::from_millis(500)).await;
    prompted(&w, &d, Some(&other), "say it again").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while auto.built(&other).is_empty() {
        assert!(Instant::now() < deadline, "the next turn after the pause never asked again");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    auto.stop(Duration::from_secs(10)).await;
}

/// What the stand-in registry recorded as notified about `session` (ticket
/// 103's `GET /_notifications`, which only the stand-in has), oldest first:
/// each one's kind and dedup. A repeat of a dedup is not recorded again.
fn notifications(w: &World, session: &str) -> Vec<(String, String)> {
    let mut c = TcpStream::connect(w.registry.addr()).unwrap();
    c.write_all(b"GET /_notifications HTTP/1.1\r\nHost: registry\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = Vec::new();
    c.read_to_end(&mut answer).unwrap();
    let body = &answer[answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
    let seen: serde_json::Value = serde_json::from_slice(body).unwrap();
    seen["notifications"].as_array().unwrap().iter().filter(|n| n["session"] == session).map(|n| (n["kind"].as_str().unwrap().to_string(), n["dedup"].as_str().unwrap().to_string())).collect()
}

/// The notifications A's bridge asked the registry for, through A's proxy,
/// repeats and refusals and all; `asked_to_notify` with `how` "HELD POST ",
/// those the proxy began to hold.
fn notify_calls(w: &World, session: &str) -> usize {
    asked_to_notify(w, session, "POST ")
}

fn asked_to_notify(w: &World, session: &str, how: &str) -> usize {
    w.reg_log.lock().unwrap().iter().filter(|(line, _)| line.starts_with(how) && line.contains(session) && line.contains("/notification ")).count()
}

/// Waits until the stand-in has recorded `n` notifications of `kind` about
/// `session`, or panics after `wait`; what it holds then.
async fn notified(w: &World, session: &str, kind: &str, n: usize, wait: Duration) -> Vec<(String, String)> {
    let deadline = Instant::now() + wait;
    loop {
        let seen = notifications(w, session);
        if seen.iter().filter(|(k, _)| k == kind).count() >= n {
            return seen;
        }
        assert!(Instant::now() < deadline, "never {n} {kind}: {seen:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The SHA-256 of `key`, in hex: the dedup the registry is sent.
fn digest(key: &str) -> String {
    use sha2::Digest;
    e2e::hex(&sha2::Sha256::digest(key.as_bytes()))
}

/// R-PERM-2, R-IOS-12, R-E2E-1: an approval on a synced session rings the
/// person's devices once, as it starts waiting: the registry is told the
/// kind and a digest of the request's id, and nothing of what it asks. A
/// viewer that attaches again is welcomed with the request, and the bridge
/// asks for nothing; a bridge started again while it waits asks once more
/// under the same key (it holds no request yet), which the registry
/// collapses: the person's devices still rang once.
#[tokio::test]
async fn r_perm_2_an_approval_on_a_synced_session_rings_the_persons_devices_once() {
    let w = World::new("ring-approval");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (_d, session) = first_turn(&w).await;
    let (stop, _cp, bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    v.commands.send(w.prompt(Some(&session), &format!("make the file {PROMPT_MARKER}"))).unwrap();
    let got = until(&mut v, Duration::from_secs(15), &mut frames, |u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(_))))).await;
    let req = got.iter().find_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) = u { Some(r.clone()) } else { None }).unwrap();
    let seen = notified(&w, &session, "approval", 1, Duration::from_secs(10)).await;
    let approvals: Vec<_> = seen.iter().filter(|(k, _)| k == "approval").collect();
    assert_eq!(approvals, [&("approval".to_string(), digest(&format!("approval:{}", req.request_id)))], "one, its dedup a digest of the request's id: {seen:?}");

    // B goes and comes back: welcomed with the request still waiting, which
    // rings nothing more.
    drop(v);
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let welcomed = until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    assert!(welcomed.iter().any(|u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) if r.request_id == req.request_id)), "welcomed with the request: {welcomed:?}");
    let calls = notify_calls(&w, &session);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(notify_calls(&w, &session), calls, "a viewer attaching again asks for nothing");

    // A's bridge starts again while the request waits: the daemon sends the
    // new one the request, and its ask collapses at the registry.
    stop.send(true).unwrap();
    bridge.await.unwrap().unwrap();
    let (_stop, _cp, bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while notify_calls(&w, &session) == calls {
        assert!(Instant::now() < deadline, "the new bridge never saw the request waiting");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(notifications(&w, &session).iter().filter(|(k, _)| k == "approval").count(), 1, "rang once");
    drop(v);
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    v.commands.send(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() }).unwrap();
    until(&mut v, Duration::from_secs(15), &mut frames, result_of).await;
    assert!(w.repo().join("approved.txt").exists(), "B's answer unblocked A's turn");
    let seen = notified(&w, &session, "done", 2, Duration::from_secs(10)).await;
    assert_eq!(seen.iter().filter(|(k, _)| k == "approval").count(), 1, "{seen:?}");
    assert!(!bridge.is_finished());
}

/// R-IOS-12, R-SYNC-2: a finished turn rings once — the session's own,
/// asked by the lease holder — and a bridge that takes the session up again
/// after a restart, the daemon replaying the turns the registry already
/// holds, asks for nothing more.
#[tokio::test]
async fn r_ios_12_a_finished_turn_rings_once_and_taking_the_session_up_again_after_a_restart_rings_nothing_more() {
    let w = World::new("ring-done");
    let a = w.device("machine-a");
    let (d, session) = first_turn(&w).await;
    let (stop, _cp, bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    // The first turn, ended moments before the bridge took the session up,
    // is recent and the registry did not hold it: it rings.
    notified(&w, &session, "done", 1, Duration::from_secs(10)).await;
    turn(&d, &w, &session, "a second turn").await;
    let seen = notified(&w, &session, "done", 2, Duration::from_secs(10)).await;
    let turns: Vec<String> = log::read_events(&log::sessions_dir(&w.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap().into_iter().filter_map(|e| if let LogBody::TurnCompleted { turn_id, .. } = e.body { Some(digest(&format!("turn:{turn_id}"))) } else { None }).collect();
    assert_eq!(seen.iter().map(|(_, d)| d.clone()).collect::<Vec<_>>(), turns, "once per turn, in order");
    stored_whole(&w, &session, Duration::from_secs(15)).await;
    let calls = notify_calls(&w, &session);
    assert_eq!(calls, 2, "each asked once");
    stop.send(true).unwrap();
    bridge.await.unwrap().unwrap();

    let (stop, _cp, bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    // The daemon replays the log before it sends a turn begun after: once
    // the next turn has rung, the replay has been through the bridge.
    turn(&d, &w, &session, "a turn after the restart").await;
    notified(&w, &session, "done", 3, Duration::from_secs(10)).await;
    assert_eq!(notify_calls(&w, &session), calls + 1, "the replayed turns ask for nothing");
    assert_eq!(notifications(&w, &session).len(), 3);
    stop.send(true).unwrap();
    bridge.await.unwrap().unwrap();
}

/// R-IOS-12, R-OFF-2: a notification never holds up the bridge, and one the
/// registry refuses leaves it running. With the registry refusing it
/// (`push_unconfigured`), a turn's batches still reach B on time and the
/// bridge streams on; with it stalling on the next, the same; once the
/// registry answers again, what it stalled on rings, and so does the turn
/// after. The refused one never does.
///
/// A notification that blocked the bridge would block this test's one
/// thread too, held: a watchdog of its own fails it rather than let it hang.
#[tokio::test]
async fn r_ios_12_a_notify_the_registry_refuses_leaves_the_bridge_running() {
    // Disarmed however the test ends, a failed assertion too, so only a
    // hang ends the process; said past the test's captured output.
    struct Disarm(Arc<AtomicBool>);
    impl Drop for Disarm {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let finished = Arc::new(AtomicBool::new(false));
    let _disarm = Disarm(finished.clone());
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(90));
        if !finished.load(Ordering::SeqCst) {
            let _ = writeln!(std::io::stderr(), "r_ios_12_a_notify_the_registry_refuses_leaves_the_bridge_running hung: a notification held the bridge");
            std::process::exit(101);
        }
    });
    let w = World::new("ring-refused");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let (d, session) = first_turn(&w).await;
    let (_stop, _cp, bridge) = w.bridge(&a, &session, w.daemon().await);
    w.synced(&session).await;
    notified(&w, &session, "done", 1, Duration::from_secs(10)).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await;
    let ended = |u: &viewer::Update| matches!(u, viewer::Update::Line(StreamLine::Log(e)) if matches!(e.body, LogBody::TurnCompleted { .. }));

    for mode in [REFUSE_NOTIFY, HOLD_NOTIFY] {
        w.reg_mode.store(mode, Ordering::SeqCst);
        let how = if mode == HOLD_NOTIFY { "HELD POST " } else { "POST " };
        let calls = asked_to_notify(&w, &session, how);
        turn(&d, &w, &session, "a turn the registry will not ring for").await;
        let done = Instant::now();
        until(&mut v, Duration::from_secs(5), &mut frames, ended).await;
        assert!(done.elapsed() < Duration::from_secs(3), "mode {mode}: the turn's end reached B {:?} after it ended", done.elapsed());
        assert!(!bridge.is_finished(), "mode {mode}: the bridge runs on");
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while asked_to_notify(&w, &session, how) == calls {
                assert!(Instant::now() < deadline, "mode {mode}: the bridge never asked");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    w.reg_mode.store(PASS, Ordering::SeqCst);
    // What was held goes on its way now, and rings.
    notified(&w, &session, "done", 2, Duration::from_secs(10)).await;
    turn(&d, &w, &session, "a turn once the registry rings again").await;
    until(&mut v, Duration::from_secs(5), &mut frames, ended).await;
    let seen = notified(&w, &session, "done", 3, Duration::from_secs(10)).await;
    assert_eq!(seen.len(), 3, "the refused one never rang: {seen:?}");
    // One ask a turn, the held one handed on once: `push_unconfigured` is
    // tried once, never again under a mode that would let it ring.
    let asked: Vec<String> = w.reg_log.lock().unwrap().iter().filter(|(l, _)| l.contains("/notification ")).map(|(l, _)| l.clone()).collect();
    assert_eq!(notify_calls(&w, &session), 4, "asked once a turn, the refusal never again: {asked:?}");
    assert!(!bridge.is_finished(), "the bridge runs on");
}

impl Auto {
    /// A's daemon stopped as `krowk host stop` stops it — asked by a client,
    /// its socket gone, `server::run` returned — and then its supervisor,
    /// as `krowk host serve` stops it after `server::run`: every bridge
    /// seals its stream and gives its lease back.
    async fn stop_daemon(&mut self, w: &World, d: &Client) {
        d.stop(true).await.expect("the daemon stops");
        let socket = daemon::socket(&w.env()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while socket.exists() {
            assert!(Instant::now() < deadline, "the daemon's socket never went");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let served = self.daemon.take().expect("a daemon");
        tokio::task::spawn_blocking(move || served.join()).await.unwrap().expect("server::run returned");
        self.stop(krowk_harness::host::SHUTDOWN_GRACE).await;
    }
}

/// The host streams the relay named, in the order they first came: a
/// relay's `joined` names the stream to the host and to each viewer, in the
/// clear, on the relay's side of A's and B's proxies.
fn streams(w: &World) -> Vec<String> {
    let seen = String::from_utf8_lossy(&w.captured()).into_owned();
    let mut found: Vec<String> = Vec::new();
    for (at, _) in seen.match_indices("\"stream\":\"") {
        let hex: String = seen[at + "\"stream\":\"".len()..].chars().take(32).collect();
        if hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()) && !found.contains(&hex) {
            found.push(hex);
        }
    }
    found
}

/// Reads B's updates until every event of A's log of `session` is among
/// `ids`, or panics after `wait`.
async fn caught_up_with(w: &World, v: &mut viewer::Viewer, session: &str, ids: &mut Vec<String>, wait: Duration) {
    let want = log_ids(w, session);
    let deadline = Instant::now() + wait;
    while want.iter().any(|i| !ids.contains(i)) && Instant::now() < deadline {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(500), v.updates.recv()).await {
            ids.extend(batch.iter().flat_map(logged));
        }
    }
    let missing: Vec<_> = want.iter().filter(|i| !ids.contains(i)).collect();
    assert!(missing.is_empty(), "B never got {missing:?}");
}

/// R-OFF-2, R-SYNC-3, R-SYNC-2 (ticket 98): a restart needs nothing of its
/// own. B is attached to a session A's supervisor bridged; A's daemon
/// stops — `krowk host stop`, an upgrade — and B sees the old stream end,
/// the lease given back. A new daemon and supervisor start on the same
/// machine, and the session's next prompt takes it up again with no
/// command: B, still attached, is told to join the new stream and shows the
/// prompt's result, on a stream other than the first, every logged event of
/// the session applied once.
#[tokio::test]
async fn r_off_2_a_restarted_daemon_takes_its_sessions_up_again_on_the_next_prompt() {
    let w = World::new("restart");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let mut auto = w.auto(&a, false);
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    w.synced(&session).await;
    let mut v = viewer::attach(w.viewer(&b, &session)).await.unwrap();
    let mut frames = Vec::new();
    let mut ids: Vec<String> = until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(true))).await.iter().flat_map(logged).collect();
    let before = streams(&w);
    assert!(!before.is_empty(), "the relay named the first stream");

    auto.stop_daemon(&w, &d).await;
    drop(d);
    // The final sealed batch: B knows the stream ended, not that it was cut.
    ids.extend(until(&mut v, Duration::from_secs(10), &mut frames, |u| matches!(u, viewer::Update::Host(false))).await.iter().flat_map(logged));
    assert_eq!(leased(&w, &session).await, Ok(false), "the stopped daemon's bridge gave its lease back");
    assert!(auto.bridged.lock().unwrap().is_empty());

    // The next daemon, on the same machine, and a prompt typed at A.
    let mut again = w.auto(&a, false);
    let d = w.daemon().await;
    let sent = Instant::now();
    let turn = {
        let (d, p) = (d.clone(), w.prompt(Some(&session), "a long answer after the restart"));
        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::channel(4096);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            d.execute(p, tx).await.unwrap().unwrap();
        })
    };
    let got = until(&mut v, Duration::from_secs(30), &mut frames, result_of).await;
    ids.extend(got.iter().flat_map(logged));
    println!("R-OFF-2 B showed the prompt's result {:?} after it was sent to the new daemon", sent.elapsed());
    assert!(got.iter().any(|u| matches!(u, viewer::Update::Host(true))), "B joined the new host: {got:?}");
    let text: String = got.iter().filter_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ItemDelta { delta, .. })) = u && let krowk_harness::protocol::Delta::Text { text } = delta { Some(text.clone()) } else { None }).collect();
    assert!(text.contains("word159 "), "the new turn's answer reached B live: {} chars", text.len());
    turn.await.unwrap();
    caught_up_with(&w, &mut v, &session, &mut ids, Duration::from_secs(20)).await;
    let mut once = ids.clone();
    once.sort();
    once.dedup();
    assert_eq!(once.len(), ids.len(), "every logged event applied once");

    let after = streams(&w);
    let new: Vec<&String> = after.iter().filter(|s| !before.contains(s)).collect();
    assert!(!new.is_empty(), "the new daemon's bridge streams on a stream of its own: before {before:?}, after {after:?}");
    assert_eq!(again.built(&session).len(), 1, "taken up once, by the prompt");
    again.stop(Duration::from_secs(10)).await;
    assert_eq!(leased(&w, &session).await, Ok(false));
}

/// R-OFF-2, R-SYNC-2 (ticket 98): a daemon that crashed leaves its lease
/// held until it lapses. A daemon thread cannot be killed in-process, so
/// the leftover is A's own device taking the lease (a 30-second one) after
/// a clean stop. The new daemon's first prompt is refused it
/// (`lease_held`) and never takes it from its holder; once it has lapsed,
/// the session is taken up within 75 seconds of the prompt — the
/// supervisor's one more try after `LEASE_HELD_WAIT` — though the turn
/// ended long before and no other came.
#[tokio::test]
async fn r_off_2_a_crashed_daemons_leftover_lease_is_waited_out_and_the_session_taken_up_again() {
    let w = World::new("crash");
    let a = w.device("machine-a");
    let mut auto = w.auto(&a, false);
    let d = w.daemon().await;
    let session = prompted(&w, &d, None, "say hello").await;
    w.synced(&session).await;
    auto.stop_daemon(&w, &d).await;
    drop(d);
    let (api, id, dev) = (w.as_device(&a), session.clone(), a.key.id().to_string());
    tokio::task::spawn_blocking(move || api.acquire_lease(&id, &dev, 30, "development")).await.unwrap().expect("the crashed daemon's lease");

    let mut again = w.auto(&a, false);
    let d = w.daemon().await;
    let sent = Instant::now();
    prompted(&w, &d, Some(&session), "say it again").await;
    taken_up(&w, &again, &session, sent + Duration::from_secs(75)).await;
    let took = sent.elapsed();
    println!("R-OFF-2 taken up again {took:?} after the prompt, past a crashed daemon's lease");
    let built = again.built(&session);
    assert_eq!(built.len(), 2, "refused once at the prompt, and taken up at its one more try: {built:?}");
    assert!(built[1] - built[0] >= auto::LEASE_HELD_WAIT - Duration::from_millis(100), "the lease waited out, never taken: {:?}", built[1] - built[0]);
    w.synced(&session).await;
    again.stop(Duration::from_secs(10)).await;
}

/// The session a registry request's line names in `/sessions/<id>/`.
fn session_in(line: &str) -> Option<&str> {
    let rest = &line[line.find("/sessions/")? + "/sessions/".len()..];
    rest.split(['/', ' ', '?']).next()
}

/// The most of `times` within any one minute.
fn most_in_a_minute(mut times: Vec<Instant>) -> usize {
    times.sort();
    let (mut most, mut from) = (0, 0);
    for i in 0..times.len() {
        while times[i] - times[from] >= Duration::from_secs(60) {
            from += 1;
        }
        most = most.max(i + 1 - from);
    }
    most
}

/// How many sessions `r_lag_9_fifty_…` bridges at once.
const FIFTY: usize = 50;

/// R-LAG-9, R-SYNC-3, R-SYNC-2, R-OFF-2 (ticket 98): fifty sessions taken
/// up together by A's supervisor and streaming at once leave the daemon's
/// thread on time — under the 30 ms lateness probe while every one types —
/// and stay well under the registry's lease ceilings: over 70 seconds and
/// more, no session makes more than 60 lease calls a minute (POST or PUT
/// `…/lease`; a renewal every 20 seconds is 3) and all of them together no
/// more than 5000 (about 150). The device list is read about once a minute
/// for all fifty (the supervisor's one reader), not once a minute each. All
/// fifty are listed on B, and stopping the supervisor gives every lease
/// back within the daemon's grace.
///
/// Held to its limits in the release build alone, as `make bench` runs it:
/// a debug build is too slow to say anything about the daemon's thread.
#[cfg_attr(debug_assertions, ignore = "release only, as make bench runs it")]
#[tokio::test]
async fn r_lag_9_fifty_auto_synced_sessions_leave_the_daemons_thread_on_time() {
    let w = World::new("fifty");
    let (a, b) = (w.device("machine-a"), w.device("machine-b"));
    let mut auto = w.auto(&a, false);
    let d = w.daemon().await;
    let t0 = Instant::now();

    // Fifty new sessions at once: fifty take-ups together.
    let mut new = tokio::task::JoinSet::new();
    for _ in 0..FIFTY {
        let (d, p) = (d.clone(), w.prompt(None, "say hello"));
        new.spawn(async move {
            let (tx, mut rx) = mpsc::channel(1024);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            d.execute(p, tx).await.unwrap().unwrap().session_id
        });
    }
    let sessions: Vec<String> = new.join_all().await;
    // A take-up the registry refused as one write too many is taken up
    // again after its minute (`auto::LIMITED_WAIT`).
    for s in &sessions {
        taken_up(&w, &auto, s, t0 + auto::LIMITED_WAIT + Duration::from_secs(30)).await;
    }
    let all_up = Instant::now();
    println!("R-SYNC-3 {FIFTY} sessions taken up {:?} after their prompts", t0.elapsed());

    // All fifty type at once: the daemon's thread is watched from the
    // moment the last starts until the first ends.
    let (typing, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut turns = tokio::task::JoinSet::new();
    for s in &sessions {
        let (d, p, typing, done) = (d.clone(), w.prompt(Some(s), "a very long answer"), typing.clone(), done.clone());
        turns.spawn(async move {
            let (tx, mut rx) = mpsc::channel(4096);
            let read = tokio::spawn(async move {
                let mut typed = false;
                while let Some(l) = rx.recv().await {
                    if !typed && matches!(l, StreamLine::Live(LiveEvent::ItemDelta { .. })) {
                        typed = true;
                        typing.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
            d.execute(p, tx).await.unwrap().unwrap();
            done.fetch_add(1, Ordering::SeqCst);
            read.await.unwrap();
        });
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while typing.load(Ordering::SeqCst) < FIFTY {
        assert!(Instant::now() < deadline, "only {} of {FIFTY} ever typed", typing.load(Ordering::SeqCst));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    w.lateness();
    let all_typing = Instant::now();
    assert_eq!(done.load(Ordering::SeqCst), 0, "a turn ended before the last began: nothing streamed together");
    while done.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let late = w.lateness();
    let together = all_typing.elapsed();
    turns.join_all().await;
    // Held to the limit in the release build alone (the doc comment).
    let timed = !cfg!(debug_assertions);
    println!("R-LAG-9 the daemon's thread was late by at most {late:?} while {FIFTY} sessions streamed together for {together:?}");
    assert!(together > Duration::from_secs(2), "they streamed together long enough to say: {together:?}");
    assert!(!timed || late < Duration::from_millis(30), "something blocked the daemon's thread {late:?} while {FIFTY} bridged sessions streamed");

    // Seventy seconds and more of the bridges' lease calls, and long enough
    // after the last take-up for the list to have been read once.
    let until = (t0 + Duration::from_secs(75)).max(all_up + host::LIST_EVERY + Duration::from_secs(5));
    tokio::time::sleep(until.saturating_duration_since(Instant::now())).await;
    let spanned = t0.elapsed();
    let timed_calls = w.reg_times.lock().unwrap().clone();
    let mut by_session: std::collections::HashMap<String, Vec<Instant>> = Default::default();
    let mut every = Vec::new();
    for (at, line, _) in &timed_calls {
        if (line.starts_with("POST ") || line.starts_with("PUT ")) && line.contains("/lease ") {
            by_session.entry(session_in(line).unwrap_or_default().to_string()).or_default().push(*at);
            every.push(*at);
        }
    }
    let total = every.len();
    let most_one = by_session.values().map(|t| most_in_a_minute(t.clone())).max().unwrap_or(0);
    let most_all = most_in_a_minute(every);
    let lists = timed_calls.iter().filter(|(_, line, _)| line.starts_with("GET ") && line.contains("/devices")).count();
    let refused: Vec<&String> = timed_calls.iter().filter(|(_, _, status)| *status == 429).map(|(_, line, _)| line).collect();
    println!("R-SYNC-2 over {spanned:?}: {total} lease calls, at most {most_all} in a minute in all and {most_one} for one session; the device list read {lists} time(s); {} request(s) refused 429", refused.len());
    assert!(spanned >= Duration::from_secs(70));
    assert_eq!(by_session.len(), FIFTY, "every session's lease was called for");
    assert!(most_one <= 60, "one session made {most_one} lease calls in a minute");
    assert!(most_all <= 5000, "{most_all} lease calls in a minute in all");
    let minutes = spanned.as_secs() / 60;
    assert!(lists >= 1 && lists as u64 <= minutes + 1, "one reader for {FIFTY} bridges reads the list about once a minute, not {FIFTY} times: {lists} reads in {spanned:?}");
    let again = sessions.iter().filter(|s| auto.built(s).len() > 1).count();
    println!("R-SYNC-3 {again} take-up(s) refused and taken up again");
    assert!(sessions.iter().all(|s| auto.built(s).len() <= 2), "a take-up refused 429 waits its minute rather than backing off into it again");

    // Past the registry's ceiling on creates, nothing is lost: what it
    // refused is put again once it takes writes (a minute on), and the
    // registry holds every session's log whole.
    for s in &sessions {
        stored_whole(&w, s, Duration::from_secs(60)).await;
    }
    println!("R-OFF-2 every session's log stored whole {:?} after the take-ups", t0.elapsed());

    let (api, keys, chain) = (w.as_device(&b), w.keys(), w.chain());
    let (listed, _) = tokio::task::spawn_blocking(move || viewer::list(&api, &keys, &chain)).await.unwrap().unwrap();
    let missing: Vec<&String> = sessions.iter().filter(|s| !listed.iter().any(|l| &l.id == *s)).collect();
    assert!(missing.is_empty(), "B lists every session: missing {missing:?}");

    // Stopped with fifty bridged, every lease goes back within the grace.
    let stopping = Instant::now();
    auto.stop(krowk_harness::host::SHUTDOWN_GRACE).await;
    let stopped = stopping.elapsed();
    println!("R-OFF-2 {FIFTY} bridges stopped in {stopped:?}");
    assert!(stopped < krowk_harness::host::SHUTDOWN_GRACE, "{FIFTY} bridges took {stopped:?} to stop");
    assert!(auto.bridged.lock().unwrap().is_empty());
    for s in &sessions {
        assert_eq!(leased(&w, s).await, Ok(false), "{s}'s lease given back");
    }
}
