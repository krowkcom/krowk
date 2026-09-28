//! The host daemon over its unix socket, against a stand-in Anthropic API
//! that types slowly: a turn outlives the client that asked for it, a
//! client that attaches late sees the turn so far and then the rest, two
//! clients on one session see the same frames, and the daemon goes away
//! once it has nothing to do (R-HOST-1, R-PROTO-1). Each test runs its own
//! daemon on a thread, in a runtime directory of its own.

#![cfg(unix)]

#[path = "common/mock.rs"]
mod mock;

use krowk_harness::daemon::{self, client::Client, server};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{Command, Delta, Item, LiveEvent, LogBody, LogEvent, PermissionMode, StreamLine, TurnStatus};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

struct Home {
    root: PathBuf,
    url: String,
}

impl Home {
    fn new(name: &str, url: &str) -> Home {
        let root = std::env::temp_dir().join(format!("krowk-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run", "repo/.git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Home { root, url: url.into() }
    }

    fn env(&self) -> impl Fn(&str) -> String + Clone + Send + 'static {
        let (root, url) = (self.root.clone(), self.url.clone());
        move |k| match k {
            "HOME" => root.join("home").display().to_string(),
            "XDG_RUNTIME_DIR" => root.join("run").display().to_string(),
            "ANTHROPIC_API_KEY" => "sk-test".into(),
            "ANTHROPIC_BASE_URL" => url.clone(),
            _ => String::new(),
        }
    }

    fn socket(&self) -> PathBuf {
        daemon::socket(&self.env()).unwrap()
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    /// Starts the daemon on a thread of its own; joined, it has exited.
    fn serve(&self, idle: Option<Duration>) -> std::thread::JoinHandle<Result<(), String>> {
        let (env, socket) = (self.env(), self.socket());
        let credentials = self.root.join("home/.krowk/credentials.json");
        let t = std::thread::spawn(move || {
            let factory: server::Factory = Box::new(move |cwd: &Path| {
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
                    permissions: Default::default(),
                    agents: krowk_harness::subagent::AgentsConfig::none(),
                })
            });
            server::run(server::Options { socket, idle, krowk_version: "test".into() }, factory)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(self.socket()).is_err() {
            assert!(std::time::Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(5));
        }
        t
    }

    async fn client(&self) -> Client {
        match Client::connect(&self.socket(), &self.repo(), "test").await {
            Ok(c) => c,
            Err(_) => panic!("no daemon answered"),
        }
    }

    fn prompt(&self, text: &str) -> Command {
        let model = Registry::resolve(&InstancesConfig::default(), &self.env()).parse_model("claude-sonnet-4-6").unwrap();
        Command::Prompt { session_id: None, text: text.into(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const ANSWER: &str = "one two three four five six seven eight nine ten eleven twelve";

/// A provider that types `ANSWER` a word every 40 ms.
fn slow() -> mock::Mock {
    mock::serve(|_, _| mock::Reply::paced(mock::text_stream(ANSWER), Duration::from_millis(40)))
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// Reads `rx` until the session's `result`.
async fn to_result(rx: &mut mpsc::Receiver<StreamLine>, into: &mut Vec<StreamLine>) {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("the turn never ended").expect("the stream closed first");
        let done = matches!(line, StreamLine::Live(LiveEvent::Result(_)));
        into.push(line);
        if done {
            return;
        }
    }
}

fn typed(lines: &[StreamLine]) -> String {
    lines
        .iter()
        .filter_map(|l| match l {
            StreamLine::Live(LiveEvent::ItemDelta { delta: Delta::Text { text }, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn answered(lines: &[StreamLine]) -> Option<String> {
    lines.iter().find_map(|l| match l {
        StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::AssistantText { text }, .. }, .. }) => Some(text.clone()),
        _ => None,
    })
}

#[test]
fn r_host_1_a_turn_outlives_its_client_and_a_new_one_reattaches_mid_stream() {
    let m = slow();
    let home = Home::new("reattach", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    let (session, first, later) = rt.block_on(async {
        // The first client starts the turn, reads three words, and goes
        // away as a closed terminal does: without a word to the daemon.
        let a = home.client().await;
        let (tx, mut rx) = mpsc::channel(1024);
        let cmd = home.prompt("count to twelve");
        let mut exec = Box::pin(a.execute(cmd, tx));
        let mut first = Vec::new();
        loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    first.push(line);
                    if typed(&first).split_whitespace().count() >= 3 {
                        break;
                    }
                }
                _ = &mut exec => panic!("the turn ended before the client left"),
            }
        }
        drop(exec);
        drop(a);
        let session = first[0].session_id().to_string();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // A new client, as `krowk` opened again, attaches mid-turn.
        let b = home.client().await;
        let (tx, mut rx) = mpsc::channel(1024);
        let running = b.attach(&session, None, tx).await.unwrap();
        assert!(running, "the turn is still under way in the daemon");
        let mut later = Vec::new();
        to_result(&mut rx, &mut later).await;
        (session, first, later)
    });
    // It was caught up from the start of the session — the log — and on
    // the turn so far, the typing of the reply under way included, then
    // given the rest live: every word, once, in order.
    assert!(matches!(&later[0], StreamLine::Log(LogEvent { body: LogBody::SessionStarted { .. }, .. })), "{:?}", later[0]);
    assert_eq!(typed(&later), ANSWER, "the typing so far and the rest, nothing twice");
    assert_eq!(answered(&later).as_deref(), Some(ANSWER));
    assert!(typed(&first).len() < ANSWER.len(), "the first client left mid-reply");
    let StreamLine::Live(LiveEvent::Result(r)) = later.last().unwrap() else { unreachable!() };
    assert_eq!((r.status, r.session_id.as_str()), (TurnStatus::Completed, session.as_str()));
    // And the log has the turn whole: the daemon finished it for nobody.
    let events = log::read_events(&log::sessions_dir(&home.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
    assert!(events.iter().any(|e| matches!(e.body, LogBody::TurnCompleted { status: TurnStatus::Completed, .. })));
    // With no client left — its runtime gone, as its process would be —
    // the daemon exits after its window.
    drop(rt);
    daemon.join().unwrap().unwrap();
}

#[test]
fn r_proto_1_two_clients_on_one_session_see_identical_frames() {
    let m = slow();
    let home = Home::new("two", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    let (a_lines, b_lines) = rt.block_on(async {
        let a = home.client().await;
        let b = home.client().await;
        let (atx, mut arx) = mpsc::channel(1024);
        let exec = a.execute(home.prompt("count to twelve"), atx);
        tokio::pin!(exec);
        let mut a_lines = Vec::new();
        let mut b_lines = Vec::new();
        let (btx, mut brx) = mpsc::channel(1024);
        // B attaches once A has seen a word: some frames it is caught up
        // on, the rest it sees live.
        let mut attached = false;
        let r = loop {
            tokio::select! {
                Some(line) = arx.recv() => {
                    a_lines.push(line);
                    if !attached && !typed(&a_lines).is_empty() {
                        attached = true;
                        b.attach(a_lines[0].session_id(), None, btx.clone()).await.unwrap();
                    }
                }
                r = &mut exec => break r,
            }
        };
        assert_eq!(r.unwrap().unwrap().status, TurnStatus::Completed);
        while let Ok(l) = arx.try_recv() {
            a_lines.push(l);
        }
        to_result(&mut brx, &mut b_lines).await;
        (a_lines, b_lines)
    });
    let json = |l: &[StreamLine]| l.iter().map(|x| serde_json::to_string(x).unwrap()).collect::<Vec<_>>();
    assert_eq!(json(&a_lines), json(&b_lines), "the same frames, in the same order");
    drop(rt);
    daemon.join().unwrap().unwrap();
}

#[test]
fn r_host_1_the_daemon_exits_after_its_idle_window_and_not_while_a_client_is_on() {
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::text_stream("hi")));
    let home = Home::new("idle", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(400)));
    let rt = rt();
    rt.block_on(async {
        let c = home.client().await;
        let (tx, _rx) = mpsc::channel(1024);
        c.execute(home.prompt("hi"), tx).await.unwrap().unwrap();
        // A client connected holds it up, however long it idles.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let status = c.status().await.unwrap();
        assert_eq!(status.clients, 1);
        assert_eq!(status.sessions.len(), 1);
        assert_eq!(status.idle_exit_ms, Some(400));
    });
    drop(rt);
    let started = std::time::Instant::now();
    daemon.join().unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "it went once idle");
    assert!(!home.socket().exists(), "and took its socket with it: nothing stays resident");
}

#[test]
fn r_proto_1_a_client_of_another_protocol_version_is_refused_with_a_fix() {
    use std::io::{BufRead, Write};
    let home = Home::new("version", "http://127.0.0.1:9");
    let daemon = home.serve(Some(Duration::from_millis(200)));
    let mut s = std::os::unix::net::UnixStream::connect(home.socket()).unwrap();
    writeln!(s, r#"{{"type":"hello","protocolVersion":999,"cwd":"/"}}"#).unwrap();
    let mut line = String::new();
    std::io::BufReader::new(&s).read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "refused");
    assert_eq!(v["code"], "protocol_mismatch");
    assert!(v["fix"].as_str().unwrap().contains("kill"), "{v}");
    drop(s);
    daemon.join().unwrap().unwrap();
}

#[test]
fn r_host_1_two_krowks_starting_at_once_spawn_one_daemon_over_a_stale_socket() {
    let home = Home::new("race", "http://127.0.0.1:9");
    // A socket a daemon that died left behind: nothing answers on it.
    drop(std::os::unix::net::UnixListener::bind(home.socket()).unwrap());
    assert!(home.socket().exists());
    let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let daemons = Arc::new(std::sync::Mutex::new(Vec::new()));
    let home = Arc::new(home);
    let starters: Vec<_> = (0..2)
        .map(|_| {
            let (home, spawned, daemons) = (home.clone(), spawned.clone(), daemons.clone());
            std::thread::spawn(move || {
                let env = home.env();
                let spawn = || {
                    spawned.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    daemons.lock().unwrap().push(home.serve(Some(Duration::from_millis(300))));
                    Ok(())
                };
                rt().block_on(async { daemon::ensure(&env, &home.repo(), "test", &spawn).await.map(|c| c.pid) }).unwrap()
            })
        })
        .collect();
    for s in starters {
        assert_eq!(s.join().unwrap(), std::process::id());
    }
    assert_eq!(spawned.load(std::sync::atomic::Ordering::SeqCst), 1, "one daemon for the two");
    for d in daemons.lock().unwrap().drain(..) {
        d.join().unwrap().unwrap();
    }
}
