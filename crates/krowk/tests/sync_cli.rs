//! `krowk sync host` and `krowk sync attach` as a person runs them, two
//! machines in one test: A hosts a session it made with `krowk -p`, B
//! attaches it and answers A's approvals and interrupts its turn by typing
//! slash commands on stdin (todo 22a). And `sync host` for a session A does
//! not have fails at once, saying so, rather than hanging (todo 22b). And
//! the direct path with no setup (ticket 42): the host goes direct with the
//! registry's published ticket keys, or says in one line why it does not.
//! And `krowk hosts` listing the machines that host, with no Tailscale tag
//! (ticket 43). And a subagent of A's turn stopped alone from B by its id,
//! and opened in a TUI attached on a third machine, which reads its
//! history there and stops it with `x` (R-SUB-12).
//! Against the stand-in registry, the reference relay and a mock model.

#![cfg(all(feature = "harness", unix))]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;
#[path = "common/pty.rs"]
mod pty;
#[path = "common/tmux.rs"]
mod tmux;

use krowk_client::e2e::{self, AccountKey};
use krowk_client::keystore::Keystore;
#[path = "common/device_list.rs"]
mod device_list;
use device_list::People;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TOKEN: &str = "krowk_sk_sync_cli_000000000000000000000";
const ANSWER: &str = "the-answer-marker-5c2e";
/// What the model answers a prompt typed into the TUI on B.
const TUI_ANSWER: &str = "tui-round-trip-answer-81aa";

/// Holds the slower subagent's answer until the test opens it.
static SLOWER: std::sync::LazyLock<mock::Gate> = std::sync::LazyLock::new(mock::Gate::default);

/// A model that answers with the marker, except when asked to make a
/// file, which it does with `bash` — an approval — and a long answer, paced
/// so that it can be interrupted part way.
fn model(body: &serde_json::Value, _n: usize) -> mock::Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or_default();
    let has_result = last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    let text = last.to_string();
    // The slower subagent: a search, then, once the test lets it
    // (`SLOWER`), a slow answer — an item begun while a TUI looks on.
    if body["system"].to_string().contains("You are a subagent") && messages.first().is_some_and(|m| m.to_string().contains("look slower")) {
        if !has_result {
            return mock::Reply::sse(&mock::tool_use("toolu_01Glob", "glob", &serde_json::json!({"pattern": "*"})));
        }
        let words: String = (0..400).map(|i| format!("slowword{i} ")).collect();
        return mock::Reply { hold: Some(("content_block_start", SLOWER.clone())), ..mock::Reply::paced(mock::text_stream(&words), Duration::from_millis(50)) };
    }
    // A subagent's own call: a slow answer, stopped part way.
    if body["system"].to_string().contains("You are a subagent") {
        let words: String = (0..400).map(|i| format!("childword{i} ")).collect();
        return mock::Reply::paced(mock::text_stream(&words), Duration::from_millis(20));
    }
    if !has_result && text.contains("start a slower subagent") {
        return mock::Reply::sse(&mock::tool_use("toolu_01Slower", "subagent", &serde_json::json!({"description": "a slower look", "prompt": "look slower"})));
    }
    if !has_result && text.contains("start a slow subagent") {
        return mock::Reply::sse(&mock::tool_use("toolu_01Kid", "subagent", &serde_json::json!({"description": "a slow look", "prompt": "look slowly"})));
    }
    if !has_result && text.contains("tui round trip") {
        return mock::Reply::paced(mock::text_stream(TUI_ANSWER), Duration::from_millis(5));
    }
    if !has_result && text.contains("a long answer") {
        let words: String = (0..400).map(|i| format!("word{i} ")).collect();
        return mock::Reply::paced(mock::text_stream(&words), Duration::from_millis(20));
    }
    for (ask, file) in [("make the allowed file", "allowed.txt"), ("make the denied file", "denied.txt")] {
        if !has_result && text.contains(ask) {
            return mock::Reply::sse(&mock::tool_use("toolu_01Touch", "bash", &serde_json::json!({"command": format!("touch {file}")})));
        }
    }
    mock::Reply::paced(mock::text_stream(&format!("{ANSWER} one two three")), Duration::from_millis(5))
}

struct World {
    /// In every path the world makes: the tests run side by side in one
    /// process under `cargo test`.
    name: String,
    root: PathBuf,
    api: String,
    relay: String,
    mock: String,
    account: AccountKey,
    /// The person's device list, which every machine is put on.
    people: std::sync::Mutex<People>,
    _registry: krowk_devregistry::Running,
    _mock: mock::Mock,
}

/// One machine: a home with its own device keys, on the person's device
/// list with a key of its own, a repository to work in, and a runtime
/// directory of its own for its daemon — short, as a socket path must be.
struct Machine {
    home: PathBuf,
    repo: PathBuf,
    run: PathBuf,
    env: Vec<(String, String)>,
    /// The registry as this machine's device.
    api: krowk_api::Client,
}

impl World {
    fn new(name: &str) -> World {
        let root = std::env::temp_dir().join(format!("krowk-sync-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        let api = format!("{}/v1", registry.url());
        let relay = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay_url = format!("ws://{}", relay.local_addr().unwrap());
        let roster = format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), e2e::hex(&krowk_devregistry::ticket_public_key()));
        let roster = krowk_harness::relay::Roster::parse(&roster).unwrap();
        std::thread::spawn(move || krowk_harness::relay::run(relay, krowk_harness::relay::Config { roster, origin: None, limits: Default::default(), state: None, origins: Vec::new(), whois: None, pin: None }));
        let m = mock::serve(model);
        World { name: name.into(), root: root.canonicalize().unwrap(), api, relay: relay_url, mock: m.url.clone(), account: AccountKey::generate(), people: Default::default(), _registry: registry, _mock: m }
    }

    fn machine(&self, name: &str) -> Machine {
        let home = self.root.join(name);
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // The inline renderer: the TUI's output here is read as it is sent.
        std::fs::create_dir_all(home.join(".krowk")).unwrap();
        std::fs::write(home.join(".krowk/config.json"), r#"{"tui": {"screen": "inline"}}"#).unwrap();
        let run = PathBuf::from(format!("/tmp/krowk-sc-{}-{name}-{}", self.name, std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ks = Keystore::new(&home.join(".krowk"));
        ks.recover(AccountKey::from_bytes(*self.account.as_bytes())).unwrap();
        let api = {
            let mut people = self.people.lock().unwrap();
            people.enlist(&ks, name);
            // A host reads the registry's list before it seals.
            people.publish(&self.api, TOKEN);
            people.client(&self.api, TOKEN, people.len() - 1)
        };
        let env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), home.display().to_string()),
            ("XDG_RUNTIME_DIR".into(), run.display().to_string()),
            ("KROWK_NO_UPDATE_CHECK".into(), "1".into()),
            ("KROWK_HOST_IDLE".into(), "5".into()),
            ("KROWK_API_URL".into(), self.api.clone()),
            ("KROWK_TOKEN".into(), device_list::key(TOKEN, name)),
            ("KROWK_RELAY_URL".into(), self.relay.clone()),
            // No tailscaled unless a test fakes one: never the one this
            // machine may run.
            ("KROWK_TAILSCALE_SOCKET".into(), run.join("no-tailscaled.sock").display().to_string()),
            ("ANTHROPIC_API_KEY".into(), "sk-test".into()),
            ("ANTHROPIC_BASE_URL".into(), self.mock.clone()),
        ];
        Machine { home, repo, run, env, api }
    }

    /// A machine that has not set sync up: a home with a config.json and
    /// no keys at all, its registry and relay `api` and `relay`.
    fn bare(&self, name: &str, api: &str, relay: &str) -> (PathBuf, Vec<(String, String)>, PathBuf) {
        let home = self.root.join(name);
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(home.join(".krowk")).unwrap();
        std::fs::write(home.join(".krowk/config.json"), "{}").unwrap();
        let run = PathBuf::from(format!("/tmp/krowk-sc-{}-{name}-{}", self.name, std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), home.display().to_string()),
            ("XDG_RUNTIME_DIR".into(), run.display().to_string()),
            ("KROWK_NO_UPDATE_CHECK".into(), "1".into()),
            ("KROWK_HOST_IDLE".into(), "5".into()),
            ("KROWK_API_URL".into(), api.into()),
            ("KROWK_TOKEN".into(), device_list::key(TOKEN, name)),
            ("KROWK_RELAY_URL".into(), relay.into()),
            ("KROWK_TAILSCALE_SOCKET".into(), run.join("no-tailscaled.sock").display().to_string()),
            ("ANTHROPIC_API_KEY".into(), "sk-test".into()),
            ("ANTHROPIC_BASE_URL".into(), self.mock.clone()),
        ];
        (repo, env, run)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Machine {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args).env_clear().envs(self.env.iter().map(|(k, v)| (k, v))).current_dir(&self.repo);
        c
    }

    /// The one session this machine has logged.
    fn session(&self) -> String {
        let dir = self.home.join(".krowk/sessions");
        std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.len() == 36).expect("a session was logged")
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.command(&["host", "stop", "--force"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir_all(&self.run);
    }
}

/// A running `krowk`, its stdout and stderr read line by line.
struct Running {
    child: Child,
    out: mpsc::Receiver<String>,
    err: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    fn spawn(mut c: Command) -> Running {
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let lines = |r: Box<dyn std::io::Read + Send>| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                for l in BufReader::new(r).lines().map_while(Result::ok) {
                    let _ = tx.send(l);
                }
            });
            rx
        };
        let out = lines(Box::new(child.stdout.take().unwrap()));
        let err = lines(Box::new(child.stderr.take().unwrap()));
        Running { child, out, err, seen: Vec::new() }
    }

    fn type_line(&mut self, line: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{line}").unwrap();
    }

    /// Reads stdout until a line passes `want`, failing on a command A
    /// refused.
    fn until(&mut self, what: &str, want: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        self.heard(what, true, want)
    }

    /// Reads stdout until a line passes `want`; with `refusing`, failing on
    /// a command A refused first.
    fn heard(&mut self, what: &str, refusing: bool, want: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let Ok(l) = self.out.recv_timeout(Duration::from_millis(100)) else { continue };
            self.seen.push(l.clone());
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else { continue };
            assert!(!(refusing && v["type"] == "sync.acked" && !v["error"].is_null()), "A refused a command: {l}");
            if want(&v) {
                return v;
            }
        }
        panic!("no {what} within 20s: {:#?}\nstderr: {:?}", self.seen, self.err.try_iter().collect::<Vec<_>>());
    }

    fn stderr(&self) -> Vec<String> {
        self.err.try_iter().collect()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is(kind: &'static str) -> impl Fn(&serde_json::Value) -> bool {
    move |v| v["type"] == kind
}

/// R-PERM-2 through the command line (todo 22a): B, attached with `krowk
/// sync attach`, allows one of A's tool calls with `/approve`, refuses
/// another with `/deny`, and stops a turn with `/interrupt`. Each
/// `approval.requested` line carries its requestId, and stderr says what
/// answers it.
#[test]
fn r_perm_2_sync_attach_approves_denies_and_interrupts_from_stdin() {
    let w = World::new("attach");
    let a = w.machine("a");
    let b = w.machine("b");
    let made = a.command(&["-p", "hello", "--model", "claude-sonnet-4-6"]).output().unwrap();
    assert!(made.status.success(), "A makes a session: {}", String::from_utf8_lossy(&made.stderr));
    let session = a.session();

    let mut host = Running::spawn(a.command(&["sync", "host", &session]));
    // B attaches once A's bridge has put the session in the registry.
    let deadline = Instant::now() + Duration::from_secs(20);
    while a.api.show_sync_session(&session).is_err() {
        assert!(Instant::now() < deadline && host.child.try_wait().unwrap().is_none(), "A syncs the session: {:?}", host.stderr());
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    view.until("the host", |v| v["type"] == "sync.host" && v["present"] == true);

    // Allowed: the file is made once B says so, and not before.
    view.type_line("make the allowed file");
    let req = view.until("an approval", is("approval.requested"));
    let id = req["requestId"].as_str().expect("the line carries its requestId").to_string();
    assert!(!a.repo.join("allowed.txt").exists(), "A waits on B");
    view.type_line(&format!("/approve {id}"));
    view.until("the turn's end", is("result"));
    assert!(a.repo.join("allowed.txt").exists(), "B's /approve ran the call on A");

    // Denied: the turn ends without it.
    view.type_line("make the denied file");
    let req = view.until("an approval", is("approval.requested"));
    let id = req["requestId"].as_str().unwrap().to_string();
    view.type_line(&format!("/deny {id}"));
    view.until("the turn's end", is("result"));
    assert!(!a.repo.join("denied.txt").exists(), "B's /deny refused the call");

    // Interrupted: part way through a long answer.
    view.type_line("give me a long answer");
    view.until("the answer starting", |v| v.to_string().contains("word3"));
    view.type_line("/interrupt");
    let result = view.until("the turn's end", is("result"));
    assert_eq!(result["status"], "interrupted", "B's /interrupt stopped the turn: {result}");
    assert!(!result["result"].as_str().unwrap_or_default().contains("word399"), "short of its end: {result}");

    // Said on stderr beside each approval line: what answers it.
    let said = view.stderr();
    assert!(said.iter().any(|l| l.contains("wants approval") && l.contains("/approve")), "stderr names the command that answers: {said:?}");
    assert!(host.child.try_wait().unwrap().is_none(), "A still hosts: {:?}", host.stderr());
}

/// R-SUB-12 through the command line: B, attached with `krowk sync
/// attach`, stops one subagent of A's turn with `/interrupt` and the id its
/// `subagent.started` line names, and the turn goes on to its answer.
#[test]
fn r_sub_12_sync_attach_stops_one_subagent_by_its_id() {
    let w = World::new("child");
    let a = w.machine("a");
    let b = w.machine("b");
    let (_host, session) = hosted(&w, &a);
    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    view.until("the host", |v| v["type"] == "sync.host" && v["present"] == true);
    view.type_line("start a slow subagent");
    let started = view.until("the subagent", is("subagent.started"));
    let child = started["subagentSessionId"].as_str().expect("the line names the child").to_string();
    view.until("the subagent typing", |v| v["type"] == "item.delta" && v["sessionId"] == child.as_str());
    view.type_line(&format!("/interrupt {child}"));
    let ended = view.until("the subagent's end", |v| v["type"] == "turn.completed" && v["sessionId"] == child.as_str());
    assert_eq!(ended["status"], "interrupted", "{ended}");
    let result = view.until("the turn's end", |v| v["type"] == "result" && v["sessionId"] == session.as_str());
    assert_eq!(result["status"], "completed", "the turn went on past its stopped subagent: {result}");
}

/// R-SUB-12, R-SUB-11: a TUI attached to a synced session opens a child of
/// it in the child view — its prompt first and what it has said so far,
/// from what the session's chunks and stream carried, with no log of it on
/// this machine — sees what it says next arrive, and stops it alone with
/// `x`, on the host.
#[test]
fn r_sub_12_an_attached_tui_opens_a_childs_transcript_and_stops_it() {
    let w = World::new("childview");
    let (a, b, c) = (w.machine("a"), w.machine("b"), w.machine("c"));
    let (host, session) = hosted(&w, &a);
    // B, on the command line, starts the child; C opens it in its TUI.
    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    view.until("the host", |v| v["type"] == "sync.host" && v["present"] == true);
    view.type_line("start a slower subagent");
    let started = view.until("the subagent", is("subagent.started"));
    let child = started["subagentSessionId"].as_str().expect("the line names the child").to_string();
    view.until("the subagent's search", |v| v["type"] == "item.completed" && v["sessionId"] == child.as_str() && v["item"]["kind"] == "toolResult");
    // In tmux, so what is on the screen is read, not the bytes that drew it.
    let mut env = c.env.clone();
    env.push(("TERM".into(), "xterm-256color".into()));
    let Some(tm) = tmux::Tmux::start("childview", 120, 40, &c.repo, &env, &["sync", "attach", &session]) else { return };
    let until = |what: &str, ok: &dyn Fn(&str) -> bool| -> String {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let screen = tm.screen();
            if ok(&screen) {
                return screen;
            }
            assert!(Instant::now() < deadline, "no {what}: {screen}\nA: {:?}", host.stderr());
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // The child view is open: its header first, its footer last, and
    // nothing of the conversation or the Agents overlay on the screen.
    let in_view = |s: &str| {
        let rows: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
        rows.first().is_some_and(|r| r.contains("a slower look · by krowk")) && rows.last().is_some_and(|r| r.contains("esc back · ← → siblings · x stop")) && !s.contains("enter opens")
    };
    until("attach", &|s| s.contains("Attached to a"));
    assert!(!c.home.join(".krowk/sessions").join(&child).exists(), "C holds no log of the child");
    tm.keys(&["C-g"]);
    until("the Agents overlay", &|s| s.contains("enter opens"));
    tm.keys(&["Enter"]);
    // Its history, read here: the parent's prompt, then its finished search.
    let screen = until("the child's history", &|s| in_view(s) && s.contains("look slower") && s.contains("Find *"));
    assert!(!screen.contains("could not be read"), "its history is read here: {screen}");
    assert!(!screen.contains("slowword"), "nothing of its answer yet: {screen}");
    // Its answer, begun only now, drawn in the view as it arrives.
    SLOWER.open();
    until("the answer, live in the view", &|s| in_view(s) && s.contains("slowword3"));
    tm.keys(&["x"]);
    let ended = view.until("the subagent's end", |v| v["type"] == "turn.completed" && v["sessionId"] == child.as_str());
    assert_eq!(ended["status"], "interrupted", "{ended}");
    let result = view.until("the turn's end", |v| v["type"] == "result" && v["sessionId"] == session.as_str());
    assert_eq!(result["status"], "completed", "the turn went on past its stopped subagent: {result}");
    let log = std::fs::read_to_string(a.home.join(".krowk/sessions").join(&child).join("events.jsonl")).unwrap();
    assert!(log.contains("\"status\":\"interrupted\""), "stopped on A: {log}");
    until("the view saying so", &|s| in_view(s) && s.contains("interrupted after"));
}

/// Todo 29b: a prompt from `krowk sync attach` that A's daemon refuses
/// before its turn starts — a Claude Code session made with `-p --trust`,
/// its repository on no trusted list — is acked with the refusal, not
/// acked as taken and then never heard of again. Once A trusts the
/// repository, B's next prompt runs, with nothing restarted.
#[test]
fn sync_attach_hears_a_prompt_the_host_refuses_as_untrusted_and_the_next_runs_once_trusted() {
    let w = World::new("untrusted");
    let mut a = w.machine("a");
    let b = w.machine("b");
    // A fake `claude` first on A's PATH: Claude Code, signed in.
    let bin = a.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude/fake-claude"), bin.join("claude")).unwrap();
    for (k, v) in a.env.iter_mut() {
        if k == "PATH" {
            *v = format!("{}:{v}", bin.display());
        }
    }
    let added = a.command(&["providers", "add", "claude", "--json"]).output().unwrap();
    assert!(added.status.success(), "A adds Claude Code: {}", String::from_utf8_lossy(&added.stderr));
    let made = a.command(&["-p", "hello", "--model", "claude/sonnet", "--trust"]).output().unwrap();
    assert!(made.status.success(), "A makes a session: {}", String::from_utf8_lossy(&made.stderr));
    let trusted = a.home.join(".krowk/trusted.json");
    assert!(!trusted.exists(), "--trust is for one run");
    let session = a.session();
    let host = Running::spawn(a.command(&["sync", "host", &session]));
    let deadline = Instant::now() + Duration::from_secs(20);
    while a.api.show_sync_session(&session).is_err() {
        assert!(Instant::now() < deadline, "A syncs the session: {:?}", host.stderr());
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    view.until("the host", |v| v["type"] == "sync.host" && v["present"] == true);
    view.type_line("hello from b");
    let acked = view.heard("the refusal", false, is("sync.acked"));
    assert!(acked["error"].as_str().is_some_and(|e| e.contains("untrusted_directory")), "B hears why A refused: {acked}\nA: {:?}", host.stderr());
    assert_eq!(view.seen.iter().filter(|l| l.contains("\"turn.started\"")).count(), 1, "no turn ran on A but its first: {:?}", view.seen);

    std::fs::write(&trusted, serde_json::json!({"directories": [a.repo]}).to_string()).unwrap();
    view.type_line("hello again from b");
    let acked = view.until("the ack", is("sync.acked"));
    assert!(acked["error"].is_null(), "{acked}");
    let result = view.until("the turn's end", |v| v["type"] == "result" && v["sessionId"] == session.as_str());
    assert_eq!(result["status"], "completed", "the turn ran once A trusted the repository: {result}");
}

/// A's session made with `krowk -p` and hosted by `krowk sync host`, synced
/// to the registry: the host's process, and the session's id.
fn hosted(w: &World, a: &Machine) -> (Running, String) {
    let made = a.command(&["-p", "hello", "--model", "claude-sonnet-4-6"]).output().unwrap();
    assert!(made.status.success(), "A makes a session: {}", String::from_utf8_lossy(&made.stderr));
    let session = a.session();
    let host = Running::spawn(a.command(&["sync", "host", &session]));
    let deadline = Instant::now() + Duration::from_secs(20);
    while a.api.show_sync_session(&session).is_err() {
        assert!(Instant::now() < deadline, "A syncs the session: {:?}", host.stderr());
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = w;
    (host, session)
}

/// D11: `krowk sync attach` with a terminal opens the TUI on the session —
/// its history drawn as a resumed session's is, the host's presence on the
/// status line, a prompt typed in the prompt box answered by A's model, and
/// an approval answered with the dialog's `y`. Mode switches, which the
/// host would refuse, are refused here, saying so.
#[test]
fn d11_sync_attach_on_a_terminal_draws_the_session_in_the_tui() {
    let w = World::new("tui");
    let a = w.machine("a");
    let b = w.machine("b");
    let (host, session) = hosted(&w, &a);
    let mut c = b.command(&["sync", "attach", &session]);
    c.env("TERM", "xterm-256color");
    let mut t = pty::Pty::spawn(c, 120, 40);
    let wait = |t: &pty::Pty, what: &str| assert!(t.wait_for(what, Duration::from_secs(20)).is_some(), "no {what:?}: {:?}\nA: {:?}", t.text(), host.stderr());
    // History: A's first answer, from the chunks; then the attach line,
    // naming A by its name on the device list, and A on the status line.
    // No splash: the session is picked up, not opened.
    wait(&t, ANSWER);
    // The pty's text is raw: the glyph and the words are styled apart, and
    // the status line is drawn a run of cells at a time.
    // `krowk sync host` seals no title: A is named alone, never the id.
    wait(&t, "⇄ \u{1b}[0mAttached to a\r");
    wait(&t, "●");
    assert!(!t.text().contains("Directory:"), "no splash mid-session: {:?}", t.text());
    // A prompt round trip.
    t.write(b"tui round trip\r");
    wait(&t, TUI_ANSWER);
    // And its turn ended here, on the host's result: the footer under it,
    // after the one the history drew.
    let deadline = Instant::now() + Duration::from_secs(20);
    while t.text().matches("Worked for").count() < 2 {
        assert!(Instant::now() < deadline, "the turn ends: {:?}", t.text());
        std::thread::sleep(Duration::from_millis(50));
    }
    // An approval, answered in the dialog. The dialog takes no key until it
    // has been up a moment (a key typed ahead is no answer).
    t.write(b"make the allowed file\r");
    // On the screen, not in the bytes: the dialog's row is drawn over the
    // one before it, and a cell the two share (a space) is not drawn again.
    assert!(t.wait_on_screen("y allow once", Duration::from_secs(20)).is_some(), "no dialog: {}\nA: {:?}", t.screen(), host.stderr());
    assert!(!a.repo.join("allowed.txt").exists(), "A waits on B");
    std::thread::sleep(Duration::from_millis(600));
    t.write(b"y");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !a.repo.join("allowed.txt").exists() {
        assert!(Instant::now() < deadline, "B's y ran the call on A: {:?}", t.text());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!t.text().contains("for this project"), "a viewer is not offered A's project rules");
    // The host caps remote prompts at its default mode: no switch offered.
    t.write(b"/mode plan\r");
    assert!(t.wait_on_screen("is the host's to change", Duration::from_secs(20)).is_some(), "no refusal: {}", t.screen());
    t.write(b"\x04\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()), "B leaves: {:?}", t.text());
}

/// D11: `krowk --resume` of a session only another machine holds opens the
/// TUI on it through sync, on a terminal, as `krowk sync attach` does.
#[test]
fn d11_resume_of_a_synced_session_on_a_terminal_opens_the_tui() {
    let w = World::new("tuiresume");
    let a = w.machine("a");
    let b = w.machine("b");
    let (host, session) = hosted(&w, &a);
    let mut c = b.command(&["--resume", &session]);
    c.env("TERM", "xterm-256color");
    let mut t = pty::Pty::spawn(c, 120, 40);
    for what in [ANSWER, "Attached to", "●"] {
        assert!(t.wait_for(what, Duration::from_secs(20)).is_some(), "no {what:?}: {:?}\nA: {:?}", t.text(), host.stderr());
    }
    t.write(b"\x04\x04");
    assert!(t.wait(Duration::from_secs(10)).is_some_and(|s| s.success()), "B leaves: {:?}", t.text());
}

/// D11: `krowk sync attach` printing stream-json ends when its stdin does,
/// once the command sent before it ended has been answered.
#[test]
fn d11_sync_attach_as_jsonl_ends_with_stdin_once_its_commands_are_answered() {
    let w = World::new("eof");
    let a = w.machine("a");
    let b = w.machine("b");
    let (_host, session) = hosted(&w, &a);
    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    view.until("the host", |v| v["type"] == "sync.host" && v["present"] == true);
    view.type_line("hello again");
    drop(view.child.stdin.take());
    view.until("its ack", is("sync.acked"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = view.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "attach ends with stdin: {:?}", view.seen);
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}: {:?}", view.stderr());
}

/// Todo 22b: `krowk sync host` for a session this machine has no log of
/// ends at once with the fix, and writes nothing to the registry — before,
/// it took the session's lease and then waited, silent, forever.
#[test]
fn sync_host_of_a_session_this_machine_does_not_have_fails_fast() {
    let w = World::new("unknown");
    let a = w.machine("a");
    let id = "01a0ec7b-2222-7000-8000-000000000022";
    let started = Instant::now();
    let mut c = a.command(&["sync", "host", id]);
    let out = c.stdin(Stdio::null()).output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "it did not wait: {:?}", started.elapsed());
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no session") && err.contains("krowk sync host"), "says why and what to do: {err}");
    assert_eq!(a.api.show_sync_session(id).unwrap_err().status, 404, "nothing was made in the registry");
}

/// #206's review, M2: before it seals anything, a host takes the device
/// list from the registry and extends its pin with it, so a machine that
/// slept through a removal never seals under the generation it left
/// behind. A registry that cannot be asked is a refusal — the kept list is
/// never used in its place.
#[test]
fn sync_host_refuses_when_the_registry_cannot_give_it_the_device_list() {
    let w = World::new("stale-list");
    let a = w.machine("a");
    let id = "01a0ec7b-3333-7000-8000-000000000033";
    let log = a.home.join(".krowk/sessions").join(id);
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(log.join(krowk_harness::log::EVENTS_FILE), "").unwrap();
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_api = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    let started = Instant::now();
    let out = a.command(&["sync", "host", id]).env("KROWK_API_URL", &dead_api).stdin(Stdio::null()).output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(60), "it did not wait: {:?}", started.elapsed());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("device list"), "refused, and says why: {err}");
    assert_eq!(a.api.show_sync_session(id).unwrap_err().status, 404, "nothing was sealed or made in the registry");
}

/// A native session stored before its krowk.db row took its log's id is
/// listed under another id; `sync host` takes that one too, and hosts the
/// log it binds to rather than refusing it as unknown.
#[test]
fn sync_host_takes_the_id_an_older_store_listed_a_session_under() {
    let w = World::new("store-id");
    let a = w.machine("a");
    let log_id = "01a0ec7b-4444-7000-8000-000000000044";
    let log = a.home.join(".krowk/sessions").join(log_id);
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(log.join(krowk_harness::log::EVENTS_FILE), "").unwrap();
    let home = a.home.display().to_string();
    let env = |k: &str| if k == "HOME" { home.clone() } else { String::new() };
    let conn = krowk_store::open(&env).unwrap();
    let binding = krowk_store::Binding { provider: "krowk".into(), harness: "krowk".into(), foreign_session_id: log_id.into(), ..Default::default() };
    let th = krowk_store::Thread { worktree: krowk_store::Worktree { path: a.repo.display().to_string(), ..Default::default() }, binding, ..Default::default() };
    krowk_store::Writer::new(&conn).ingest(&th).unwrap();
    let store_id: String = conn.query_row("SELECT id FROM session", [], |r| r.get(0)).unwrap();
    assert_ne!(store_id, log_id, "a row minted its own id, as before");

    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_api = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    let out = a.command(&["sync", "host", &store_id]).env("KROWK_API_URL", &dead_api).stdin(Stdio::null()).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("no session"), "the store's id names the log: {err}");
    assert!(!out.status.success() && err.contains("device list"), "it got as far as the registry: {err}");
}

/// A tailscaled on a unix socket in `dir` answering `status` with `status`.
fn tailscaled(dir: &std::path::Path, status: serde_json::Value) -> PathBuf {
    let socket = dir.join("ts.sock");
    let l = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                match std::io::Read::read(&mut c, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
            }
            let _ = write!(c, "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{status}");
        }
    });
    socket
}

fn on_tailscale(m: &mut Machine, status: serde_json::Value) {
    let socket = tailscaled(&m.run, status);
    m.env.retain(|(k, _)| k != "KROWK_TAILSCALE_SOCKET");
    m.env.push(("KROWK_TAILSCALE_SOCKET".into(), socket.display().to_string()));
}

/// Ticket 43: on a tailnet with no tags and no policy, `krowk hosts` on B
/// lists A — the machine of B's person that hosts a synced session — by
/// its device list name, hosting, and reachable directly over the node A
/// kept in the session's sealed index. A peer on the tailnet that is not
/// on the device list, and B itself, which hosts nothing, are not listed.
/// With Tailscale down on B, A is listed still, not direct.
#[test]
fn r_net_4_hosts_lists_your_machines_that_host_with_no_tailscale_tags() {
    let w = World::new("hosts");
    let mut a = w.machine("a");
    let mut b = w.machine("b");
    on_tailscale(&mut a, serde_json::json!({"BackendState": "Running", "Self": {"HostName": "a-box", "DNSName": "a-box.tail1.ts.net.", "TailscaleIPs": ["100.64.0.1"], "UserID": 1, "Online": true}, "Peer": {}}));
    let (host, session) = hosted(&w, &a);
    // Hosting is the lease, which the bridge takes after it publishes.
    let deadline = Instant::now() + Duration::from_secs(20);
    while a.api.show_sync_session(&session).ok().and_then(|s| s.lease).is_none() {
        assert!(Instant::now() < deadline, "A holds the lease: {:?}", host.stderr());
        std::thread::sleep(Duration::from_millis(50));
    }
    let peers = serde_json::json!({
        "k1": {"HostName": "a-box", "DNSName": "a-box.tail1.ts.net.", "TailscaleIPs": ["100.64.0.1"], "UserID": 1, "Online": true},
        "k2": {"HostName": "stranger", "DNSName": "stranger.tail1.ts.net.", "TailscaleIPs": ["100.64.0.9"], "UserID": 2, "Online": true}});
    let b_plain = b.env.clone();
    on_tailscale(&mut b, serde_json::json!({"BackendState": "Running", "Self": {"HostName": "b-box", "DNSName": "b-box.tail1.ts.net.", "TailscaleIPs": ["100.64.0.3"], "UserID": 1, "Online": true}, "Peer": peers}));

    let out = b.command(&["hosts", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hosts = v["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1, "A alone: {v}");
    let h = &hosts[0];
    assert_eq!((h["name"].as_str(), h["hosting"].as_bool(), h["online"].as_bool(), h["direct"].as_bool(), h["thisMachine"].as_bool(), h["sessions"].as_u64()), (Some("a"), Some(true), Some(true), Some(true), Some(false), Some(1)), "{h}");
    assert_eq!(h["dnsName"], "a-box.tail1.ts.net");

    let human = b.command(&["hosts", "--format", "human"]).output().unwrap();
    let human = String::from_utf8_lossy(&human.stdout);
    assert_eq!(human.trim(), "a  hosting  reachable directly  1 session", "{human}");

    b.env = b_plain;
    let out = b.command(&["hosts", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["tailscale"], false);
    assert_eq!((v["hosts"][0]["name"].as_str(), v["hosts"][0]["direct"].as_bool()), (Some("a"), Some(false)), "{v}");
}

/// A tailscaled on a unix socket in `dir`, answering `status` for a node
/// whose tailnet address is loopback, in `state`.
fn fake_tailscaled(dir: &std::path::Path, state: &'static str) -> PathBuf {
    tailscaled(dir, serde_json::json!({"BackendState": state, "Self": {"HostName": "a", "DNSName": "localhost.", "TailscaleIPs": ["127.0.0.1"], "Addrs": [], "UserID": 1, "Online": true}, "Peer": {}}))
}

/// What B's `sync attach` says of its path, until `want` passes.
fn path_until(view: &mut Running, want: &str) -> serde_json::Value {
    view.until(want, |v| v["type"] == "sync.path" && v["path"] == want)
}

/// Ticket 42: with Tailscale up and no krowk variable set, the host fetches
/// the registry's ticket keys itself, keeps them, and B's session moves to
/// the direct path. It fetches at every hosting: keys kept from before — a
/// key since rotated out — give way to what the registry publishes now.
#[test]
fn r_net_1_sync_host_goes_direct_with_the_ticket_keys_the_registry_publishes() {
    let w = World::new("direct-keys");
    let mut a = w.machine("a");
    let b = w.machine("b");
    let socket = fake_tailscaled(&a.run, "Running");
    a.env.retain(|(k, _)| k != "KROWK_TAILSCALE_SOCKET");
    a.env.push(("KROWK_TAILSCALE_SOCKET".into(), socket.display().to_string()));
    assert!(!a.env.iter().any(|(k, _)| k == "KROWK_RELAY_TICKET_KEYS"));
    let kept = a.home.join(".krowk/cache/relay-ticket-keys.json");
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    std::fs::write(&kept, serde_json::json!({"registry": w.api, "fetchedMs": now, "ticketKeys": {e2e::hex(&krowk_devregistry::TICKET_KID): "ab".repeat(32)}}).to_string()).unwrap();

    let (host, session) = hosted(&w, &a);
    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    let moved = path_until(&mut view, "direct over Tailscale");
    assert!(moved["via"].as_str().unwrap_or_default().starts_with("ws://127.0.0.1:"), "{moved}");
    assert!(!host.stderr().iter().any(|l| l.contains("no direct path")), "A offered a direct path");

    let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
    assert_eq!(kept["registry"], w.api.as_str());
    assert_eq!(kept["ticketKeys"][e2e::hex(&krowk_devregistry::TICKET_KID)], e2e::hex(&krowk_devregistry::ticket_public_key()));
}

/// Ticket 42: with Tailscale stopped, the host says in one line that there
/// is no direct path and why, and the session goes by the relay; it asks
/// the registry for no keys it cannot use.
#[test]
fn r_net_1_with_tailscale_stopped_the_host_says_why_in_one_line_and_goes_by_the_relay() {
    let w = World::new("direct-stopped");
    let mut a = w.machine("a");
    let b = w.machine("b");
    let socket = fake_tailscaled(&a.run, "Stopped");
    a.env.retain(|(k, _)| k != "KROWK_TAILSCALE_SOCKET");
    a.env.push(("KROWK_TAILSCALE_SOCKET".into(), socket.display().to_string()));

    let (host, session) = hosted(&w, &a);
    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    path_until(&mut view, "relay");
    view.type_line("still by the relay");
    view.until("the turn's end", is("result"));
    let said: Vec<_> = host.stderr().into_iter().filter(|l| l.contains("direct path")).collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("Tailscale isn't running: it is Stopped"), "{said:?}");
    assert!(!a.home.join(".krowk/cache/relay-ticket-keys.json").exists(), "no keys fetched");
}

/// Ticket 42: `KROWK_RELAY_TICKET_KEYS` still wins over the registry's
/// keys, for a stand-in registry: keys that are not the registry's let no
/// viewer onto the direct path, though the registry publishes its own.
#[test]
fn r_net_1_krowk_relay_ticket_keys_overrides_the_published_keys() {
    let w = World::new("direct-override");
    let mut a = w.machine("a");
    let b = w.machine("b");
    let socket = fake_tailscaled(&a.run, "Running");
    let keys = a.run.join("keys.json");
    std::fs::write(&keys, format!(r#"{{"ticketKeys": {{"{}": "{}"}}}}"#, e2e::hex(&krowk_devregistry::TICKET_KID), "ab".repeat(32))).unwrap();
    a.env.retain(|(k, _)| k != "KROWK_TAILSCALE_SOCKET");
    a.env.push(("KROWK_TAILSCALE_SOCKET".into(), socket.display().to_string()));
    a.env.push(("KROWK_RELAY_TICKET_KEYS".into(), keys.display().to_string()));

    let (host, session) = hosted(&w, &a);
    let mut view = Running::spawn(b.command(&["sync", "attach", &session]));
    path_until(&mut view, "relay");
    // Past the time B's race for a direct path takes.
    std::thread::sleep(krowk_harness::sync::viewer::PROBE + Duration::from_secs(2));
    view.type_line("still by the relay");
    view.until("the turn's end", is("result"));
    assert!(!view.seen.iter().any(|l| l.contains("direct over")), "B joined under keys A does not hold: {:?}", view.seen);
    assert!(!a.home.join(".krowk/cache/relay-ticket-keys.json").exists(), "the registry's keys were not fetched");
    let said = host.stderr();
    assert!(!said.iter().any(|l| l.contains("no direct path")), "A listened, under the named keys: {said:?}");
    assert!(said.iter().any(|l| l.contains("bad_ticket")), "B raced the direct path and its ticket was refused there: {said:?}");
}

/// Waits until `m`'s registry shows `session` with no lease, or panics.
fn no_lease(m: &Machine, session: &str, within: Duration) {
    let deadline = Instant::now() + within;
    while a_lease(m, session).is_some() {
        assert!(Instant::now() < deadline, "A still holds session {session}'s lease");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn a_lease(m: &Machine, session: &str) -> Option<String> {
    m.api.show_sync_session(session).ok().and_then(|s| s.lease).map(|l| l.device)
}

/// `krowk sync status --json`'s data on `m`, with `env` added.
fn sync_status(m: &Machine, env: &[(&str, &str)]) -> serde_json::Value {
    let out = m.command(&["sync", "status", "--json"]).envs(env.iter().copied()).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["data"].clone()
}

/// R-SYNC-3, R-HOST-2 (ticket 95): A runs a turn in its host daemon with no
/// sync command — kept up as `krowk host enable` keeps it, with no idle
/// exit — and B lists the session within 5 seconds of the command's start,
/// then follows a prompt it sends to its result. `krowk sync status` on A
/// counts the bridge, `krowk sync host` of the session says it is synced
/// already, and `krowk host stop` gives the lease back as the daemon exits.
#[test]
fn r_sync_3_a_daemon_session_is_listed_elsewhere_with_no_sync_command() {
    let w = World::new("auto");
    let mut a = w.machine("a");
    let b = w.machine("b");
    a.env.retain(|(k, _)| k != "KROWK_HOST_IDLE");
    a.env.push(("KROWK_HOST_IDLE".into(), "0".into()));
    let started = Instant::now();
    let made = a.command(&["-p", "--daemon", "hello", "--model", "claude-sonnet-4-6"]).stdin(Stdio::null()).output().unwrap();
    assert!(made.status.success(), "A runs a turn in its daemon: {}", String::from_utf8_lossy(&made.stderr));
    let session = a.session();
    let host_log = || std::fs::read_to_string(a.home.join(".krowk/host.log")).unwrap_or_default();

    let listed = loop {
        let out = b.command(&["sync", "sessions", "--json"]).stdin(Stdio::null()).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        if v["sessions"].as_array().is_some_and(|l| l.iter().any(|s| s["id"] == session.as_str())) {
            break started.elapsed();
        }
        assert!(started.elapsed() < Duration::from_secs(5), "B never listed {session} within 5 s: {v}\nA's host.log: {}", host_log());
        std::thread::sleep(Duration::from_millis(100));
    };
    eprintln!("listed on B {listed:?} after A's command started");

    // B follows it, live: a prompt it sends runs on A, and its result comes back.
    let mut view = Running::spawn(b.command(&["sync", "attach", &session, "--json"]));
    view.until("the host", |v| v["type"] == "sync.host" && v["present"] == true);
    view.type_line("hello from b");
    let result = view.until("the turn's end", |v| v["type"] == "result" && v["sessionId"] == session.as_str());
    assert_eq!(result["status"], "completed", "{result}");
    assert!(result["result"].as_str().is_some_and(|r| r.contains(ANSWER)), "{result}");
    drop(view);

    let data = sync_status(&a, &[]);
    assert!(data["bridged"].as_u64().is_some_and(|n| n >= 1), "A's daemon bridges it: {data}");
    assert_eq!((data["autoSync"]["on"].as_bool(), data["directory"]["synced"].as_bool()), (Some(true), Some(true)), "{data}");
    // Off as this shell reads it, while the daemon — started with another
    // environment — syncs: said, rather than left as a plain "off".
    let human = a.command(&["sync", "status", "--format", "human"]).env("KROWK_SYNC_AUTO", "0").stdin(Stdio::null()).output().unwrap();
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("auto-sync: off (KROWK_SYNC_AUTO)") && human.contains("the running host daemon still syncs 1 session(s)"), "{human}");

    let again = a.command(&["sync", "host", &session]).stdin(Stdio::null()).output().unwrap();
    let said = String::from_utf8_lossy(&again.stdout);
    assert!(again.status.success() && said.contains("synced already by this machine's host daemon"), "{said}{}", String::from_utf8_lossy(&again.stderr));

    assert!(a_lease(&a, &session).is_some(), "A holds the lease while its daemon runs");
    let stopped = a.command(&["host", "stop"]).stdin(Stdio::null()).output().unwrap();
    assert!(stopped.status.success(), "{}", String::from_utf8_lossy(&stopped.stderr));
    // The exit grace gave it back, rather than leaving it to lapse (60 s).
    no_lease(&a, &session, Duration::from_secs(15));
}

/// R-SYNC-3 (ticket 95): `krowk sync status` says whether automatic sync is
/// on and why not, whether this directory is synced or what opts it out,
/// and how many sessions the host daemon bridges — none running here.
#[test]
fn r_sync_3_sync_status_says_what_applies_here() {
    let w = World::new("autostatus");
    let a = w.machine("a");
    let data = sync_status(&a, &[]);
    assert_eq!(data["autoSync"], serde_json::json!({"on": true, "why": null}), "{data}");
    assert_eq!(data["directory"], serde_json::json!({"synced": true, "optedOutBy": null}), "{data}");
    assert_eq!(data["bridged"], 0, "{data}");
    let human = a.command(&["sync", "status", "--format", "human"]).stdin(Stdio::null()).output().unwrap();
    let human = String::from_utf8_lossy(&human.stdout);
    for line in ["auto-sync: on", "this directory: synced", "bridged: 0 (no host daemon running)"] {
        assert!(human.lines().any(|l| l == line), "{line:?} in {human}");
    }

    let data = sync_status(&a, &[("KROWK_SYNC_AUTO", "0")]);
    assert_eq!(data["autoSync"], serde_json::json!({"on": false, "why": "KROWK_SYNC_AUTO"}), "{data}");
    assert_eq!(data["directory"]["synced"], false, "{data}");

    let repo = a.repo.display().to_string();
    std::fs::write(a.home.join(".krowk/config.json"), serde_json::json!({"tui": {"screen": "inline"}, "sync": {"exclude": [repo]}}).to_string()).unwrap();
    let data = sync_status(&a, &[]);
    assert_eq!(data["directory"]["synced"], false, "{data}");
    assert!(data["directory"]["optedOutBy"].as_str().is_some_and(|by| by.contains("sync.exclude") && by.contains(&repo)), "names the entry: {data}");
    let human = a.command(&["sync", "status", "--format", "human"]).stdin(Stdio::null()).output().unwrap();
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.lines().any(|l| l == format!("this directory: opted out by sync.exclude ({repo})")), "{human}");

    std::fs::write(a.home.join(".krowk/config.json"), r#"{"sync": {"auto": "no"}}"#).unwrap();
    let data = sync_status(&a, &[]);
    assert_eq!(data["autoSync"]["on"], false, "a policy that does not read is off: {data}");
    assert!(data["autoSync"]["why"].as_str().is_some_and(|w| w.contains("sync.auto")), "{data}");
}

/// A listener that counts the connections made to it.
fn counting() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = n.clone();
    std::thread::spawn(move || {
        for c in l.incoming() {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(c);
        }
    });
    (addr, n)
}

/// R-SYNC-1 (ticket 95): a machine that has not set sync up runs a turn in
/// its host daemon as before, and neither the registry nor the relay hears
/// from it — not one connection. `krowk sync status` fails `not_set_up`,
/// saying automatic sync is off until it is.
#[test]
fn r_sync_1_a_machine_without_sync_makes_no_sync_call() {
    let w = World::new("nosync");
    let ((api, api_seen), (relay, relay_seen)) = (counting(), counting());
    let (repo, env, run) = w.bare("bare", &format!("http://{api}/v1"), &format!("ws://{relay}"));
    let command = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args).env_clear().envs(env.iter().map(|(k, v)| (k, v))).current_dir(&repo).stdin(Stdio::null());
        c
    };
    let made = command(&["-p", "--daemon", "hello", "--model", "claude-sonnet-4-6"]).output().unwrap();
    assert!(made.status.success(), "the turn runs: {}", String::from_utf8_lossy(&made.stderr));
    assert!(String::from_utf8_lossy(&made.stdout).contains(ANSWER));
    std::thread::sleep(Duration::from_secs(3));
    let seen = (api_seen.load(std::sync::atomic::Ordering::SeqCst), relay_seen.load(std::sync::atomic::Ordering::SeqCst));
    // The supervisor was told of the turn, and its check stopped at the set-up.
    let log = std::fs::read_to_string(repo.parent().unwrap().join(".krowk/host.log")).unwrap_or_default();

    let status = command(&["sync", "status"]).output().unwrap();
    let _ = command(&["host", "stop", "--force"]).output();
    let _ = std::fs::remove_dir_all(&run);
    assert_eq!(seen, (0, 0), "no connection to the registry or the relay");
    assert!(log.contains("automatic sync is not set up here"), "the daemon's check ran: {log}");
    let err = String::from_utf8_lossy(&status.stderr);
    assert!(!status.status.success() && err.contains("not_set_up") && err.contains("automatic sync is off until sync is set up here"), "{err}");
}

/// R-SYNC-3 (ticket 95), failing closed in the daemon itself: on a machine
/// with sync set up, a turn in the host daemon syncs nothing when
/// config.json turns automatic sync off, or when the session's directory
/// opts out with `.krowk/no-sync` — the registry never hears of the
/// session, and host.log says why.
#[test]
fn r_sync_3_the_daemon_syncs_nothing_the_policy_leaves_out() {
    let w = World::new("autooff");
    let off = w.machine("a");
    std::fs::write(off.home.join(".krowk/config.json"), r#"{"sync": {"auto": false}}"#).unwrap();
    let marked = w.machine("b");
    std::fs::create_dir_all(marked.repo.join(".krowk")).unwrap();
    std::fs::write(marked.repo.join(".krowk/no-sync"), "").unwrap();
    for (m, why) in [(&off, "automatic sync is off (sync.auto in config.json)"), (&marked, "opts")] {
        let made = m.command(&["-p", "--daemon", "hello", "--model", "claude-sonnet-4-6"]).stdin(Stdio::null()).output().unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        let session = m.session();
        let deadline = Instant::now() + Duration::from_secs(10);
        let log = loop {
            let log = std::fs::read_to_string(m.home.join(".krowk/host.log")).unwrap_or_default();
            if log.contains(why) {
                break log;
            }
            assert!(Instant::now() < deadline, "host.log never said {why:?}: {log}");
            std::thread::sleep(Duration::from_millis(50));
        };
        if why == "opts" {
            assert!(log.contains(&format!("session {session} is not synced automatically")) && log.contains(".krowk/no-sync"), "{log}");
        }
        assert_eq!(m.api.show_sync_session(&session).unwrap_err().status, 404, "the registry never heard of {session}");
    }
}
