//! The host daemon's WebSocket transport and the no-lag rules, against a
//! stand-in Anthropic API that types at a known rate (R-PROTO-1, R-LAG-1,
//! 2, 3, 4, 5, 9, 10). A test binary of its own, its tests one at a time:
//! they measure time, and a daemon under another test's load would measure
//! that instead. The client is tungstenite's, not the daemon's code, so a
//! framing mistake cannot pass on both ends.

#![cfg(unix)]

#[path = "common/mock.rs"]
mod mock;
#[path = "common/scratch.rs"]
mod scratch;

use futures_util::{SinkExt, StreamExt};
use krowk_harness::daemon::outbox::Caps;
use krowk_harness::daemon::ws::{self, Envelope};
use krowk_harness::daemon::{self, client::Client, server};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::protocol::{ClientFrame, Command, Delta, LiveEvent, PermissionMode, ServerFrame, StreamLine, PROTOCOL_VERSION};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Home {
    root: PathBuf,
    url: String,
    /// The daemon's thread's worst lateness, in µs, since last zeroed.
    late: Arc<AtomicU64>,
}

impl Home {
    fn new(name: &str, url: &str) -> Home {
        let root = scratch::root(&format!("ws-{name}"));
        for d in ["home", "run", "repo/.git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Home { root, url: url.into(), late: Arc::default() }
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

    /// Starts the daemon, with its WebSocket on a port of its choosing, on
    /// a thread of its own.
    fn serve(&self, heartbeat: Duration, caps: Caps) -> std::thread::JoinHandle<Result<(), String>> {
        let (env, socket) = (self.env(), self.socket());
        let credentials = self.root.join("home/.krowk/credentials.json");
        let late = self.late.clone();
        let t = std::thread::spawn(move || {
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
            let opts = server::Options { socket, idle: Some(Duration::from_millis(300)), krowk_version: "test".into(), websocket: Some("127.0.0.1:0".parse().unwrap()), heartbeat, caps, lateness: Some(late), ..Default::default() };
            server::run(opts, factory)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(self.socket()).is_err() {
            assert!(Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(5));
        }
        t
    }

    async fn client(&self) -> Client {
        match Client::connect(&self.socket(), &self.repo(), "test", false).await {
            Ok(c) => c,
            Err(_) => panic!("no daemon answered"),
        }
    }

    /// The WebSocket's address, from `status`, and its token, from beside
    /// the socket.
    async fn websocket(&self) -> (String, String) {
        let addr = self.client().await.status().await.unwrap().websocket.expect("the daemon listens on a WebSocket");
        let token = std::fs::read_to_string(self.socket().parent().unwrap().join(ws::TOKEN)).unwrap();
        (addr, token)
    }

    /// The worst the daemon's thread has been late since the last call, and
    /// zeroed for the next.
    fn lateness(&self) -> Duration {
        Duration::from_micros(self.late.swap(0, Ordering::Relaxed))
    }

    fn prompt(&self, text: &str, mode: PermissionMode) -> Command {
        let model = Registry::resolve(&InstancesConfig::default(), &self.env()).parse_model("claude-sonnet-4-6").unwrap();
        Command::Prompt { session_id: None, text: text.into(), model: Some(model), permission_mode: mode, toolset: None, effort: None, budget: None }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// `n` distinct words, so a word twice or missing shows.
fn words(n: usize) -> String {
    (1..=n).map(|i| format!("w{i} ")).collect::<String>().trim_end().to_string()
}

fn prompt_of(body: &serde_json::Value) -> String {
    body["messages"][0]["content"].to_string()
}

type Wire = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A WebSocket client of the daemon, the relay's side of the contract.
struct Ws {
    wire: Wire,
    batches: u64,
    /// Acknowledges every batch as it arrives; off, the daemon's window
    /// fills.
    acks: bool,
}

enum Got {
    Batch(Envelope, Vec<ServerFrame>),
    Ping,
    Pong,
}

impl Ws {
    async fn connect(addr: &str, token: Option<&str>, cwd: &Path) -> (Ws, ServerFrame) {
        // No Nagle: an ack or a ping held back for the last one's ACK would
        // time the client's TCP, not the daemon.
        let (wire, _) = tokio_tungstenite::connect_async_with_config(format!("ws://{addr}"), None, true).await.unwrap();
        let mut ws = Ws { wire, batches: 0, acks: true };
        ws.send(&ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: cwd.display().to_string(), krowk_version: "test".into(), answers_approvals: false, token: token.map(String::from) }).await;
        loop {
            if let Got::Batch(_, mut frames) = ws.next().await.expect("the daemon answers the hello") {
                return (ws, frames.remove(0));
            }
        }
    }

    async fn send(&mut self, f: &ClientFrame) {
        let e = Envelope { kind: ws::KIND_FRAME, flags: 0, enc: ws::ENC_NONE, session: [0; 16], seq: 0, payload: serde_json::to_vec(f).unwrap() };
        self.wire.send(Message::Binary(e.encode().into())).await.unwrap();
    }

    async fn ack(&mut self) {
        let e = Envelope { kind: ws::KIND_ACK, flags: 0, enc: ws::ENC_NONE, session: [0; 16], seq: self.batches, payload: Vec::new() };
        self.wire.send(Message::Binary(e.encode().into())).await.unwrap();
    }

    /// The next message; none once closed.
    async fn next(&mut self) -> Option<Got> {
        loop {
            match self.wire.next().await? {
                Ok(Message::Binary(b)) => {
                    let e = Envelope::decode(&b).unwrap();
                    assert_eq!(e.kind, ws::KIND_BATCH);
                    let plain = e.plain(64 << 20).unwrap();
                    let frames = plain.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(|l| serde_json::from_slice::<ServerFrame>(l).unwrap()).collect();
                    self.batches = self.batches.wrapping_add(1);
                    if self.acks {
                        self.ack().await;
                    }
                    return Some(Got::Batch(e, frames));
                }
                Ok(Message::Ping(_)) => return Some(Got::Ping),
                Ok(Message::Pong(_)) => return Some(Got::Pong),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }
}

/// The assistant's text a logged item carries whole.
fn completed_text(f: &ServerFrame) -> Option<&str> {
    use krowk_harness::protocol::{Item, LogBody, LogEvent};
    match f {
        ServerFrame::Line { line: StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::AssistantText { text }, .. }, .. }), .. } => Some(text),
        _ => None,
    }
}

fn delta_text(f: &ServerFrame) -> Option<(&str, &str)> {
    match f {
        ServerFrame::Line { line: StreamLine::Live(LiveEvent::ItemDelta { session_id, delta: Delta::Text { text }, .. }), .. } => Some((session_id, text)),
        _ => None,
    }
}

#[test]
fn r_proto_1_a_websocket_client_runs_a_turn_and_one_without_the_token_is_refused() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let answer = words(40);
    let m = { let a = answer.clone(); mock::serve(move |_, _| mock::Reply::paced(mock::text_stream(&a), Duration::from_millis(2))) };
    let home = Home::new("turn", &m.url);
    let daemon = home.serve(ws::HEARTBEAT, Caps::default());
    rt().block_on(async {
        let (addr, token) = home.websocket().await;
        let (_, refused) = Ws::connect(&addr, None, &home.repo()).await;
        assert!(matches!(&refused, ServerFrame::Refused { code, .. } if code == "unauthorized"), "{refused:?}");
        let (_, refused) = Ws::connect(&addr, Some("not-the-token"), &home.repo()).await;
        assert!(matches!(&refused, ServerFrame::Refused { code, .. } if code == "unauthorized"), "{refused:?}");

        let (mut c, welcome) = Ws::connect(&addr, Some(&token), &home.repo()).await;
        assert!(matches!(welcome, ServerFrame::Welcome { protocol_version: PROTOCOL_VERSION, .. }));
        c.send(&ClientFrame::Execute { id: 1, command: home.prompt("count", PermissionMode::Default) }).await;
        let (mut typed, mut seqs, mut compressed) = (String::new(), Vec::new(), 0);
        let mut result_seen = false;
        'turn: while let Some(got) = c.next().await {
            let Got::Batch(e, frames) = got else { continue };
            compressed += usize::from(e.flags & ws::FLAG_ZSTD != 0);
            for f in &frames {
                if let Some((_, t)) = delta_text(f) {
                    typed.push_str(t);
                }
                match f {
                    ServerFrame::Line { seq: Some(s), cmd: Some(1), line, .. } => {
                        seqs.push(*s);
                        result_seen |= matches!(line, StreamLine::Live(LiveEvent::Result(_)));
                    }
                    ServerFrame::Done { id: 1, result, error } => {
                        assert!(error.is_none(), "{error:?}");
                        assert!(result_seen, "the turn's lines, its result among them, come before its done");
                        assert_eq!(result.as_ref().unwrap().result, answer);
                        break 'turn;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(typed, answer);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "numbered in order: {seqs:?}");
        assert!(compressed > 0, "batches of several frames go compressed");
    });
    daemon.join().unwrap().unwrap();
}

/// R-LAG-1: while the model types, nothing under krowk's home is written —
/// neither the log nor krowk.db nor anything else: the deltas go from the
/// provider's stream to the socket through memory alone, and only the
/// finished item is logged. The stream is held open after its last word
/// until every word is read, so the finished item's write, one pace behind
/// it, cannot land before a slow client has looked.
#[test]
fn r_lag_1_no_write_sits_between_the_provider_stream_and_the_socket() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let answer = words(60);
    let gate = mock::Gate::default();
    let m = {
        let (a, g) = (answer.clone(), gate.clone());
        mock::serve(move |_, _| mock::Reply { hold: Some(("content_block_stop", g.clone())), ..mock::Reply::paced(mock::text_stream(&a), Duration::from_millis(10)) })
    };
    let home = Home::new("nowrite", &m.url);
    let daemon = home.serve(ws::HEARTBEAT, Caps::default());
    fn written(dir: &Path, into: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let (p, meta) = (e.path(), e.metadata().unwrap());
            if meta.is_dir() {
                written(&p, into);
            } else {
                into.push((p, meta.len(), meta.modified().unwrap()));
            }
        }
        into.sort();
    }
    let snapshot = || {
        let mut v = Vec::new();
        written(&home.root.join("home"), &mut v);
        v
    };
    rt().block_on(async {
        let (addr, token) = home.websocket().await;
        let (mut c, _) = Ws::connect(&addr, Some(&token), &home.repo()).await;
        c.send(&ClientFrame::Execute { id: 1, command: home.prompt("count", PermissionMode::Default) }).await;
        let (mut typed, mut first, mut deltas) = (String::new(), None, 0);
        'turn: while let Some(got) = c.next().await {
            let Got::Batch(_, frames) = got else { continue };
            for f in &frames {
                if let Some((_, t)) = delta_text(f) {
                    typed.push_str(t);
                    deltas += 1;
                    let now = snapshot();
                    match &first {
                        None => first = Some(now),
                        Some(then) => assert_eq!(then, &now, "something was written while the model typed ({} words in)", typed.split_whitespace().count()),
                    }
                    if typed.len() == answer.len() {
                        break 'turn;
                    }
                }
            }
        }
        assert_eq!(typed, answer);
        assert!(deltas > 10, "the answer streamed in pieces: {deltas}");
        let first = first.unwrap();
        let events = first.iter().filter(|(p, ..)| p.ends_with(log::EVENTS_FILE)).count();
        assert_eq!(events, 1, "the log was there all along, and did not move");
        // The stream ends, and the finished item is logged: the one write,
        // and one the snapshots would have seen.
        gate.open();
        while let Some(got) = c.next().await {
            let Got::Batch(_, frames) = got else { continue };
            if frames.iter().any(|f| matches!(f, ServerFrame::Done { id: 1, .. })) {
                break;
            }
        }
        let grown = |v: &[(PathBuf, u64, std::time::SystemTime)]| v.iter().find(|(p, ..)| p.ends_with(log::EVENTS_FILE)).map(|(_, len, _)| *len);
        assert!(grown(&snapshot()) > grown(&first), "the finished item is logged once the stream ends");
    });
    daemon.join().unwrap().unwrap();
}

/// The acceptance load (R-LAG-2: one queue per session; R-LAG-3: batches by
/// time, 16 ms, never by content; R-LAG-9: nothing blocks the thread they
/// share): three sessions streaming 500 tokens a second and a fourth
/// flooding, over one WebSocket; no session's stream stalls more than 50 ms
/// because of another, and the daemon's thread is never late more than
/// 30 ms. The progress-slot half of R-LAG-2 — a subagent flooding `cost`
/// frames costs one frame — is `outbox`'s own test.
///
/// Here the flood is 5,000 tokens a second, which the unoptimized build
/// `make check` runs keeps up with; the release build takes the flood
/// unpaced (`…_release_…`, run by `make bench`), since an unoptimized
/// build spends tens of milliseconds parsing a provider's burst of
/// hundreds of kilobytes on the thread.
#[test]
fn r_lag_2_r_lag_3_three_sessions_at_500_tokens_a_second_and_a_flood_stall_none() {
    load(8_000, Some(Duration::from_micros(200)));
}

/// The same with the flood unpaced — every token the provider has, as fast
/// as it sends them. Release only: `make bench` runs it.
#[test]
#[cfg_attr(debug_assertions, ignore = "release only, as make bench runs it")]
fn r_lag_2_r_lag_3_release_three_sessions_and_an_unpaced_flood_stall_none() {
    load(30_000, None);
}

fn load(flood_words: usize, flood_pace: Option<Duration>) {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let paced = words(1000);
    let flood = words(flood_words);
    let m = {
        let (p, f) = (paced.clone(), flood.clone());
        mock::serve(move |body, _| {
            if !prompt_of(body).contains("flood") {
                mock::Reply::paced(mock::text_stream(&p), Duration::from_millis(2))
            } else if let Some(pace) = flood_pace {
                mock::Reply::paced(mock::text_stream(&f), pace)
            } else {
                mock::Reply::sse(&mock::text_stream(&f))
            }
        })
    };
    let home = Home::new("load", &m.url);
    let daemon = home.serve(ws::HEARTBEAT, Caps::default());
    let (arrivals, texts, late) = rt().block_on(async {
        let (addr, token) = home.websocket().await;
        let (mut c, _) = Ws::connect(&addr, Some(&token), &home.repo()).await;
        for (id, what) in [(1, "paced one"), (2, "paced two"), (3, "paced three"), (4, "flood")] {
            c.send(&ClientFrame::Execute { id, command: home.prompt(what, PermissionMode::Default) }).await;
        }
        // For each session: when each batch with its typing arrived, and
        // what it typed.
        let mut arrivals: std::collections::HashMap<String, Vec<Instant>> = Default::default();
        let mut texts: std::collections::HashMap<String, String> = Default::default();
        let mut done = 0;
        let mut probed = false;
        let mut late = Duration::ZERO;
        while done < 4 {
            let Some(got) = tokio::time::timeout(Duration::from_secs(30), c.next()).await.expect("the load finished") else { panic!("closed") };
            let Got::Batch(_, frames) = got else { continue };
            let at = Instant::now();
            let mut sessions = std::collections::HashSet::new();
            for f in &frames {
                if let Some((s, t)) = delta_text(f) {
                    texts.entry(s.to_string()).or_default().push_str(t);
                    sessions.insert(s.to_string());
                }
                done += usize::from(matches!(f, ServerFrame::Done { .. }));
            }
            for s in sessions {
                arrivals.entry(s).or_default().push(at);
            }
            // Once every session is typing, the daemon's thread is watched
            // until the last ends: nothing may block it long enough to
            // stall the others (R-LAG-9). The first host's setup, before
            // that, is the known exception (harness.md → The host daemon).
            if !probed && arrivals.len() == 4 {
                probed = true;
                home.lateness();
            }
            if probed && done > 0 && late.is_zero() {
                late = home.lateness().max(Duration::from_nanos(1));
            }
        }
        (arrivals, texts, late)
    });
    eprintln!("the daemon's thread was late by at most {late:?} while all four streamed");
    assert!(late < Duration::from_millis(30), "something blocked the daemon's thread {late:?} while four sessions streamed");
    let paced_sessions: Vec<&String> = texts.iter().filter(|(_, t)| t.len() == paced.len()).map(|(s, _)| s).collect();
    assert_eq!(paced_sessions.len(), 3, "each paced session typed its answer whole");
    assert!(texts.values().any(|t| t == &flood), "and the flood its own");
    for s in paced_sessions {
        let at = &arrivals[s];
        let worst = at.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        let span = *at.last().unwrap() - at[0];
        eprintln!("session {s}: {} batches over {span:?}, longest gap {worst:?}", at.len());
        assert!(worst < Duration::from_millis(50), "session {s} stalled {worst:?} while the others streamed");
    }
    daemon.join().unwrap().unwrap();
}

/// R-LAG-4 / R-LAG-10 over the WebSocket: a client that stops reading and
/// acknowledging fills its window, then its outbox, and falls behind to its
/// cursor — the daemon holds no more than the cap for it however long it
/// sleeps — and once it reads again it is caught up from that cursor:
/// every word, once, in order.
#[test]
fn r_lag_4_r_lag_10_a_slow_websocket_client_is_caught_up_from_its_cursor_in_bounded_memory() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let answer = words(3000);
    let m = { let a = answer.clone(); mock::serve(move |_, _| mock::Reply::paced(mock::text_stream(&a), Duration::from_micros(700))) };
    let home = Home::new("slow", &m.url);
    let cap = 32 * 1024;
    let daemon = home.serve(ws::HEARTBEAT, Caps { session_bytes: cap, control_bytes: 1 << 20 });
    rt().block_on(async {
        let (addr, token) = home.websocket().await;
        let watcher = home.client().await;
        let (mut c, _) = Ws::connect(&addr, Some(&token), &home.repo()).await;
        c.send(&ClientFrame::Execute { id: 1, command: home.prompt("count", PermissionMode::Default) }).await;
        // Asleep: nothing read, nothing acknowledged, while the turn runs.
        let mut most = 0u64;
        let until = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < until {
            most = most.max(watcher.status().await.unwrap().queued_bytes);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(most <= (cap + 16 * 1024) as u64, "the daemon held {most} bytes for a client that does not read");
        // Awake: read everything to the turn's end.
        let (mut typed, mut completed, mut seqs) = (String::new(), Vec::new(), Vec::new());
        'turn: while let Some(got) = tokio::time::timeout(Duration::from_secs(20), c.next()).await.expect("the turn ended") {
            let Got::Batch(_, frames) = got else { continue };
            for f in &frames {
                if let Some((_, t)) = delta_text(f) {
                    typed.push_str(t);
                }
                completed.extend(completed_text(f).map(String::from));
                if let ServerFrame::Line { seq: Some(s), .. } = f {
                    seqs.push(*s);
                }
                if let ServerFrame::Done { id: 1, result, .. } = f {
                    assert_eq!(result.as_ref().unwrap().result, answer);
                    break 'turn;
                }
            }
        }
        // The typing it was caught up on runs on from where it stood — a
        // turn that ended meanwhile is in the log, whole, and its typing
        // ephemeral — and the item comes once, complete.
        assert!(answer.starts_with(&typed), "nothing typed twice, nothing skipped: {} of {} bytes", typed.len(), answer.len());
        assert_eq!(completed, vec![answer.clone()], "the finished item once");
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "in order, nothing twice");
        assert!(watcher.status().await.unwrap().caught_up > 0, "it fell behind and was caught up from its cursor");
    });
    daemon.join().unwrap().unwrap();
}

/// The same bound on the unix socket, which the TUI is on: a TUI suspended
/// with ^Z while it follows a long turn costs the daemon the cap, not the
/// turn (R-LAG-4, R-LAG-10).
#[test]
fn r_lag_4_r_lag_10_a_suspended_unix_client_is_caught_up_from_its_cursor_in_bounded_memory() {
    use std::io::{BufRead, Write};
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let answer = words(4000);
    let m = { let a = answer.clone(); mock::serve(move |_, _| mock::Reply::paced(mock::text_stream(&a), Duration::from_micros(500))) };
    let home = Home::new("suspended", &m.url);
    let cap = 32 * 1024;
    let daemon = home.serve(ws::HEARTBEAT, Caps { session_bytes: cap, control_bytes: 1 << 20 });
    let mut s = std::os::unix::net::UnixStream::connect(home.socket()).unwrap();
    let hello = ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: home.repo().display().to_string(), krowk_version: "test".into(), answers_approvals: false, token: None };
    let exec = ClientFrame::Execute { id: 1, command: home.prompt("count", PermissionMode::Default) };
    writeln!(s, "{}\n{}", serde_json::to_string(&hello).unwrap(), serde_json::to_string(&exec).unwrap()).unwrap();
    // Suspended: not a byte read, while a second client watches the
    // daemon's queues.
    let most = rt().block_on(async {
        let watcher = home.client().await;
        let mut most = 0u64;
        let until = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < until {
            most = most.max(watcher.status().await.unwrap().queued_bytes);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        most
    });
    assert!(most <= (cap + 16 * 1024) as u64, "the daemon held {most} bytes for a client that does not read");
    // Resumed.
    let (mut typed, mut completed, mut seqs) = (String::new(), Vec::new(), Vec::new());
    for line in std::io::BufReader::new(&s).lines() {
        let f: ServerFrame = serde_json::from_str(&line.unwrap()).unwrap();
        if let Some((_, t)) = delta_text(&f) {
            typed.push_str(t);
        }
        completed.extend(completed_text(&f).map(String::from));
        if let ServerFrame::Line { seq: Some(n), .. } = f {
            seqs.push(n);
        }
        if let ServerFrame::Done { id: 1, .. } = f {
            break;
        }
    }
    assert!(answer.starts_with(&typed), "nothing typed twice, nothing skipped: {} of {} bytes", typed.len(), answer.len());
    assert_eq!(completed, vec![answer.clone()], "the finished item once");
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "in order, nothing twice");
    let caught_up = rt().block_on(async { home.client().await.status().await.unwrap().caught_up });
    assert!(caught_up > 0, "it fell behind and was caught up from its cursor");
    drop(s);
    daemon.join().unwrap().unwrap();
}

/// R-LAG-9: a tool that blocks for five seconds holds up neither the
/// daemon's pings nor its answers to the client's: a busy host never looks
/// like a dead one.
#[test]
fn r_lag_9_a_tool_that_blocks_for_five_seconds_does_not_delay_heartbeats() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let m = mock::serve(|body, _| {
        let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
        if last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result")) {
            mock::Reply::sse(&mock::text_stream("Slept."))
        } else {
            mock::Reply::sse(&mock::tool_use("toolu_01Sleep", "bash", &serde_json::json!({"command": "sleep 5"})))
        }
    });
    let home = Home::new("heartbeat", &m.url);
    let beat = Duration::from_millis(100);
    let daemon = home.serve(beat, Caps::default());
    rt().block_on(async {
        let (addr, token) = home.websocket().await;
        let (mut c, _) = Ws::connect(&addr, Some(&token), &home.repo()).await;
        let started = Instant::now();
        home.lateness();
        c.send(&ClientFrame::Execute { id: 1, command: home.prompt("sleep", PermissionMode::BypassPermissions) }).await;
        let (mut pings, mut rtts, mut asked) = (Vec::new(), Vec::new(), None::<Instant>);
        // The thread's lateness a beat at a time, so a failure says when in
        // the turn the thread was held: at its start (the host's setup, the
        // tool's spawn) or in the middle of the tool's sleep, where the
        // daemon has nothing to do and only the machine can hold it.
        let mut when = Vec::new();
        let mut ask = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = ask.tick() => {
                    when.push((started.elapsed(), home.lateness()));
                    if asked.is_none() {
                        c.wire.send(Message::Ping(Vec::new().into())).await.unwrap();
                        asked = Some(Instant::now());
                    }
                }
                got = c.next() => match got.expect("the connection stayed up") {
                    Got::Ping => pings.push(Instant::now()),
                    Got::Pong => rtts.push(asked.take().expect("a pong answers a ping").elapsed()),
                    Got::Batch(_, frames) => {
                        if frames.iter().any(|f| matches!(f, ServerFrame::Done { id: 1, .. })) {
                            break;
                        }
                    }
                },
            }
        }
        let took = started.elapsed();
        when.push((took, home.lateness()));
        let late = when.iter().map(|(_, l)| *l).max().unwrap();
        let held: Vec<String> = when.iter().filter(|(_, l)| *l >= Duration::from_millis(5)).map(|(at, l)| format!("{l:?} by {at:?}")).collect();
        assert!(took >= Duration::from_secs(5), "the tool ran its five seconds: {took:?}");
        assert!(late < Duration::from_millis(30), "the daemon's thread was blocked {late:?} during the turn — held {held:?}");
        let gap = pings.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        let rtt = rtts.iter().max().unwrap();
        eprintln!("{} pings, longest gap {gap:?}; {} pongs, slowest {rtt:?}", pings.len(), rtts.len());
        assert!(pings.len() as u128 >= took.as_millis() / beat.as_millis() - 2, "a ping every beat: {}", pings.len());
        assert!(gap < beat + Duration::from_millis(50), "pings went on while the tool blocked: {gap:?}");
        assert!(*rtt < Duration::from_millis(50), "and pings were answered at once: {rtt:?}");
    });
    daemon.join().unwrap().unwrap();
}

/// R-LAG-9's other half: a peer that has gone — nothing heard, not even a
/// pong, for three beats — is let go by the daemon, however long TCP would
/// take to notice.
#[test]
fn r_lag_9_a_peer_that_answers_no_heartbeat_is_let_go() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let home = Home::new("dead", "http://127.0.0.1:9");
    let daemon = home.serve(Duration::from_millis(100), Caps::default());
    rt().block_on(async {
        let (addr, token) = home.websocket().await;
        let watcher = home.client().await;
        // Said hello, then never read again: its pongs never go out.
        let (c, _) = Ws::connect(&addr, Some(&token), &home.repo()).await;
        assert_eq!(watcher.status().await.unwrap().clients, 2);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(watcher.status().await.unwrap().clients, 1, "the silent peer was let go");
        drop(c);
    });
    daemon.join().unwrap().unwrap();
}

/// A client of the unix socket reading and writing raw frames, as a TUI's
/// link does, with the `seq`s and cursors the client library hides.
struct Raw {
    w: std::os::unix::net::UnixStream,
    r: std::io::BufReader<std::os::unix::net::UnixStream>,
    epoch: u64,
}

impl Raw {
    fn connect(home: &Home) -> Raw {
        use std::io::Write;
        let s = std::os::unix::net::UnixStream::connect(home.socket()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut raw = Raw { w: s.try_clone().unwrap(), r: std::io::BufReader::new(s), epoch: 0 };
        let hello = ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: home.repo().display().to_string(), krowk_version: "test".into(), answers_approvals: false, token: None };
        writeln!(raw.w, "{}", serde_json::to_string(&hello).unwrap()).unwrap();
        match raw.next() {
            ServerFrame::Welcome { epoch, .. } => raw.epoch = epoch,
            f => panic!("{f:?}"),
        }
        raw
    }

    fn send(&mut self, f: &ClientFrame) {
        use std::io::Write;
        writeln!(self.w, "{}", serde_json::to_string(f).unwrap()).unwrap();
    }

    fn next(&mut self) -> ServerFrame {
        use std::io::BufRead;
        let mut line = String::new();
        self.r.read_line(&mut line).expect("a frame in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }

    /// Frames up to and including the first `until` matches.
    fn until(&mut self, until: impl Fn(&ServerFrame) -> bool) -> Vec<ServerFrame> {
        let mut out = Vec::new();
        loop {
            let f = self.next();
            let end = until(&f);
            out.push(f);
            if end {
                return out;
            }
        }
    }
}

fn prompt_in(home: &Home, session: Option<&str>, text: &str) -> Command {
    let Command::Prompt { model, permission_mode, .. } = home.prompt(text, PermissionMode::Default) else { unreachable!() };
    Command::Prompt { session_id: session.map(String::from), text: text.into(), model, permission_mode, toolset: None, effort: None, budget: None }
}

fn seqs_of(frames: &[ServerFrame]) -> Vec<u64> {
    frames.iter().filter_map(|f| if let ServerFrame::Line { seq: Some(s), .. } = f { Some(*s) } else { None }).collect()
}

fn typed_of(frames: &[ServerFrame]) -> String {
    frames.iter().filter_map(delta_text).map(|(_, t)| t).collect()
}

/// R-PROTO-1's cursor: a session's `seq`s never start again while the
/// daemon runs, even once nothing followed it and it was let go; and a
/// cursor from another daemon run (`epoch`) is caught up from the log, not
/// trusted to a `seq` this daemon never gave.
#[test]
fn r_proto_1_a_seq_is_never_reused_and_a_cursor_from_another_run_is_caught_up_from_the_log() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let answer = words(200);
    let m = { let a = answer.clone(); mock::serve(move |_, _| mock::Reply::paced(mock::text_stream(&a), Duration::from_millis(2))) };
    let home = Home::new("seq", &m.url);
    let daemon = home.serve(ws::HEARTBEAT, Caps::default());
    let mut a = Raw::connect(&home);
    a.send(&ClientFrame::Execute { id: 1, command: prompt_in(&home, None, "one") });
    let first = a.until(|f| matches!(f, ServerFrame::Done { id: 1, .. }));
    let session = first.iter().find_map(|f| if let ServerFrame::Line { session, .. } = f { Some(session.clone()) } else { None }).unwrap();
    let top = *seqs_of(&first).iter().max().unwrap();
    // Gone, and with it the session's hub: nothing follows or runs it.
    drop(a);
    std::thread::sleep(Duration::from_millis(100));
    let mut b = Raw::connect(&home);
    b.send(&ClientFrame::Execute { id: 1, command: prompt_in(&home, Some(&session), "two") });
    // Mid-turn, a client with a cursor from another daemon run attaches:
    // a `seq` past anything, which this daemon must not trust.
    let mut c = Raw::connect(&home);
    let early = b.until(|f| typed_of(std::slice::from_ref(f)).contains("w20 "));
    c.send(&ClientFrame::Attach { id: 7, session_id: session.clone(), after_event_id: None, after_seq: Some(1_000_000), epoch: Some(c.epoch - 1) });
    let rest = b.until(|f| matches!(f, ServerFrame::Done { id: 1, .. }));
    let second: Vec<ServerFrame> = early.into_iter().chain(rest).collect();
    assert!(seqs_of(&second).iter().all(|s| *s > top), "numbered on from {top}, never again from 1: {:?}", &seqs_of(&second)[..3]);
    let caught = c.until(|f| matches!(f, ServerFrame::Line { line: StreamLine::Live(LiveEvent::Result(_)), .. }));
    let typed = typed_of(&caught);
    assert_eq!(typed, answer, "the typing so far and the rest, from the log's cursor alone");
    drop((b, c));
    daemon.join().unwrap().unwrap();
}

/// R-LAG-4: a client caught up across a turn boundary gets the first turn's
/// `done` after that turn's lines and before the next turn's — the place
/// it was queued at, not after all the catching up.
#[test]
fn r_lag_4_a_done_held_while_behind_goes_back_at_its_place() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let answer = words(1500);
    let m = { let a = answer.clone(); mock::serve(move |_, _| mock::Reply::paced(mock::text_stream(&a), Duration::from_micros(300))) };
    let home = Home::new("held", &m.url);
    let daemon = home.serve(ws::HEARTBEAT, Caps { session_bytes: 16 * 1024, control_bytes: 1 << 20 });
    // X runs the first turn and stops reading; Y runs the next one in the
    // same session while X is still behind.
    let mut x = Raw::connect(&home);
    x.send(&ClientFrame::Execute { id: 1, command: prompt_in(&home, None, "one") });
    let head = x.until(|f| matches!(f, ServerFrame::Line { .. }));
    let ServerFrame::Line { session, .. } = &head[0] else { unreachable!() };
    let session = session.clone();
    let watcher = Raw::connect(&home);
    drop(watcher);
    let mut y = Raw::connect(&home);
    // The first turn ends while X sleeps: waited for, not slept on, since a
    // slow runner types slower than the mock's pace and a prompt sent into
    // the running turn would make one turn of two.
    rt().block_on(async {
        let w = home.client().await;
        let deadline = Instant::now() + Duration::from_secs(20);
        while w.status().await.unwrap().sessions.iter().any(|s| s.session_id == session && s.running) {
            assert!(Instant::now() < deadline, "the first turn ended");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    y.send(&ClientFrame::Execute { id: 9, command: prompt_in(&home, Some(&session), "two") });
    y.until(|f| matches!(f, ServerFrame::Done { id: 9, .. }));
    // X reads again: its first turn's end, its done, then the second turn.
    use krowk_harness::protocol::{LogBody, LogEvent};
    let log_kind = |f: &ServerFrame, started: bool| match f {
        ServerFrame::Line { line: StreamLine::Log(LogEvent { body: LogBody::TurnStarted { .. }, .. }), .. } => started,
        ServerFrame::Line { line: StreamLine::Log(LogEvent { body: LogBody::TurnCompleted { .. }, .. }), .. } => !started,
        _ => false,
    };
    let mut all = head;
    let mut ends = 0;
    while ends < 2 {
        let f = x.next();
        ends += usize::from(log_kind(&f, false));
        all.push(f);
    }
    let first_end = all.iter().position(|f| log_kind(f, false)).unwrap();
    let done = all.iter().position(|f| matches!(f, ServerFrame::Done { id: 1, .. })).expect("the first turn's done came");
    let second_start = all.iter().enumerate().filter(|(_, f)| log_kind(f, true)).map(|(i, _)| i).nth(1).expect("the second turn started");
    assert!(first_end < done && done < second_start, "done at {done}, between its turn's end ({first_end}) and the next turn's start ({second_start})");
    let caught_up = rt().block_on(async { home.client().await.status().await.unwrap().caught_up });
    assert!(caught_up > 0, "it did fall behind");
    drop((x, y));
    daemon.join().unwrap().unwrap();
}

/// R-LAG-10: catching up is paged. A client attaching to a session whose
/// log is many times the cap, and reading slowly, costs the daemon about a
/// page at a time, not the log — and gets the whole log once, in order,
/// then `attached`.
#[test]
fn r_lag_10_an_attach_to_a_long_log_is_paged_at_the_cap() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let m = mock::serve(|_, n| mock::Reply::sse(&mock::text_stream(&format!("answer {n}: {}", words(300)))));
    let home = Home::new("paged", &m.url);
    let cap = 4 * 1024;
    let daemon = home.serve(ws::HEARTBEAT, Caps { session_bytes: cap, control_bytes: 1 << 20 });
    // Twelve turns: a log several times the cap.
    let mut a = Raw::connect(&home);
    a.send(&ClientFrame::Execute { id: 0, command: prompt_in(&home, None, "turn") });
    let first = a.until(|f| matches!(f, ServerFrame::Done { id: 0, .. }));
    let ServerFrame::Line { session, .. } = &first[0] else { unreachable!() };
    let session = session.clone();
    for id in 1..12 {
        a.send(&ClientFrame::Execute { id, command: prompt_in(&home, Some(&session), "turn") });
        a.until(|f| matches!(f, ServerFrame::Done { id: d, .. } if *d == id));
    }
    let log = std::fs::read(log::sessions_dir(&home.env()).unwrap().join(&session).join(log::EVENTS_FILE)).unwrap();
    assert!(log.len() > 8 * cap, "a log of {} bytes", log.len());
    let events = log.iter().filter(|b| **b == b'\n').count();
    // A slow reader attaches.
    let mut c = Raw::connect(&home);
    c.send(&ClientFrame::Attach { id: 5, session_id: session.clone(), after_event_id: None, after_seq: None, epoch: None });
    let watch = std::thread::spawn({
        let socket = home.socket();
        let repo = home.repo();
        move || {
            rt().block_on(async {
                let w = match Client::connect(&socket, &repo, "test", false).await { Ok(c) => c, Err(_) => panic!("no daemon") };
                let mut most = 0;
                for _ in 0..60 {
                    most = most.max(w.status().await.unwrap().queued_bytes);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                most
            })
        }
    });
    let mut got = Vec::new();
    loop {
        std::thread::sleep(Duration::from_millis(2));
        let f = c.next();
        if matches!(f, ServerFrame::Attached { id: 5, .. }) {
            break;
        }
        got.push(f);
    }
    let most = watch.join().unwrap();
    let biggest = log.split(|b| *b == b'\n').map(|l| l.len()).max().unwrap();
    assert!(most as usize <= cap + biggest + 1024, "the daemon held {most} bytes for an attach of a {}-byte log", log.len());
    let ids: Vec<String> = got.iter().filter_map(|f| if let ServerFrame::Line { line: StreamLine::Log(e), .. } = f { Some(e.id.clone()) } else { None }).collect();
    assert_eq!(ids.len(), events, "every event once");
    let mut sorted = ids.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len());
    drop((a, c));
    daemon.join().unwrap().unwrap();
}

/// What a client without the token can learn and cost: nothing about the
/// daemon (not even its pid or version, whatever protocol it claims), no
/// decompression, and no connection at all from a browser page.
#[test]
fn r_proto_1_nothing_is_said_before_the_token_and_browsers_are_refused() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let home = Home::new("preauth", "http://127.0.0.1:9");
    let daemon = home.serve(ws::HEARTBEAT, Caps::default());
    rt().block_on(async {
        let (addr, token) = home.websocket().await;
        // Another protocol, no token: unauthorized, not a version to read.
        let (wire, _) = tokio_tungstenite::connect_async_with_config(format!("ws://{addr}"), None, true).await.unwrap();
        let mut ws = Ws { wire, batches: 0, acks: true };
        ws.send(&ClientFrame::Hello { protocol_version: 999, cwd: "/".into(), krowk_version: String::new(), answers_approvals: false, token: None }).await;
        let Some(Got::Batch(_, f)) = ws.next().await else { panic!("no answer") };
        assert!(matches!(&f[0], ServerFrame::Refused { code, message, .. } if code == "unauthorized" && !message.contains("pid")), "{:?}", f[0]);
        // A compressed hello is not decompressed, token or not.
        let (wire, _) = tokio_tungstenite::connect_async_with_config(format!("ws://{addr}"), None, true).await.unwrap();
        let mut ws = Ws { wire, batches: 0, acks: true };
        let hello = ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, cwd: "/".into(), krowk_version: String::new(), answers_approvals: false, token: Some(token.clone()) };
        let packed = ws::batch("", 0, serde_json::to_string(&hello).unwrap().repeat(20).as_bytes());
        assert_eq!(packed.flags & ws::FLAG_ZSTD, ws::FLAG_ZSTD);
        let e = Envelope { kind: ws::KIND_FRAME, ..packed };
        ws.wire.send(Message::Binary(e.encode().into())).await.unwrap();
        let Some(Got::Batch(_, f)) = ws.next().await else { panic!("no answer") };
        assert!(matches!(&f[0], ServerFrame::Refused { code, .. } if code == "unauthorized"), "{:?}", f[0]);
        // A browser's handshake carries an Origin: refused before a frame.
        let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!("ws://{addr}")).unwrap();
        req.headers_mut().insert("origin", "https://example.com".parse().unwrap());
        assert!(tokio_tungstenite::connect_async(req).await.is_err(), "a page may not connect");
        // An ack past anything sent neither panics nor stalls the writer.
        let (mut ok, _) = Ws::connect(&addr, Some(&token), &home.repo()).await;
        ok.batches = u64::MAX;
        ok.ack().await;
        ok.send(&ClientFrame::Status { id: 3 }).await;
        let Some(Got::Batch(_, f)) = ok.next().await else { panic!("the writer stalled") };
        assert!(matches!(&f[0], ServerFrame::Status { id: 3, .. }), "{:?}", f[0]);
    });
    daemon.join().unwrap().unwrap();
}
