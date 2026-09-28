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
    /// How many hosts' configurations the daemon has asked for.
    made: Arc<std::sync::atomic::AtomicUsize>,
}

impl Home {
    fn new(name: &str, url: &str) -> Home {
        let root = std::env::temp_dir().join(format!("krowk-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run", "repo/.git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Home { root, url: url.into(), made: Arc::default() }
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
        let made = self.made.clone();
        let t = std::thread::spawn(move || {
            let factory: server::Factory = Box::new(move |cwd: &Path, answers: bool| {
                made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            server::run(server::Options { socket, idle, krowk_version: "test".into(), ..Default::default() }, factory)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(self.socket()).is_err() {
            assert!(std::time::Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(5));
        }
        t
    }

    async fn client(&self) -> Client {
        self.client_answering(false).await
    }

    async fn client_answering(&self, answers: bool) -> Client {
        match Client::connect(&self.socket(), &self.repo(), "test", answers).await {
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
                    Ok(None)
                };
                rt().block_on(async { daemon::ensure(&env, &home.repo(), "test", false, &spawn).await.map(|c| c.pid) }).unwrap()
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

/// A model that runs one bash command, then answers once it has the result.
fn runs_bash() -> mock::Mock {
    mock::serve(|body, _| {
        let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
        if last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result")) {
            mock::Reply::sse(&mock::text_stream("Done."))
        } else {
            mock::Reply::sse(&mock::tool_use("toolu_01Call", "bash", &serde_json::json!({"command": "touch made-by-the-model"})))
        }
    })
}

fn tool_result(lines: &[StreamLine]) -> Option<(String, bool)> {
    lines.iter().find_map(|l| match l {
        StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, is_error, .. }, .. }, .. }) => Some((output.clone(), *is_error)),
        _ => None,
    })
}

/// R-HOST-1: a `krowk -p` client answers no approval request, so its turn
/// in the daemon refuses what would be asked exactly as in-process `-p`
/// does — never waiting on a person nobody will be.
#[test]
fn r_host_1_a_p_clients_turn_refuses_what_would_be_asked_as_in_process() {
    let m = runs_bash();
    let home = Home::new("refuse", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    let lines = rt.block_on(async {
        let c = home.client().await;
        let (tx, mut rx) = mpsc::channel(1024);
        let r = tokio::time::timeout(Duration::from_secs(10), c.execute(home.prompt("make a file"), tx)).await.expect("the turn never waits on a person").unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Completed);
        let mut lines = Vec::new();
        while let Ok(l) = rx.try_recv() {
            lines.push(l);
        }
        lines
    });
    assert!(!lines.iter().any(|l| matches!(l, StreamLine::Live(LiveEvent::ApprovalRequested(_)))), "nobody was asked");
    let (output, is_error) = tool_result(&lines).unwrap();
    assert!(is_error && output.contains("--permission-mode"), "the in-process refusal, with what would allow it: {output}");
    assert!(!home.repo().join("made-by-the-model").exists());
    drop(rt);
    daemon.join().unwrap().unwrap();
}

/// R-HOST-1: a turn waiting on an answer from a client that went away is
/// answered `deny`, so it ends, and the daemon can go.
#[test]
fn r_host_1_a_request_left_with_no_client_to_answer_it_is_denied() {
    let m = runs_bash();
    let home = Home::new("leave", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    let session = rt.block_on(async {
        let a = home.client_answering(true).await;
        let (tx, mut rx) = mpsc::channel(1024);
        let mut exec = Box::pin(a.execute(home.prompt("make a file"), tx));
        let session = loop {
            tokio::select! {
                Some(line) = rx.recv() => if let StreamLine::Live(LiveEvent::ApprovalRequested(r)) = line { break r.session_id },
                _ = &mut exec => panic!("the turn ended without asking"),
            }
        };
        // The terminal closes with the question on screen.
        drop(exec);
        drop(a);
        session
    });
    drop(rt);
    // No client left: the request is denied, the turn ends, and the
    // daemon exits after its window.
    daemon.join().unwrap().unwrap();
    let events = log::read_events(&log::sessions_dir(&home.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
    assert!(events.iter().any(|e| matches!(e.body, LogBody::TurnCompleted { .. })), "the turn ended");
    let lines: Vec<StreamLine> = events.into_iter().map(StreamLine::Log).collect();
    let (output, is_error) = tool_result(&lines).unwrap();
    assert!(is_error, "{output}");
    assert!(!home.repo().join("made-by-the-model").exists());
}

/// R-HOST-2: `krowk host enable` hands the socket over to the service by
/// asking the daemon krowk started to stop — refused while a turn runs.
#[test]
fn r_host_2_a_daemon_stops_when_asked_unless_a_turn_runs() {
    let m = slow();
    let home = Home::new("stop", &m.url);
    let daemon = home.serve(None);
    let rt = rt();
    rt.block_on(async {
        let a = home.client().await;
        let (tx, mut rx) = mpsc::channel(1024);
        let mut exec = Box::pin(a.execute(home.prompt("count"), tx));
        loop {
            tokio::select! {
                Some(line) = rx.recv() => if matches!(line, StreamLine::Live(LiveEvent::ItemDelta { .. })) { break },
                _ = &mut exec => panic!("ended early"),
            }
        }
        let b = home.client().await;
        assert_eq!(b.stop(false).await.unwrap_err().code, "host_busy");
        exec.await.unwrap();
        // A's still connected: an open TUI, say.
        assert_eq!(b.stop(false).await.unwrap_err().code, "host_in_use");
        b.stop(true).await.unwrap();
    });
    drop(rt);
    // With no idle window at all, only the stop ends it.
    daemon.join().unwrap().unwrap();
    assert!(!home.socket().exists());
}

/// R-HOST-1: what the TUI reattaches with — a client following a running
/// turn gets its frames and its result as the client that sent it does;
/// one following a session with nothing running is told so at once.
#[test]
fn r_host_1_a_follower_gets_the_running_turns_frames_and_its_result() {
    let m = slow();
    let home = Home::new("follow", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    rt.block_on(async {
        let a = home.client().await;
        let (atx, mut arx) = mpsc::channel(1024);
        let mut exec = Box::pin(a.execute(home.prompt("count"), atx));
        let mut a_lines = Vec::new();
        loop {
            tokio::select! {
                Some(line) = arx.recv() => {
                    a_lines.push(line);
                    if !typed(&a_lines).is_empty() {
                        break;
                    }
                }
                _ = &mut exec => panic!("ended early"),
            }
        }
        let session = a_lines[0].session_id().to_string();
        let b = home.client_answering(true).await;
        let (btx, mut brx) = mpsc::channel(1024);
        let (ra, rb) = tokio::join!(&mut exec, b.follow(&session, None, btx));
        let (ra, rb) = (ra.unwrap().unwrap(), rb.unwrap().expect("the turn's result"));
        assert_eq!((rb.turn_id.as_str(), rb.status), (ra.turn_id.as_str(), TurnStatus::Completed));
        let mut b_lines = Vec::new();
        while let Ok(l) = brx.try_recv() {
            b_lines.push(l);
        }
        assert_eq!(typed(&b_lines), ANSWER, "every word, once");
        // Over: following it now answers at once, with nothing.
        let (ctx, _crx) = mpsc::channel(1024);
        assert!(b.follow(&session, None, ctx).await.unwrap().is_none());
    });
    drop(rt);
    daemon.join().unwrap().unwrap();
}

/// R-PROTO-1: a client running a turn in one session while following
/// another keeps their streams apart — each line goes to its session's.
#[test]
fn r_proto_1_a_client_in_two_sessions_keeps_their_streams_apart() {
    let m = slow();
    let home = Home::new("apart", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    rt.block_on(async {
        let a = home.client().await;
        let (atx, mut arx) = mpsc::channel(1024);
        let mut first = Box::pin(a.execute(home.prompt("count"), atx));
        let one = tokio::select! {
            Some(line) = arx.recv() => line.session_id().to_string(),
            _ = &mut first => panic!("ended early"),
        };
        // The same client starts a second session while following the first.
        let b = home.client().await;
        let (ftx, mut frx) = mpsc::channel(1024);
        let (stx, mut srx) = mpsc::channel(1024);
        let (r1, r2) = tokio::join!(b.follow(&one, None, ftx), b.execute(home.prompt("count again"), stx));
        let (r1, r2) = (r1.unwrap().unwrap(), r2.unwrap().unwrap());
        assert_ne!(r1.session_id, r2.session_id);
        let drain = |rx: &mut mpsc::Receiver<StreamLine>| {
            let mut v = Vec::new();
            while let Ok(l) = rx.try_recv() {
                v.push(l);
            }
            v
        };
        let (followed, ran) = (drain(&mut frx), drain(&mut srx));
        assert!(followed.iter().all(|l| l.session_id() == r1.session_id), "only the followed session's lines");
        assert!(ran.iter().all(|l| l.session_id() == r2.session_id), "only its own turn's lines");
        first.await.unwrap();
    });
    drop(rt);
    daemon.join().unwrap().unwrap();
}

/// `/connect` in the TUI: the daemon reads every host's instances again.
#[test]
fn r_proto_1_reload_reads_every_hosts_instances_again() {
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::text_stream("hi")));
    let home = Home::new("reload", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    rt.block_on(async {
        let c = home.client().await;
        let (tx, _rx) = mpsc::channel(1024);
        c.execute(home.prompt("hi"), tx).await.unwrap().unwrap();
        let before = home.made.load(std::sync::atomic::Ordering::SeqCst);
        c.reload(Some("anthropic"), None).await.unwrap();
        assert!(home.made.load(std::sync::atomic::Ordering::SeqCst) > before, "each host's configuration made again");
    });
    drop(rt);
    daemon.join().unwrap().unwrap();
}

/// R-PROTO-1: a turn's stream nobody drains never holds up the client's
/// other answers — an interrupt sent on the same connection is answered,
/// and stops the turn.
#[test]
fn r_proto_1_an_interrupt_is_answered_while_the_turns_stream_is_full() {
    let m = mock::serve(|_, _| mock::Reply::paced(mock::text_stream(&"word ".repeat(2000)), Duration::from_millis(3)));
    let home = Home::new("full", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    rt.block_on(async {
        let c = home.client().await;
        // Two lines of room, and nobody reading them.
        let (tx, mut rx) = mpsc::channel(2);
        let mut exec = Box::pin(c.execute(home.prompt("talk"), tx));
        let session = tokio::select! {
            Some(l) = rx.recv() => l.session_id().to_string(),
            _ = &mut exec => panic!("ended early"),
        };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = &mut exec => panic!("ended early"),
        }
        let (itx, _irx) = mpsc::channel(1);
        tokio::time::timeout(Duration::from_secs(3), c.execute(Command::Interrupt { session_id: session }, itx)).await.expect("answered, not stuck behind the stream").unwrap();
        // Drained, the turn ends interrupted, its lines all there.
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let r = exec.await.unwrap().unwrap();
        assert_eq!(r.status, TurnStatus::Interrupted);
        drain.abort();
    });
    drop(rt);
    daemon.join().unwrap().unwrap();
}

/// R-HOST-1: a client whose daemon went away reaches the next one on its
/// next command — started when none runs — instead of failing until krowk
/// is restarted.
#[test]
fn r_host_1_a_remote_client_reconnects_when_its_daemon_goes() {
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::text_stream("hi")));
    let home = Arc::new(Home::new("reconnect", &m.url));
    let first = home.serve(Some(Duration::from_millis(300)));
    let started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rt = rt();
    rt.block_on(async {
        let (h, st) = (home.clone(), started.clone());
        let spawn: Box<daemon::Spawn<'static>> = Box::new(move || {
            st.lock().unwrap().push(h.serve(Some(Duration::from_millis(300))));
            Ok(None)
        });
        let env = home.env();
        let r = daemon::remote::Remote::connect(Box::new(env), home.repo(), "test".into(), true, spawn).await.unwrap();
        let (tx, _rx) = mpsc::channel(1024);
        r.execute(home.prompt("hi"), tx).await.unwrap().unwrap();
        // Stopped under it, as `krowk host stop --force` does.
        let other = home.client().await;
        other.stop(true).await.unwrap();
        drop(other);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (tx, _rx) = mpsc::channel(1024);
        let again = r.execute(home.prompt("hi again"), tx).await.unwrap().unwrap();
        assert_eq!(again.status, TurnStatus::Completed);
        assert!(r.take_note().is_some_and(|n| n.contains("reconnected")));
    });
    drop(rt);
    first.join().unwrap().unwrap();
    assert_eq!(started.lock().unwrap().len(), 1, "the next daemon was started for it");
    for d in started.lock().unwrap().drain(..) {
        d.join().unwrap().unwrap();
    }
}

/// R-PROTO-1: a prompt that starts a session gets its own lines, bound by
/// its command — never those of a session the client still follows that
/// happen to arrive first — and a session left sends nothing more.
#[test]
fn r_proto_1_a_new_sessions_stream_is_bound_by_its_command_and_leave_stops_the_rest() {
    let m = slow();
    let home = Home::new("bind", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    rt.block_on(async {
        let a = home.client().await;
        // A ran a turn in S, so it follows S.
        let (tx, _rx) = mpsc::channel(1024);
        let s = a.execute(home.prompt("count"), tx).await.unwrap().unwrap().session_id;
        // B runs another turn in S while A starts a new session.
        let b = home.client().await;
        let resume = |text: &str| match home.prompt(text) {
            Command::Prompt { model, permission_mode, .. } => Command::Prompt { session_id: Some(s.clone()), text: text.into(), model, permission_mode, toolset: None, effort: None, budget: None },
            _ => unreachable!(),
        };
        let (btx, _brx) = mpsc::channel(1024);
        let mut other = Box::pin(b.execute(resume("again"), btx));
        let (atx, mut arx) = mpsc::channel(1024);
        let mut watch = a.watch();
        let mut mine = Box::pin(a.execute(home.prompt("count too"), atx));
        let (r_other, r_mine) = tokio::join!(&mut other, &mut mine);
        let (r_other, r_mine) = (r_other.unwrap().unwrap(), r_mine.unwrap().unwrap());
        assert_eq!(r_other.session_id, s);
        assert_ne!(r_mine.session_id, s);
        let mut lines = Vec::new();
        while let Ok(l) = arx.try_recv() {
            lines.push(l);
        }
        assert!(!lines.is_empty() && lines.iter().all(|l| l.session_id() == r_mine.session_id), "only its own session's lines");
        // S's frames came to A's watch instead; once A leaves S, none do.
        let mut saw_s = false;
        while let Ok(l) = watch.try_recv() {
            saw_s |= l.session_id() == s;
        }
        assert!(saw_s, "S's frames went where they belong");
        a.leave(&s);
        let (btx, _brx) = mpsc::channel(1024);
        b.execute(resume("once more"), btx).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(watch.try_recv().is_err(), "a session left sends nothing");
    });
    drop(rt);
    daemon.join().unwrap().unwrap();
}

/// R-HOST-1: reattaching after the last event a replay drew gets every
/// later event once — nothing appended between the replay and the attach
/// is lost, and nothing drawn comes again.
#[test]
fn r_host_1_following_after_the_replays_last_event_misses_and_repeats_nothing() {
    let m = slow();
    let home = Home::new("after", &m.url);
    let daemon = home.serve(Some(Duration::from_millis(300)));
    let rt = rt();
    rt.block_on(async {
        let a = home.client().await;
        let (atx, mut arx) = mpsc::channel(1024);
        let mut exec = Box::pin(a.execute(home.prompt("count"), atx));
        let mut seen = Vec::new();
        loop {
            tokio::select! {
                Some(l) = arx.recv() => {
                    seen.push(l);
                    if typed(&seen).split_whitespace().count() >= 2 { break }
                }
                _ = &mut exec => panic!("ended early"),
            }
        }
        let session = seen[0].session_id().to_string();
        let path = log::sessions_dir(&home.env()).unwrap().join(&session).join(log::EVENTS_FILE);
        // The replay, then time passes before the attach.
        let drawn = log::read_events(&path).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let b = home.client().await;
        let (btx, mut brx) = mpsc::channel(1024);
        b.follow(&session, drawn.last().map(|e| e.id.as_str()), btx).await.unwrap().unwrap();
        exec.await.unwrap();
        let mut got: Vec<String> = drawn.iter().map(|e| e.id.clone()).collect();
        while let Ok(l) = brx.try_recv() {
            if let StreamLine::Log(ev) = l {
                got.push(ev.id);
            }
        }
        let whole: Vec<String> = log::read_events(&path).unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(got, whole, "the log, each event once, in order");
    });
    drop(rt);
    daemon.join().unwrap().unwrap();
}
