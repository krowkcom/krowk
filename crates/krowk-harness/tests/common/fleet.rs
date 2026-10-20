//! The fake fleet (Harness P4 ticket 75): the stand-in registry, the
//! reference relay, a mock Anthropic model, a host daemon and a sync bridge
//! for one session, in this process, every listener on 127.0.0.1 — the
//! backend an app's tests and the App Review demo run against instead of
//! api.krowk.com (R-IOS-15). Built from the pieces `sync_attach.rs`'s
//! `World` uses, less its proxies and second device. Included by
//! `examples/fake_fleet.rs`, which serves it, and by `tests/fake_fleet.rs`,
//! which holds it to what it prints. Never a part of a build krowk ships.
//!
//! Host A's keys are kept on disk, in the fleet's `home/.krowk`, as `krowk
//! sync init` keeps a device's: so a `krowk` run with that home and the
//! fleet's API *is* host A, and `Pairer` (ticket 76) answers a phone's
//! pairing with the real `krowk devices add` rather than a copy of it.
//!
//! The crate including it declares `mock` and `scratch` beside it.
#![allow(dead_code)]

use crate::{mock, scratch};
use krowk_client::device_chain::{Batch, Chain, Kind, Subject};
use krowk_client::e2e::{self, DeviceKey, SigningKey};
use krowk_client::keystore::Keystore;
use krowk_client::session_record;
use krowk_client::user_key::{UserKey, UserKeys};
use krowk_harness::daemon::{self, client::Client, server};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{Command, LiveEvent, PermissionMode, StreamLine};
use krowk_harness::sync::host;
use std::io::BufRead;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// When the fleet's device list is made, by its devices' clocks.
pub const T0: u64 = 1_790_000_000;
/// The person's token base: every key of theirs is `TOKEN#name`, and a
/// browser login the fleet approves is minted under it
/// (`Config::login_person`), so it lands in host A's person.
pub const TOKEN: &str = "krowk_sk_fake_fleet_0000000000000000";
/// The demo's held tool call, which the model makes on "make the file".
pub const APPROVAL: &str = "toolu_01Touch";
/// The session's title: a readable one for the demo.
const TITLE: &str = "fake fleet";
const DEMO_TITLE: &str = "A demo session";
const HELLO: &str = "Say hello";
const ANSWER: &str = "Hello! I can read this project, change files in it and run commands, asking you first.";
const MAKE: &str = "Please make the file approved.txt";
const MADE: &str = "Done: approved.txt is in the project.";
const DENIED: &str = "All right, I left the project as it was.";

/// A device of the person's: its keys and the token that speaks for it.
pub struct Device {
    pub key: DeviceKey,
    pub signing: SigningKey,
    pub token: String,
}

impl Device {
    pub fn new(name: &str) -> Device {
        Device { key: DeviceKey::generate(), signing: SigningKey::generate(), token: format!("{TOKEN}#{name}") }
    }

    /// The device whose keys `store` keeps, made and kept there now.
    pub fn kept(store: &Keystore, name: &str) -> Device {
        let key = store.device_key().expect("the fleet's home keeps host A's device key");
        let signing = store.signing_key().expect("the fleet's home keeps host A's signing key");
        Device { key, signing, token: format!("{TOKEN}#{name}") }
    }

    /// Its own registry client, its device calls signed by its key.
    pub fn client(&self, api: &str) -> Arc<krowk_api::Client> {
        let signer = e2e::DeviceSigner::new(self.key.id(), self.signing_copy()).shared();
        Arc::new(krowk_api::Client::new(api, &self.token).signed_by(signer))
    }

    pub fn signing_copy(&self) -> SigningKey {
        SigningKey::from_secret(&*self.signing.secret_bytes()).unwrap()
    }

    pub fn subject(&self, name: &str) -> Subject {
        Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: self.key.public(), signing: self.signing.public() }
    }
}

/// A device list post of `b`, as the chain made it.
pub fn post(b: &Batch) -> krowk_api::sync::ListPost {
    krowk_api::sync::ListPost {
        entries: b.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: b.links.iter().map(|l| e2e::hex(l)).collect(),
        wraps: b.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over: false,
    }
}

/// The model: a short answer to anything, except "make the file", which it
/// answers with a `bash` call — an approval — and then says how it went.
fn model(body: &serde_json::Value, _n: usize) -> mock::Reply {
    let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
    let result = last["content"].as_array().and_then(|c| c.iter().find(|b| b["type"] == "tool_result").cloned());
    let text = if let Some(r) = result {
        if r["is_error"] == true { DENIED } else { MADE }
    } else if last.to_string().contains("make the file") {
        return mock::Reply::sse(&mock::tool_use(APPROVAL, "bash", &serde_json::json!({"command": "touch approved.txt"})));
    } else {
        ANSWER
    };
    mock::Reply::paced(mock::text_stream(text), Duration::from_millis(20))
}

/// A running fleet. Dropping it without `stop` stops the registry and
/// leaves the bridge, relay, daemon and model to the process's end.
pub struct Fleet {
    /// The registry's API base, `http://127.0.0.1:…/v1`.
    pub api: String,
    pub relay: String,
    pub env: String,
    /// POST here, `{code}` replaced by the code an app shows, to approve
    /// its browser sign-in.
    pub approve: String,
    /// The synced session's id.
    pub session: String,
    /// With `demo`, the model's id of the tool call waiting for a yes
    /// (`APPROVAL`), and the id of its request, which `approve` names.
    pub approval: Option<String>,
    pub request: Option<String>,
    /// Host A, the first device on the person's list.
    pub host: Device,
    /// The person's device list as A started it, and the user key it left
    /// current, which every device of theirs holds.
    pub chain: Chain,
    pub user: UserKey,
    pub root: PathBuf,
    stop: watch::Sender<bool>,
    bridge: std::thread::JoinHandle<()>,
    _registry: krowk_devregistry::Running,
    _mock: mock::Mock,
}

impl Fleet {
    /// One `key=value` line per thing a test needs, in the order printed.
    pub fn lines(&self) -> Vec<String> {
        let mut l = vec![format!("api={}", self.api), format!("relay={}", self.relay), format!("env={}", self.env), format!("approve={}", self.approve), format!("session={}", self.session)];
        l.extend(self.approval.iter().map(|a| format!("approval={a}")));
        l.extend(self.request.iter().map(|r| format!("request={r}")));
        l
    }

    /// Takes the host away (R-HAND-4): the bridge stops and lets its lease
    /// go, while the registry and the relay stay up.
    pub fn stop_host(&self) {
        let _ = self.stop.send(true);
    }

    /// Stops the bridge, waits for it, and removes the scratch root.
    pub fn stop(self) {
        self.stop_host();
        let _ = self.bridge.join();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Starts every piece, runs the first turn, syncs it and, with `demo`,
/// leaves a second turn waiting on an approval. Panics, saying what, if a
/// piece does not come up.
pub fn start(demo: bool) -> Fleet {
    let root = scratch::root("fleet");
    for d in ["home", "run", "repo/.git"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let config = krowk_devregistry::Config { login_person: Some(TOKEN.into()), ..Default::default() };
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).expect("the stand-in registry starts");
    let api = format!("{}/v1", registry.url());
    let relay = relay();
    let m = mock::serve(model);
    let env = env(&root, &m.url);

    // Host A starts the person's device list, its keys, the user key and
    // the list kept in its home as `sync init` keeps them.
    let store = Keystore::new(&root.join("home/.krowk"));
    let a = Device::kept(&store, "host-a");
    let (chain, start) = Chain::start(a.subject("host-a"), &a.signing, None, T0).unwrap();
    a.client(&api).init_device_list(&post(&start)).expect("the registry takes host A's device list");
    let user = start.newest;
    store.save_user_keys(&UserKeys::new(user.clone(), []).unwrap()).expect("host A's home keeps the user key");
    store.save_device_list(&start.entries).expect("host A's home keeps the device list");

    serve(&root, env.clone());
    let socket = daemon::socket(&env).unwrap();
    let repo = root.join("repo");
    let (keys, sessions) = (UserKeys::new(user.clone(), []).unwrap(), log::sessions_dir(&env).unwrap());
    let options = |session: &str| host::Options {
        relay: relay.clone(),
        env: "development".into(),
        api: a.client(&api),
        device: a.key.id(),
        signing: a.signing_copy(),
        keys: UserKeys::new(user.clone(), []).unwrap(),
        chain: chain.clone(),
        session: session.into(),
        title: if demo { DEMO_TITLE } else { TITLE }.into(),
        cwd: repo.display().to_string(),
        sessions: sessions.clone(),
        ttl: host::LEASE_TTL,
        keep: host::KEEP,
        direct: None,
        tailnet: None,
        handed: None,
    };
    let prompt = |session: Option<&str>, text: &str| {
        let model = Registry::resolve(&InstancesConfig::default(), &env).parse_model("claude-sonnet-4-6").unwrap();
        Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }
    };

    // The first turn, then the bridge, on a runtime of their own thread.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let d = rt.block_on(Client::connect(&socket, &repo, "test", true)).unwrap_or_else(|e| panic!("the host daemon does not answer: {}", connect_error(e)));
    let first = prompt(None, HELLO);
    let session = rt.block_on(async { d.execute(first, drain()).await }).expect("the first turn runs").expect("the first turn ends").session_id;
    let bridged = rt.block_on(Client::connect(&socket, &repo, "test", true)).unwrap_or_else(|e| panic!("the host daemon does not answer the bridge: {}", connect_error(e)));
    let (stop, stop_rx) = watch::channel(false);
    let o = options(&session);
    let bridge = std::thread::spawn(move || {
        let (_cp, cp_rx) = mpsc::unbounded_channel();
        if let Err(e) = rt.block_on(host::run(o, Arc::new(bridged), stop_rx, cp_rx)) {
            eprintln!("fleet: the bridge ended: {e}");
        }
        drop(d);
    });
    synced(&a.client(&api), &session, &keys, &chain);

    // With `demo`, a second turn, typed at the host, held on its approval.
    let request = demo.then(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let d = rt.block_on(Client::connect(&socket, &repo, "test", true)).unwrap_or_else(|e| panic!("the host daemon does not answer: {}", connect_error(e)));
        let (tx, rx) = std::sync::mpsc::channel();
        let cmd = prompt(Some(&session), MAKE);
        std::thread::spawn(move || {
            rt.block_on(async move {
                let (out, mut lines) = mpsc::channel(4096);
                tokio::spawn(async move {
                    while let Some(l) = lines.recv().await {
                        if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = l {
                            let _ = tx.send(r.request_id);
                        }
                    }
                });
                // Held until the process ends, or the approval is answered.
                let _ = d.execute(cmd, out).await;
            })
        });
        // The bridge hears it as this client does, and hands it to a viewer
        // in its welcome, or live to one welcomed already.
        rx.recv_timeout(Duration::from_secs(10)).expect("the demo's second turn asks for an approval")
    });
    let approval = request.as_ref().map(|_| APPROVAL.to_string());

    let approve = format!("{}/_approve/cli/authorizations/{{code}}/approval", registry.url());
    Fleet { api, relay, env: "development".into(), approve, session, approval, request, host: a, chain, user, root, stop, bridge, _registry: registry, _mock: m }
}

fn connect_error(e: daemon::client::ConnectError) -> String {
    match e {
        daemon::client::ConnectError::Absent => "nothing listens".into(),
        daemon::client::ConnectError::Failed(e) => e.message,
    }
}

/// A turn's lines, read and dropped.
fn drain() -> mpsc::Sender<StreamLine> {
    let (tx, mut rx) = mpsc::channel(4096);
    std::thread::spawn(move || while rx.blocking_recv().is_some() {});
    tx
}

/// The reference relay, trusting the stand-in registry's ticket key.
fn relay() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
    let roster = krowk_harness::relay::Roster::parse(&roster).unwrap();
    std::thread::spawn(move || krowk_harness::relay::run(listener, krowk_harness::relay::Config { roster, origin: None, limits: Default::default(), state: None, origins: Vec::new(), whois: None, pin: None }));
    format!("ws://{addr}")
}

/// The environment host A's daemon reads: its home and runtime directory
/// under the scratch root, and the mock model for Anthropic.
fn env(root: &Path, model: &str) -> impl Fn(&str) -> String + Clone + Send + Sync + 'static {
    let (root, url) = (root.to_path_buf(), model.to_string());
    move |k| match k {
        "HOME" => root.join("home").display().to_string(),
        "XDG_RUNTIME_DIR" => root.join("run").display().to_string(),
        "ANTHROPIC_API_KEY" => "sk-test".into(),
        "ANTHROPIC_BASE_URL" => url.clone(),
        _ => String::new(),
    }
}

/// Host A's daemon on a thread of its own, once it listens.
fn serve(root: &Path, env: impl Fn(&str) -> String + Clone + Send + Sync + 'static) {
    let socket = daemon::socket(&env).unwrap();
    let credentials = root.join("home/.krowk/credentials.json");
    let wait = socket.clone();
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
                session: Default::default(),
            })
        });
        server::run(server::Options { socket, idle: None, krowk_version: "test".into(), ..Default::default() }, factory)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::os::unix::net::UnixStream::connect(&wait).is_err() {
        assert!(Instant::now() < deadline, "the host daemon never listened");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until the session's sealed index names a lease, a checkpoint and
/// a head: the bridge has taken the session up and written it.
fn synced(api: &krowk_api::Client, session: &str, keys: &UserKeys, chain: &Chain) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ready = (|| -> Result<(), String> {
            let s = api.show_sync_session(session).map_err(|e| e.to_string())?;
            let key = krowk_harness::sync::store::open_session_key(&s, session, keys, chain, session_record::Signer::EverHeld)?;
            let index = krowk_harness::sync::store::open_index(&key, session, &s.sealed_index)?;
            match (s.lease.is_some(), index.checkpoint.is_some(), index.head.is_some()) {
                (true, true, true) => Ok(()),
                (lease, checkpoint, head) => Err(format!("lease {lease}, checkpoint {checkpoint}, head {head}")),
            }
        })();
        match ready {
            Ok(()) => return,
            Err(e) => assert!(Instant::now() < deadline, "the bridge never synced the session: {e}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Host A's side of pairing (ticket 76), played by the real CLI: a debug
/// `krowk` (`cargo build -p krowk --features harness`, never `--release`,
/// whose build ignores the unattended approval) with host A's home and the
/// fleet's API, logged in as A once and then run as `krowk devices add`, one
/// at a time, saying yes to the device that joins (R-E2E-3).
pub struct Pairer {
    bin: PathBuf,
    root: PathBuf,
    api: String,
    running: Option<Running>,
}

/// The `devices add` running now, shared with the thread that waits for
/// it, and a channel that hears once it has ended and been reaped.
struct Running {
    child: Arc<std::sync::Mutex<std::process::Child>>,
    ended: std::sync::mpsc::Receiver<()>,
}

/// The code on a `  Code: XXXX-XXXX    valid 10 minutes, once` line, and
/// only a pairing code: an error's `Code: pairing_open` is not one.
fn code_of(line: &str) -> Option<String> {
    let code = line.trim().strip_prefix("Code: ")?.split_whitespace().next()?;
    let shaped = code.len() == 9 && code.bytes().enumerate().all(|(i, b)| if i == 4 { b == b'-' } else { krowk_client::pairing::ALPHABET.contains(&b) });
    shaped.then(|| code.to_string())
}

impl Pairer {
    /// Stores host A's token in its home's credentials file with `krowk
    /// login --token`, so a `krowk` with that home needs no `KROWK_TOKEN`.
    pub fn new(fleet: &Fleet, bin: &Path) -> Result<Pairer, String> {
        let p = Pairer { bin: bin.to_path_buf(), root: fleet.root.clone(), api: fleet.api.clone(), running: None };
        let out = p.krowk(&["login", "--token", &fleet.host.token]).stdout(Stdio::null()).output().map_err(|e| format!("cannot run KROWK_BIN={}: {e}", bin.display()))?;
        if !out.status.success() {
            return Err(format!("`krowk login --token` as host A failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(p)
    }

    /// `krowk` as host A: its home and the fleet's API, nothing of this
    /// machine's, and the debug build's stand-in for the person on.
    fn krowk(&self, args: &[&str]) -> Process {
        let home = self.root.join("home");
        let mut c = Process::new(&self.bin);
        c.args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("KROWK_HOME", home.join(".krowk"))
            .env("KROWK_API_URL", &self.api)
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1")
            .current_dir(&home)
            .stdin(Stdio::null());
        c
    }

    /// Ends the running `devices add`, if any, and starts another; its code,
    /// once it shows one. The child goes on waiting for the device that
    /// types the code, and its lines and exit status go to stderr.
    pub fn pair(&mut self) -> Result<String, String> {
        self.end();
        let mut child = self.krowk(&["devices", "add"]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("cannot run KROWK_BIN={}: {e}", self.bin.display()))?;
        let stderr = child.stderr.take().expect("piped");
        let (code, shown) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("fleet: krowk devices add: {line}");
                if let Some(c) = code_of(&line) {
                    let _ = code.send(c);
                }
            }
        });
        // Reaped only under the lock, so `end` never signals a pid that
        // may have been handed to another process.
        let (child, (done, ended)) = (Arc::new(std::sync::Mutex::new(child)), std::sync::mpsc::channel());
        let waited = child.clone();
        std::thread::spawn(move || {
            loop {
                // Its own statement, so the lock is let go before the sleep.
                let status = waited.lock().unwrap().try_wait();
                match status {
                    Ok(Some(s)) => eprintln!("fleet: krowk devices add ended: {s}"),
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    Err(e) => eprintln!("fleet: krowk devices add could not be waited for: {e}"),
                }
                break;
            }
            let _ = done.send(());
        });
        self.running = Some(Running { child, ended });
        shown.recv_timeout(Duration::from_secs(30)).map_err(|_| "`krowk devices add` showed no code (its lines are above) — is KROWK_BIN a debug build, `cargo build -p krowk --features harness`?".to_string())
    }

    /// Ends the running `devices add`: ^C, on which it ends its pairing on
    /// the registry — which keeps one live pairing a person — and exits;
    /// killed if it has not within five seconds, which leaves its pairing
    /// open, so the next `pair` is refused (`pairing_open`) until the
    /// registry's ten minutes are up.
    pub fn end(&mut self) {
        let Some(r) = self.running.take() else { return };
        {
            let mut c = r.child.lock().unwrap();
            if !matches!(c.try_wait(), Ok(None)) {
                return;
            }
            // SAFETY: kill(2) on our own child, not yet reaped: it is reaped
            // only under this lock.
            unsafe { libc::kill(c.id() as libc::pid_t, libc::SIGINT) };
        }
        if r.ended.recv_timeout(Duration::from_secs(5)).is_err() {
            let _ = r.child.lock().unwrap().kill();
            let _ = r.ended.recv();
        }
    }
}

impl Drop for Pairer {
    fn drop(&mut self) {
        self.end();
    }
}
