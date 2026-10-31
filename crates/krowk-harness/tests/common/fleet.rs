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
use krowk_harness::instances::{InstanceKind, InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{Command, LiveEvent, PermissionMode, StreamLine};
use krowk_harness::sync::{announce, auto, host};
use std::io::BufRead;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
/// The demo's tool call that runs with no approval, a `read` of the
/// project's README, which the model makes on "read the README".
pub const READ: &str = "toolu_01Read";
/// The `read` the model makes on "stream a long answer" before it streams
/// `LONG_LINES` lines at 200 tokens a second: a turn a phone follows live.
pub const STREAM: &str = "toolu_01Stream";
/// The long answer's lines, about eleven tokens each: about 6.6 s.
pub const LONG_LINES: usize = 120;
/// The very long answer's, about 16.5 s, for a longer run by hand (and
/// `fake_fleet.rs`, which holds it to reaching a viewer whole).
pub const VERY_LONG_LINES: usize = 300;
/// The `read` before the very long answer.
pub const STREAM_LONGER: &str = "toolu_01StreamLonger";
/// What a test types to have the model stream its long answer.
pub const LONG: &str = "Please stream a long answer";
/// Host A's second instance (ticket 135): another Anthropic API key on the
/// same mock model, ready beside `anthropic`, for a phone to switch a
/// session to, and the instance a limit on `anthropic` offers.
pub const PERSONAL: &str = "anthropic:personal";
/// The keys the mock tells the two instances apart by.
const KEY: &str = "sk-test";
const PERSONAL_KEY: &str = "sk-personal";
const PERSONAL_KEY_ENV: &str = "KROWK_FLEET_PERSONAL_KEY";
/// The session's title: a readable one for the demo.
const TITLE: &str = "fake fleet";
const DEMO_TITLE: &str = "A demo session";
const HELLO: &str = "Say hello";
const ANSWER: &str = "Hello! I can read this project, change files in it and run commands, asking you first.";
const MAKE: &str = "Please make the file approved.txt";
/// The demo's second turn: a tool call with its result, and an answer.
const LOOK: &str = "Please read the README";
const README: &str = "# The demo project\nA project the fake fleet made, for the phone to read.\n";
const LOOKED: &str = "The README says this is the demo project.";
const MADE: &str = "Done: approved.txt is in the project.";
const DENIED: &str = "All right, I left the project as it was.";
/// The README of the repository host A announces (ticket 134), where a
/// phone starts a session of its own.
const PROJECT_README: &str = "# The fleet's project\nA repository host A trusts, for a phone to start a session in.\n";
/// How often host A refreshes its announcement, and the TTL it asks: a
/// start a phone sends is seen within `ANNOUNCE_EVERY`, and a host taken
/// offline (`Hosting::offline`) is listed so within `ANNOUNCE_TTL` seconds.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(2);
const ANNOUNCE_TTL: u64 = 10;

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
/// answers with a `bash` call — an approval — and then says how it went;
/// "read the README", which it answers with a `read` call that runs at
/// once, and then says what it read; and "stream a long answer", a `read`
/// call and then `LONG_LINES` numbered lines, one word-sized delta every
/// 5 ms (200 tokens a second, as R-LAG-7's phone budget assumes) —
/// `VERY_LONG_LINES` for "stream a very long answer".
fn model(body: &serde_json::Value) -> mock::Reply {
    let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
    let result = last["content"].as_array().and_then(|c| c.iter().find(|b| b["type"] == "tool_result").cloned());
    let text = if let Some(r) = result {
        if r["tool_use_id"] == STREAM {
            return mock::Reply::paced(mock::text_stream(&mock::numbered_lines(LONG_LINES)), Duration::from_millis(5));
        } else if r["tool_use_id"] == STREAM_LONGER {
            return mock::Reply::paced(mock::text_stream(&mock::numbered_lines(VERY_LONG_LINES)), Duration::from_millis(5));
        } else if r["tool_use_id"] == READ {
            LOOKED
        } else if r["is_error"] == true {
            DENIED
        } else {
            MADE
        }
    } else if last.to_string().contains("stream a very long answer") {
        return mock::Reply::paced(mock::tool_use(STREAM_LONGER, "read", &serde_json::json!({"path": "README.md"})), Duration::from_millis(20));
    } else if last.to_string().contains("stream a long answer") {
        return mock::Reply::paced(mock::tool_use(STREAM, "read", &serde_json::json!({"path": "README.md"})), Duration::from_millis(20));
    } else if last.to_string().contains("read the README") {
        return mock::Reply::sse(&mock::tool_use(READ, "read", &serde_json::json!({"path": "README.md"})));
    } else if last.to_string().contains("make the file") {
        return mock::Reply::sse(&mock::tool_use(APPROVAL, "bash", &serde_json::json!({"command": "touch approved.txt"})));
    } else {
        ANSWER
    };
    mock::Reply::paced(mock::text_stream(text), Duration::from_millis(20))
}

/// The mock model as host A's instances reach it (ticket 135): while
/// `limited` counts, a call on `anthropic`'s key answers 429 with
/// `retry-after: 0`, one per attempt the native engine makes
/// (`http::ATTEMPTS`), so the turn fails on its limit and the host offers
/// `PERSONAL`. Every other answer says how near its limit each instance is
/// (R-INST-6): `anthropic` 85% used, a warning; `PERSONAL` 10%.
fn answer(seen: &mock::Seen, limited: &AtomicUsize) -> mock::Reply {
    let key = seen.header("x-api-key").unwrap_or_default();
    if key == KEY && limited.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
        let refused = serde_json::json!({"type": "error", "error": {"type": "rate_limit_error", "message": "Number of request tokens has exceeded your per-minute rate limit"}});
        return mock::Reply { headers: vec![("retry-after".into(), "0".into())], ..mock::Reply::json(429, &refused) };
    }
    let mut reply = model(&seen.body);
    let remaining = if key == PERSONAL_KEY { "900" } else { "150" };
    reply.headers.push(("anthropic-ratelimit-tokens-limit".into(), "1000".into()));
    reply.headers.push(("anthropic-ratelimit-tokens-remaining".into(), remaining.into()));
    reply
}

/// Host A's instances: Anthropic's from the environment, on the mock, and
/// `PERSONAL` beside it on the same mock under a key of its own.
fn instances(model: &str) -> InstancesConfig {
    let personal = InstanceKind::AnthropicApi { api_key_env: Some(PERSONAL_KEY_ENV.into()), base_url: Some(model.into()), thinking: None, max_tokens: None, effort: None };
    InstancesConfig { instances: [(PERSONAL.to_string(), personal)].into_iter().collect(), ..Default::default() }
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
    /// The repository host A trusts and announces (R-HOST-3): a session
    /// another device starts runs here, and automatic sync takes it up.
    pub project: PathBuf,
    /// Host A as the fleet's control uses it: a session of a test's own,
    /// hosted and let go.
    pub hosting: Arc<Hosting>,
    /// Host A's automatic sync, beside its daemon: its announcement, the
    /// starts other devices send it, and a bridge for each session a start
    /// began.
    supervisor: Option<auto::Handle>,
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

    /// Stops the bridge and every session a test had hosted, waits for
    /// them, closes every direct listener, and removes the scratch root.
    pub fn stop(mut self) {
        self.stop_host();
        let _ = self.bridge.join();
        self.hosting.release_all();
        if let Some(s) = self.supervisor.take() {
            s.stop(Duration::from_secs(5));
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Starts every piece, runs the first turn, syncs it and, with `demo`,
/// runs a second turn with a tool call and its result and leaves a third
/// waiting on an approval. Panics, saying what, if a
/// piece does not come up.
pub fn start(demo: bool) -> Fleet {
    start_with(demo, false)
}

/// `start`, every bridge offering its LAN listener on 127.0.0.1 with
/// `direct_lan` (`KROWK_DIRECT_LAN`, with no Tailscale on the machine): a
/// welcome names `ws://127.0.0.1:<port>` as a `lan` candidate, which a
/// viewer may dial because the welcome named loopback itself.
pub fn start_with(demo: bool, direct_lan: bool) -> Fleet {
    let root = scratch::root("fleet");
    for d in ["home", "run", "repo/.git", "project"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("repo/README.md"), README).unwrap();
    let project = root.join("project");
    git_init(&project);
    std::fs::write(project.join("README.md"), PROJECT_README).unwrap();
    let config = krowk_devregistry::Config { login_person: Some(TOKEN.into()), ..Default::default() };
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).expect("the stand-in registry starts");
    let api = format!("{}/v1", registry.url());
    let relay = relay();
    let limited: Arc<AtomicUsize> = Arc::default();
    let m = {
        let limited = limited.clone();
        mock::serve_seen(move |seen, _| answer(seen, &limited))
    };
    let env = env(&root, &m.url);
    let instances = instances(&m.url);

    // Host A starts the person's device list, its keys, the user key and
    // the list kept in its home as `sync init` keeps them.
    let store = Keystore::new(&root.join("home/.krowk"));
    let a = Device::kept(&store, "host-a");
    let (chain, start) = Chain::start(a.subject("host-a"), &a.signing, None, T0).unwrap();
    a.client(&api).init_device_list(&post(&start)).expect("the registry takes host A's device list");
    let user = start.newest;
    store.save_user_keys(&UserKeys::new(user.clone(), []).unwrap()).expect("host A's home keeps the user key");
    store.save_device_list(&start.entries).expect("host A's home keeps the device list");

    let (turns, turns_rx) = auto::turn_hook();
    let bridged: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>> = Arc::default();
    serve(&root, env.clone(), instances.clone(), turns, bridged.clone());
    let hosting = Arc::new(Hosting {
        api: api.clone(),
        relay: relay.clone(),
        socket: daemon::socket(&env).unwrap(),
        repo: root.join("repo"),
        sessions: log::sessions_dir(&env).unwrap(),
        env: Arc::new(env.clone()),
        host: Device::kept(&store, "host-a"),
        chain: chain.clone(),
        user: user.clone(),
        tests: std::sync::Mutex::default(),
        started: std::sync::Condvar::new(),
        turns: Arc::default(),
        offline: Arc::default(),
        direct_lan: direct_lan.then(|| root.join("no-tailscaled.sock")),
        directs: std::sync::Mutex::default(),
        instances,
        limited,
    });
    let supervisor = hosting.supervise(&root, &project, turns_rx, bridged);
    let (socket, repo) = (hosting.socket.clone(), hosting.repo.clone());

    // The first turn, then the bridge, on a runtime of their own thread.
    let (session, stop, bridge) = hosting.begin(if demo { DEMO_TITLE } else { TITLE }).unwrap_or_else(|e| panic!("{e}"));

    // With `demo`, a second turn, typed at the host: a tool call that runs
    // at once, its result and an answer (a transcript draws all three).
    // Then a third, held on its approval.
    let request = demo.then(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let d = rt.block_on(Client::connect(&socket, &repo, "test", true)).unwrap_or_else(|e| panic!("the host daemon does not answer: {}", connect_error(e)));
        let look = hosting.prompt(Some(&session), LOOK);
        let ended = rt.block_on(d.execute(look, drain())).unwrap_or_else(|e| panic!("the demo's second turn failed: {e:?}"));
        assert!(ended.is_some(), "the demo's second turn did not end");
        let (tx, rx) = std::sync::mpsc::channel();
        let cmd = hosting.prompt(Some(&session), MAKE);
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
    Fleet { api, relay, env: "development".into(), approve, session, approval, request, host: a, chain, user, root, project, hosting, supervisor: Some(supervisor), stop, bridge, _registry: registry, _mock: m }
}

/// A session of a test's still starting: counted down when dropped.
struct Starting<'a>(&'a Hosting);

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        let mut tests = self.0.tests.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        tests.starting -= 1;
        self.0.started.notify_all();
    }
}

/// How many of the turns `Hosting::turn` typed still run, by session, and
/// a signal when one ends.
type Turns = Arc<(std::sync::Mutex<std::collections::HashMap<String, usize>>, std::sync::Condvar)>;

/// A turn `Hosting::turn` typed, running: counted down when dropped, so a
/// turn whose thread panics still ends for `turn_ended`.
struct TypedTurn(Turns, String);

impl Drop for TypedTurn {
    fn drop(&mut self) {
        let (running, ended) = &*self.0;
        if let Some(n) = running.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(&self.1) {
            *n = n.saturating_sub(1);
        }
        ended.notify_all();
    }
}

/// A bridge the fleet runs: what stops it, and its thread.
type Bridge = (watch::Sender<bool>, std::thread::JoinHandle<()>);

/// Host A as it hosts a session: its daemon, its keys, the registry and
/// the relay. The fleet's own session is hosted through it, and so is a
/// test's own (`host`), which the test lets go (`release`) to see a
/// session's lease lapse (R-IOS-7) without taking the fleet's host away
/// from every other test.
pub struct Hosting {
    api: String,
    relay: String,
    socket: PathBuf,
    repo: PathBuf,
    sessions: PathBuf,
    env: Arc<dyn Fn(&str) -> String + Send + Sync>,
    host: Device,
    chain: Chain,
    user: UserKey,
    /// The sessions hosted for tests, and a signal when one has finished
    /// starting.
    tests: std::sync::Mutex<Tests>,
    started: std::sync::Condvar,
    turns: Turns,
    /// Host A's announcement is not refreshed while set (`offline`).
    offline: Arc<AtomicBool>,
    /// With `--direct-lan`, where tailscaled is not (no Tailscale): every
    /// bridge offers its LAN listener on 127.0.0.1.
    direct_lan: Option<PathBuf>,
    /// What stops each session's direct listener, by session.
    directs: std::sync::Mutex<std::collections::HashMap<String, watch::Sender<bool>>>,
    /// Host A's instances, as its daemon and its announcement resolve them.
    instances: InstancesConfig,
    /// The calls on `anthropic` still to answer 429 (`limit`).
    limited: Arc<AtomicUsize>,
}

/// The sessions hosted for tests, by id; how many are still starting; and
/// whether the fleet is stopping, which refuses a new one and waits for
/// those starting.
#[derive(Default)]
struct Tests {
    stopping: bool,
    starting: usize,
    bridges: std::collections::HashMap<String, Bridge>,
    /// Every test session's title, hosted or let go: what `host_again`
    /// hosts it under.
    titles: std::collections::HashMap<String, String>,
}

impl Hosting {
    /// A new session titled `title`, its first turn run and a bridge
    /// holding its lease: its id, once the registry shows it leased and
    /// written.
    pub fn host(&self, title: &str) -> Result<String, String> {
        {
            let mut tests = self.tests.lock().unwrap();
            if tests.stopping {
                return Err("the fleet is stopping".into());
            }
            tests.starting += 1;
        }
        // Counted down however `begin` ends, a panic included.
        let starting = Starting(self);
        let (session, stop, bridge) = self.begin(title)?;
        let mut tests = self.tests.lock().unwrap();
        if tests.stopping {
            // `release_all` may have stopped waiting: stopped here, then.
            drop(tests);
            let _ = stop.send(true);
            let _ = bridge.join();
            drop(starting);
            return Err("the fleet is stopping".into());
        }
        tests.bridges.insert(session.clone(), (stop, bridge));
        tests.titles.insert(session.clone(), title.into());
        drop(tests);
        drop(starting);
        Ok(session)
    }

    /// Hosts `session`, a test's own that `release` let go, again (R-HAND-4:
    /// a host coming back): a bridge holds its lease once more, and every
    /// viewer waiting on it is welcomed. Returns once the registry shows it
    /// leased.
    pub fn host_again(&self, session: &str) -> Result<(), String> {
        let title = {
            let mut tests = self.tests.lock().unwrap();
            if tests.stopping {
                return Err("the fleet is stopping".into());
            }
            if tests.bridges.contains_key(session) {
                return Err(format!("the fleet hosts {session} already"));
            }
            let title = tests.titles.get(session).cloned().ok_or_else(|| format!("the fleet hosted no test session {session}"))?;
            tests.starting += 1;
            title
        };
        let starting = Starting(self);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let (stop, bridge) = self.bridge(rt, session, &title, None)?;
        let keys = UserKeys::new(self.user.clone(), []).unwrap();
        if let Err(e) = synced(&self.host.client(&self.api), session, &keys, &self.chain) {
            let _ = stop.send(true);
            let _ = bridge.join();
            return Err(e);
        }
        let mut tests = self.tests.lock().unwrap();
        if tests.stopping || tests.bridges.contains_key(session) {
            drop(tests);
            let _ = stop.send(true);
            let _ = bridge.join();
            drop(starting);
            return Err(format!("the fleet is stopping, or hosts {session} already"));
        }
        tests.bridges.insert(session.into(), (stop, bridge));
        drop(tests);
        drop(starting);
        Ok(())
    }

    /// Closes `session`'s direct listener (with `--direct-lan`), as a LAN
    /// path lost would: its viewers there go back to the relay, and those
    /// racing it later find nothing listening. The bridge, the relay and
    /// the lease stay.
    pub fn close_direct(&self, session: &str) -> Result<(), String> {
        if self.direct_lan.is_none() {
            return Err("the fleet offers no direct path: start it with --direct-lan".into());
        }
        let directs = self.directs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let stop = directs.get(session).ok_or_else(|| format!("the fleet hosts no session {session}"))?;
        let _ = stop.send(true);
        Ok(())
    }

    /// Stops the bridge of `session`, a test's own, which lets its lease
    /// go; returns once the registry shows no lease on it.
    pub fn release(&self, session: &str) -> Result<(), String> {
        let (stop, bridge) = self.tests.lock().unwrap().bridges.remove(session).ok_or_else(|| format!("the fleet hosts no test session {session}"))?;
        let _ = stop.send(true);
        // Its direct listener goes with it; `host_again` starts another.
        if let Some(direct) = self.directs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(session) {
            let _ = direct.send(true);
        }
        let _ = bridge.join();
        let api = self.host.client(&self.api);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match api.show_sync_session(session) {
                Ok(s) if s.lease.is_none() => return Ok(()),
                Ok(_) if Instant::now() >= deadline => return Err(format!("session {session} is still leased 10 s after its bridge stopped")),
                Err(e) if Instant::now() >= deadline => return Err(format!("session {session} cannot be read: {e}")),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// A turn in `session`, a test's own, typed at host A: `text` sent to
    /// the model on a thread of its own, its lines read and dropped.
    /// Returns once the turn's first line came (the daemon took the prompt),
    /// or with why it didn't within 10 s; the turn runs on, and the
    /// session's bridge hands it to every viewer live.
    pub fn turn(&self, session: &str, text: &str) -> Result<(), String> {
        if !self.tests.lock().unwrap().bridges.contains_key(session) {
            return Err(format!("the fleet hosts no test session {session}"));
        }
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let d = rt.block_on(Client::connect(&self.socket, &self.repo, "test", true)).map_err(|e| format!("the host daemon does not answer: {}", connect_error(e)))?;
        let cmd = self.prompt(Some(session), text);
        let (said, heard) = std::sync::mpsc::channel::<Result<(), String>>();
        let (out, mut lines) = mpsc::channel(4096);
        let first = said.clone();
        std::thread::spawn(move || {
            let mut first = Some(first);
            while lines.blocking_recv().is_some() {
                if let Some(f) = first.take() {
                    let _ = f.send(Ok(()));
                }
            }
        });
        *self.turns.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(session.to_string()).or_default() += 1;
        let typed = TypedTurn(self.turns.clone(), session.to_string());
        std::thread::spawn(move || {
            let _typed = typed;
            match rt.block_on(d.execute(cmd, out)) {
                // Ended with no line seen first: started all the same.
                Ok(_) => {
                    let _ = said.send(Ok(()));
                }
                Err(e) => {
                    eprintln!("fleet: a test's turn failed: {e:?}");
                    let _ = said.send(Err(format!("the turn failed: {e:?}")));
                }
            }
        });
        heard.recv_timeout(Duration::from_secs(10)).map_err(|_| "the turn did not start within 10 s".to_string())?
    }

    /// Waits, up to `wait`, until every turn `turn` typed in `session`, a
    /// test's own, has ended at host A (the host daemon finished it; the
    /// bridge hands its last lines on after): `true` once they have,
    /// `false` if one still runs. A session with none running has nothing
    /// to wait for. How long a turn takes is the host's (a starved runner
    /// streams slower); a test asks this until `true`, then checks only how
    /// soon its viewer shows the end.
    pub fn turn_ended(&self, session: &str, wait: Duration) -> Result<bool, String> {
        if !self.tests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).titles.contains_key(session) {
            return Err(format!("the fleet hosts no test session {session}"));
        }
        let (running, ended) = &*self.turns;
        let guard = running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (still, _) = ended.wait_timeout_while(guard, wait, |r| r.get(session).is_some_and(|n| *n > 0)).unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(still.get(session).is_none_or(|n| *n == 0))
    }

    /// Host A's automatic sync (R-SYNC-3), as `krowk host serve` runs it
    /// beside the daemon, and its announcement of itself (R-HOST-3): the
    /// mock model's instance ready, and `project` the one directory, which
    /// `trusted.json` in host A's home lists. The fleet's own sessions, in
    /// `repo`, are excluded: the fleet bridges those itself. A session
    /// another device starts (R-HOST-4) runs in `project` and is taken up
    /// at its first turn.
    fn supervise(&self, root: &Path, project: &Path, turns: mpsc::UnboundedReceiver<daemon::server::TurnEvent>, bridged: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>) -> auto::Handle {
        let krowk = root.join("home/.krowk");
        let trusted = krowk_harness::trust::Store::new(Some(krowk.join(krowk_harness::trust::FILE)), Some(root.join("home")));
        trusted.trust(&project.canonicalize().unwrap()).expect("host A's home trusts the fleet's project");
        let excluded = serde_json::json!({"sync": {"exclude": [self.repo.display().to_string()]}});
        let announcer = {
            let (api, token, signing, device, krowk, offline) = (self.api.clone(), self.host.token.clone(), self.host.signing_copy(), self.host.key.id(), krowk.clone(), self.offline.clone());
            move |_fresh: bool| -> Result<announce::Announcer, (String, String)> {
                if offline.load(Ordering::SeqCst) {
                    return Err(("fleet_offline".into(), "the fleet took host A offline (POST /hosts/offline)".into()));
                }
                let signer = e2e::DeviceSigner::new(device, SigningKey::from_secret(&*signing.secret_bytes()).unwrap()).shared();
                let api = Arc::new(krowk_api::Client::new(&api, &token).signed_by(signer));
                let (keys, chain) = listed(&api, &krowk)?;
                Ok(announce::Announcer { api, keys, chain, device })
            }
        };
        let build: auto::Build = {
            let (relay, api, token, signing, device, krowk, sessions) = (self.relay.clone(), self.api.clone(), self.host.token.clone(), self.host.signing_copy(), self.host.key.id(), krowk.clone(), self.sessions.clone());
            Box::new(move |session: &str, cwd: &Path| {
                let signer = e2e::DeviceSigner::new(device, SigningKey::from_secret(&*signing.secret_bytes()).unwrap()).shared();
                let api = Arc::new(krowk_api::Client::new(&api, &token).signed_by(signer));
                let (keys, chain) = listed(&api, &krowk)?;
                Ok(host::Options {
                    relay: relay.clone(),
                    env: "development".into(),
                    api,
                    device,
                    signing: SigningKey::from_secret(&*signing.secret_bytes()).unwrap(),
                    keys,
                    chain,
                    session: session.into(),
                    title: String::new(),
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
            })
        };
        let (env, cfg) = (self.env.clone(), self.instances.clone());
        let c = auto::Config {
            socket: self.socket.clone(),
            sessions: self.sessions.clone(),
            version: "test".into(),
            krowk_home: Some(krowk.clone()),
            build,
            policy: Box::new(move || auto::policy(&|_| String::new(), Some(&excluded))),
            ready: Box::new(|| Ok(())),
            bridged,
            paid_pause: auto::PAID_PAUSE,
            parked_recheck: auto::PARKED_RECHECK,
            announce: Some(announce::Options {
                every: ANNOUNCE_EVERY,
                ttl: ANNOUNCE_TTL,
                online_since_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64),
                keys: Box::new(announcer),
                instances: Box::new(move || {
                    let registry = Registry::resolve(&cfg, &*env);
                    let all: Vec<_> = registry.instances.values().collect();
                    let probe = krowk_harness::readiness::Probe::at(krowk.join("probe"));
                    let reports = krowk_harness::readiness::check_all(&all, &krowk.join("credentials.json"), &probe);
                    (registry, reports)
                }),
                catalog: Box::new(|| None),
                trusted,
                mode_for: Box::new(|_| Ok(PermissionMode::Default)),
                take: None,
            }),
        };
        auto::Supervisor::spawn(c, turns)
    }

    /// Takes host A offline as a machine that went to sleep is (R-IOS-7):
    /// its announcement is no longer refreshed and lapses, still listed,
    /// offline, with when it was last seen. Its sessions, the registry and
    /// the relay stay. Returns once the registry lists it offline.
    pub fn offline(&self) -> Result<(), String> {
        self.offline.store(true, Ordering::SeqCst);
        self.announced(false, Duration::from_secs(ANNOUNCE_TTL + 20))
    }

    /// Holds host A's refreshes, so a start sent now is not delivered
    /// until `online`: a test sees its phone's `starting on host-a…` row
    /// for as long as it likes. Returns at once; the announcement stays
    /// listed online until it lapses, `ANNOUNCE_TTL` seconds after the last
    /// refresh.
    pub fn hold(&self) {
        self.offline.store(true, Ordering::SeqCst);
    }

    /// The next turn on `anthropic` hits its limit (R-INST-7): the mock
    /// answers its calls 429 with `retry-after: 0`, every attempt of one
    /// call, and the turn ends failed with an offer of `PERSONAL`. Later
    /// calls answer as before.
    pub fn limit(&self) {
        self.limited.store(krowk_harness::http::ATTEMPTS as usize, Ordering::SeqCst);
    }

    /// Host A announces itself again; returns once it is listed online.
    pub fn online(&self) -> Result<(), String> {
        self.offline.store(false, Ordering::SeqCst);
        self.announced(true, Duration::from_secs(20))
    }

    /// Waits up to `most` until the registry lists host A's announcement
    /// as `online` says.
    fn announced(&self, online: bool, most: Duration) -> Result<(), String> {
        let (api, me) = (self.host.client(&self.api), self.host.key.id().to_string());
        let deadline = Instant::now() + most;
        loop {
            let listed = api.list_presences().map_err(|e| e.to_string())?;
            if listed.iter().any(|p| p.device == me && p.online == online) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("host A is not listed {} within {most:?}", if online { "online" } else { "offline" }));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Stops every test's session, and any that starts from now on.
    fn release_all(&self) {
        let all: Vec<Bridge> = {
            let mut tests = self.tests.lock().unwrap();
            tests.stopping = true;
            // A session starting takes up to the bridge's sync wait.
            let (mut tests, _) = self.started.wait_timeout_while(tests, Duration::from_secs(30), |t| t.starting > 0).unwrap();
            tests.bridges.drain().map(|(_, b)| b).collect()
        };
        for (stop, _) in &all {
            let _ = stop.send(true);
        }
        for (_, bridge) in all {
            let _ = bridge.join();
        }
        for (_, direct) in self.directs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain() {
            let _ = direct.send(true);
        }
    }

    /// A prompt to host A's daemon, for the mock model.
    fn prompt(&self, session: Option<&str>, text: &str) -> Command {
        let model = Registry::resolve(&self.instances, &*self.env).parse_model("claude-sonnet-4-6").unwrap();
        Command::Prompt { session_id: session.map(String::from), text: text.into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }
    }

    /// Runs a first turn at host A, then a bridge for its session on a
    /// thread of its own, and waits until the registry shows it synced.
    fn begin(&self, title: &str) -> Result<(String, watch::Sender<bool>, std::thread::JoinHandle<()>), String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let connect = || rt.block_on(Client::connect(&self.socket, &self.repo, "test", true)).map_err(|e| format!("the host daemon does not answer: {}", connect_error(e)));
        let d = connect()?;
        let first = self.prompt(None, HELLO);
        let session = rt.block_on(async { d.execute(first, drain()).await }).map_err(|e| format!("the first turn failed: {e:?}"))?.ok_or("the first turn did not end")?.session_id;
        let (stop, bridge) = self.bridge(rt, &session, title, Some(d))?;
        let keys = UserKeys::new(self.user.clone(), []).unwrap();
        if let Err(e) = synced(&self.host.client(&self.api), &session, &keys, &self.chain) {
            let _ = stop.send(true);
            let _ = bridge.join();
            return Err(e);
        }
        Ok((session, stop, bridge))
    }

    /// A bridge for `session` on a thread of its own, running on `rt`,
    /// connected to host A's daemon: what stops it, and its thread. `keep`,
    /// the connection that ran the session's first turn, is held as long.
    fn bridge(&self, rt: tokio::runtime::Runtime, session: &str, title: &str, keep: Option<Client>) -> Result<Bridge, String> {
        let bridged = rt.block_on(Client::connect(&self.socket, &self.repo, "test", true)).map_err(|e| format!("the host daemon does not answer: {}", connect_error(e)))?;
        let (stop, stop_rx) = watch::channel(false);
        let o = host::Options {
            relay: self.relay.clone(),
            env: "development".into(),
            api: self.host.client(&self.api),
            device: self.host.key.id(),
            signing: self.host.signing_copy(),
            keys: UserKeys::new(self.user.clone(), []).unwrap(),
            chain: self.chain.clone(),
            session: session.into(),
            title: title.into(),
            cwd: self.repo.display().to_string(),
            sessions: self.sessions.clone(),
            ttl: host::LEASE_TTL,
            keep: host::KEEP,
            direct: self.direct_lan.as_ref().map(|socket| {
                let (stop, stopped) = watch::channel(false);
                self.directs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(session.into(), stop);
                let lan_at: krowk_harness::sync::direct::LanAt = Arc::new(|| vec![std::net::Ipv4Addr::LOCALHOST.into()]);
                krowk_harness::sync::direct::Config { socket: socket.clone().into(), roster: roster(), same_user: false, lan: true, lan_at: Some(lan_at), stop: Some(stopped), whois_kept: None }
            }),
            tailnet: None,
            handed: None,
            approval_wait: host::APPROVAL_WAIT,
            stale: None,
            supervised: None,
        };
        let bridge = std::thread::spawn(move || {
            let (_cp, cp_rx) = mpsc::unbounded_channel();
            if let Err(e) = rt.block_on(host::run(o, Arc::new(bridged), stop_rx, cp_rx)) {
                eprintln!("fleet: the bridge ended: {e}");
            }
            drop(keep);
        });
        Ok((stop, bridge))
    }
}

/// The device list the registry serves, verified from its first entry, and
/// the user keys host A's home keeps: a device paired, or removed, since
/// the fleet started is on it, or off it. Errors are `(code, message)`.
fn listed(api: &krowk_api::Client, krowk: &Path) -> Result<(UserKeys, Chain), (String, String)> {
    let failed = |e: String| ("list_unread".to_string(), e);
    let list = api.device_list_all().map_err(|e| failed(e.to_string()))?;
    let entries = list
        .entries
        .iter()
        .map(|e| krowk_client::device_chain::SignedEntry::from_parts(e2e::unhex(&e.entry).ok_or("not hex")?, &e2e::unhex(&e.signatures).ok_or("not hex")?).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, String>>()
        .map_err(failed)?;
    let chain = Chain::verify(&entries, None).map_err(|e| failed(e.to_string()))?;
    let keys = Keystore::new(krowk).user_keys().map_err(failed)?.ok_or_else(|| failed("host A's home keeps no user key".into()))?;
    Ok((keys, chain))
}

/// `dir` made a git repository: `git init`, or, where git isn't, a `.git`
/// of its own, which is all krowk looks for.
fn git_init(dir: &Path) {
    let made = Process::new("git").args(["init", "-q"]).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !made {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }
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

/// The reference relay, trusting the stand-in registry's ticket key, at
/// the origin it prints, as the hosted relay is: not the request's `Host`,
/// which Network.framework's WebSocket sends without the port, so a phone
/// signing the address it dialed would never verify.
fn relay() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("ws://{}", listener.local_addr().unwrap());
    let config = krowk_harness::relay::Config { roster: roster(), origin: Some(origin.clone()), limits: Default::default(), state: None, origins: Vec::new(), whois: None, pin: None };
    std::thread::spawn(move || krowk_harness::relay::run(listener, config));
    origin
}

/// The stand-in registry's ticket key, as the relay and the direct
/// listeners trust it.
fn roster() -> krowk_harness::relay::Roster {
    let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
    krowk_harness::relay::Roster::parse(&roster).unwrap()
}

/// The environment host A's daemon reads: its home and runtime directory
/// under the scratch root, and the mock model for Anthropic.
fn env(root: &Path, model: &str) -> impl Fn(&str) -> String + Clone + Send + Sync + 'static {
    let (root, url) = (root.to_path_buf(), model.to_string());
    move |k| match k {
        "HOME" => root.join("home").display().to_string(),
        "XDG_RUNTIME_DIR" => root.join("run").display().to_string(),
        "ANTHROPIC_API_KEY" => KEY.into(),
        PERSONAL_KEY_ENV => PERSONAL_KEY.into(),
        "ANTHROPIC_BASE_URL" => url.clone(),
        _ => String::new(),
    }
}

/// Host A's daemon on a thread of its own, once it listens, telling
/// `turns` of each turn (automatic sync's hook).
fn serve(root: &Path, env: impl Fn(&str) -> String + Clone + Send + Sync + 'static, instances: InstancesConfig, turns: mpsc::UnboundedSender<daemon::server::TurnEvent>, bridged: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>) {
    let socket = daemon::socket(&env).unwrap();
    let credentials = root.join("home/.krowk/credentials.json");
    let wait = socket.clone();
    std::thread::spawn(move || {
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
        server::run(server::Options { socket, idle: None, krowk_version: "test".into(), turns: Some(turns), bridged: Some(bridged), ..Default::default() }, factory)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::os::unix::net::UnixStream::connect(&wait).is_err() {
        assert!(Instant::now() < deadline, "the host daemon never listened");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until the session's sealed index names a lease, a checkpoint and
/// a head: the bridge has taken the session up and written it. Up to a
/// minute: a debug host on GitHub's busy macOS runner has taken longer
/// than 10 s to write its first checkpoint, and the fleet never started.
fn synced(api: &krowk_api::Client, session: &str, keys: &UserKeys, chain: &Chain) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
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
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= deadline => return Err(format!("the bridge never synced the session: {e}")),
            Err(_) => {}
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

    /// `krowk devices remove <name>` as host A, saying yes (R-E2E-3): the
    /// device is off the list, its keys are refused, and the user key
    /// rotates to the devices left. The running `devices add` is ended
    /// first: its code was for the list as it was, so the next pairing
    /// asks for a fresh one. Its lines, on failure, are the error.
    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        self.end();
        let out = self.krowk(&["devices", "remove", name]).env("KROWK_TEST_ANSWERS", "y").output().map_err(|e| format!("cannot run KROWK_BIN={}: {e}", self.bin.display()))?;
        let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
        eprintln!("fleet: krowk devices remove: {said}");
        if out.status.success() { Ok(()) } else { Err(format!("`krowk devices remove` as host A failed ({}): {said}", out.status)) }
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
