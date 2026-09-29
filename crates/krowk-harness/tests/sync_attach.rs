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
use krowk_harness::sync::{host, viewer};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
}

struct Device {
    key: DeviceKey,
    signing: SigningKey,
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
        std::thread::spawn(move || krowk_harness::relay::run(relay, krowk_harness::relay::Config { roster, origin: None, limits: Default::default(), state: None }));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cut = Arc::new(AtomicBool::new(false));
        let relay_a = proxy(relay_addr, seen.clone(), cut.clone());
        let relay_b = proxy(relay_addr, seen.clone(), Arc::new(AtomicBool::new(false)));
        let m = mock::serve(model);
        let mock_url = m.url.clone();
        World { root, registry, api, account: AccountKey::generate(), seen, cut, relay_a: format!("ws://{relay_a}"), relay_b: format!("ws://{relay_b}"), _mock: m, mock_url }
    }

    fn client(&self) -> Arc<krowk_api::Client> {
        Arc::new(krowk_api::Client::new(&self.api, "krowk_sk_sync_attach_0000000000000000"))
    }

    /// A device of the workspace, registered with its signing key.
    fn device(&self, name: &str) -> Device {
        let d = Device { key: DeviceKey::generate(), signing: SigningKey::generate() };
        self.client().register_device(&e2e::hex(&d.key.public().0), &e2e::hex(&d.signing.public().0), name, &self.account.id().to_string()).unwrap();
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
        let o = host::Options {
            relay: self.relay_a.clone(),
            env: "development".into(),
            api: self.client(),
            device: a.key.id(),
            signing: SigningKey::from_secret(&*a.signing.secret_bytes()).unwrap(),
            account: AccountKey::from_bytes(*self.account.as_bytes()),
            session: session.into(),
            title: "the title is sealed too".into(),
            cwd: self.repo().display().to_string(),
            ttl: host::LEASE_TTL,
        };
        let (stop, stop_rx) = watch::channel(false);
        let (cp, cp_rx) = mpsc::unbounded_channel();
        (stop, cp, tokio::spawn(host::run(o, daemon, stop_rx, cp_rx)))
    }

    fn viewer(&self, b: &Device, session: &str) -> viewer::Options {
        viewer::Options {
            relay: self.relay_b.clone(),
            env: "development".into(),
            api: self.client(),
            device: b.key.id(),
            signing: SigningKey::from_secret(&*b.signing.secret_bytes()).unwrap(),
            account: AccountKey::from_bytes(*self.account.as_bytes()),
            session: session.into(),
            known: None,
        }
    }

    /// Waits until the session's sealed index names a checkpoint: the
    /// bridge has taken the session up.
    async fn synced(&self, session: &str) {
        let api = self.client();
        let id = session.to_string();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (api, id) = (api.clone(), id.clone());
            let s = tokio::task::spawn_blocking(move || api.show_sync_session(&id)).await.unwrap();
            if s.is_ok_and(|s| !s.sealed_index.is_empty() && s.lease.is_some()) {
                tokio::time::sleep(Duration::from_millis(300)).await;
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
