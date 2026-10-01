//! `krowk sync host` and `krowk sync attach` as a person runs them, two
//! machines in one test: A hosts a session it made with `krowk -p`, B
//! attaches it and answers A's approvals and interrupts its turn by typing
//! slash commands on stdin (todo 22a). And `sync host` for a session A does
//! not have fails at once, saying so, rather than hanging (todo 22b).
//! Against the stand-in registry, the reference relay and a mock model.

#![cfg(all(feature = "harness", unix))]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use krowk_client::e2e::{self, AccountKey, SigningKey};
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

/// A model that answers with the marker, except when asked to make a
/// file, which it does with `bash` — an approval — and a long answer, paced
/// so that it can be interrupted part way.
fn model(body: &serde_json::Value, _n: usize) -> mock::Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or_default();
    let has_result = last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    let text = last.to_string();
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

/// One machine: a home with its own device keys and the shared account
/// key, registered, a repository to work in, and a runtime directory of its
/// own for its daemon — short, as a socket path must be.
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
        let run = PathBuf::from(format!("/tmp/krowk-sc-{}-{name}-{}", self.name, std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ks = Keystore::new(&home.join(".krowk"));
        ks.recover(AccountKey::from_bytes(*self.account.as_bytes())).unwrap();
        let (device, signing) = (ks.device().unwrap().unwrap(), ks.signing_key().unwrap());
        {
            let mut people = self.people.lock().unwrap();
            people.enlist(&ks, name);
            // A host reads the registry's list before it seals.
            people.publish(&self.api, TOKEN);
        }
        let api = krowk_api::Client::new(&self.api, TOKEN).signed_by(e2e::DeviceSigner::new(device.id(), SigningKey::from_secret(&*signing.secret_bytes()).unwrap()).shared());
        api.register_device(&e2e::hex(&device.public().0), &e2e::hex(&signing.public().0), name, &self.account.id().to_string()).unwrap();
        let env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), home.display().to_string()),
            ("XDG_RUNTIME_DIR".into(), run.display().to_string()),
            ("KROWK_NO_UPDATE_CHECK".into(), "1".into()),
            ("KROWK_HOST_IDLE".into(), "5".into()),
            ("KROWK_API_URL".into(), self.api.clone()),
            ("KROWK_TOKEN".into(), TOKEN.into()),
            ("KROWK_RELAY_URL".into(), self.relay.clone()),
            ("ANTHROPIC_API_KEY".into(), "sk-test".into()),
            ("ANTHROPIC_BASE_URL".into(), self.mock.clone()),
        ];
        Machine { home, repo, run, env, api }
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
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let Ok(l) = self.out.recv_timeout(Duration::from_millis(100)) else { continue };
            self.seen.push(l.clone());
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else { continue };
            assert!(!(v["type"] == "sync.acked" && !v["error"].is_null()), "A refused a command: {l}");
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
    assert!(a.api.show_sync_session(id).is_err(), "nothing was made in the registry");
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
    assert!(a.api.show_sync_session(id).is_err(), "nothing was sealed or made in the registry");
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
