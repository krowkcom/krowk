//! The god agent's steer of a gruntas-run session (GA06, D5), through the
//! daemon over its socket against the fake `claude`: an idle session takes
//! a message as its next prompt, framed with its origin; a running one as a
//! steer into its turn; an interrupt stops the turn, and one with no turn
//! is refused `nothing_running`.

#![cfg(unix)]

#[path = "common/fake.rs"]
mod fake;
#[path = "common/scratch.rs"]
mod scratch;

use gruntas_client::host_command::{refusal, HostAnswer};
use gruntas_harness::daemon::{self, client::Client, server};
use gruntas_harness::host::HostConfig;
use gruntas_harness::instances::{InstancesConfig, Registry};
use gruntas_harness::log;
use gruntas_harness::protocol::{Command, ModelRef, Origin, OriginKind, PermissionMode, RunResult, TurnStatus};
use gruntas_harness::sync::god::inbox::Steer;
use gruntas_harness::sync::god::run::deliver_on;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const GOD: &str = "01a0ec7b-0000-7000-8000-0000000000f1";

struct Home {
    root: PathBuf,
    instances: InstancesConfig,
}

impl Home {
    fn new(name: &str, scenario: Option<&str>) -> Home {
        let root = scratch::root(&format!("god-steer-{name}"));
        for d in ["home", "run", "repo/.git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let instances = InstancesConfig { instances: [("claude".to_string(), fake::claude(&root, "claude", scenario))].into_iter().collect(), ..Default::default() };
        Home { root, instances }
    }

    fn env(&self) -> impl Fn(&str) -> String + Clone + Send + 'static {
        let root = self.root.clone();
        move |k| match k {
            "HOME" => root.join("home").display().to_string(),
            "XDG_RUNTIME_DIR" => root.join("run").display().to_string(),
            "PATH" => std::env::var("PATH").unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn socket(&self) -> PathBuf {
        daemon::socket(&self.env()).unwrap()
    }

    /// The daemon on a thread of its own, exiting once idle.
    fn serve(&self) -> std::thread::JoinHandle<Result<(), String>> {
        let (env, socket, instances) = (self.env(), self.socket(), self.instances.clone());
        let credentials = self.root.join("home/.gruntas/credentials.json");
        let t = std::thread::spawn(move || {
            let factory: server::Factory = Box::new(move |cwd: &Path, answers: bool| {
                Ok(HostConfig {
                    sessions_dir: log::sessions_dir(&env).unwrap(),
                    cwd: cwd.to_path_buf(),
                    registry: Registry::resolve(&instances, &env),
                    gruntas_version: "test".into(),
                    pricer: Arc::new(|_, _, _| None),
                    catalog: Arc::new(|_, _| None),
                    credentials: credentials.clone(),
                    trust: gruntas_harness::trust::allow_all(),
                    publisher: None,
                    permissions: gruntas_harness::permissions::Config { approvals: answers, ..Default::default() },
                    agents: gruntas_harness::agents::AgentsConfig::none(),
                    session: Default::default(),
                })
            });
            server::run(server::Options { socket, idle: Some(Duration::from_millis(300)), gruntas_version: "test".into(), ..Default::default() }, factory)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(self.socket()).is_err() {
            assert!(Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(5));
        }
        t
    }

    async fn client(&self) -> Client {
        match Client::connect(&self.socket(), &self.root.join("repo"), "test", false).await {
            Ok(c) => c,
            Err(_) => panic!("no daemon answered"),
        }
    }

    /// The `user` lines the fake was written, as text.
    fn written(&self) -> Vec<String> {
        fake::prompts(&self.root, "claude").iter().map(|p| p["message"]["content"].to_string()).collect()
    }

    /// `written`, once it holds `n` lines.
    fn written_n(&self, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let w = self.written();
            if w.len() >= n || Instant::now() > deadline {
                return w;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn prompt(text: &str) -> Command {
    let model = ModelRef { instance: "claude".into(), model: "sonnet".into() };
    Command::Prompt { session_id: None, text: text.into(), images: Vec::new(), model: Some(model), permission_mode: PermissionMode::Default, toolset: None, effort: None, rollover: None, budget: None, attendance: None, fenced: false, origin: None }
}

fn origin() -> Origin {
    Origin { kind: OriginKind::Agent, session_id: Some(GOD.into()), device: Some("laptop".into()), attendance: None }
}

fn message(text: &str) -> Steer {
    Steer::Message { id: "m".into(), text: text.into(), origin: origin(), at: "2026-11-24T10:00:00Z".into() }
}

fn interrupt() -> Steer {
    Steer::Interrupt { id: "i".into(), origin: origin(), at: "2026-11-24T10:00:00Z".into() }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn code(a: &HostAnswer) -> &str {
    match a {
        HostAnswer::Refused { code, .. } => code,
        HostAnswer::Delivered { .. } => "delivered",
        _ => "other",
    }
}

/// An idle gruntas-run session takes a message as its next prompt, framed
/// with where it came from.
#[test]
fn ga06_an_idle_session_gets_a_message_as_a_prompt() {
    let home = Home::new("idle", None);
    let daemon = home.serve();
    rt().block_on(async {
        let c = home.client().await;
        let (tx, _rx) = mpsc::channel(256);
        let session = c.execute(prompt("first"), tx).await.unwrap().unwrap().session_id;
        let answer = deliver_on(&home.socket(), "test", &session, &message("rebase first")).await;
        assert_eq!(answer, HostAnswer::Delivered { session });
    });
    let written = home.written_n(2);
    assert_eq!(written.len(), 2, "{written:?}");
    assert!(written[1].contains("rebase first") && written[1].contains(&format!("steer from {GOD} on laptop")), "framed with its origin: {}", written[1]);
    daemon.join().unwrap().unwrap();
}

/// A running gruntas-run session takes a message as a steer into its turn,
/// and an interrupt stops that turn; with no turn, the interrupt is refused
/// `nothing_running`.
#[test]
fn ga06_a_running_session_gets_a_message_as_a_steer_and_an_interrupt_stops_it() {
    let interrupted = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","errors":[],"session_id":"{{SESSION}}"}"#;
    let home = Home::new("running", Some(&format!("@await-steer\n@await-interrupt\n{interrupted}")));
    let daemon = home.serve();
    let (result, after) = rt().block_on(async {
        let c = home.client().await;
        let (tx, mut rx) = mpsc::channel(256);
        let run = c.execute(prompt("work"), tx);
        tokio::pin!(run);
        let session = tokio::select! {
            Some(l) = rx.recv() => l.session_id().to_string(),
            _ = &mut run => panic!("ended early"),
        };
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        // Steered once Claude Code is at work on the prompt.
        let deadline = Instant::now() + Duration::from_secs(10);
        while home.written().is_empty() {
            assert!(Instant::now() < deadline, "the prompt never reached Claude Code");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let steered = deliver_on(&home.socket(), "test", &session, &message("also run the tests")).await;
        assert_eq!(steered, HostAnswer::Delivered { session: session.clone() });
        let deadline = Instant::now() + Duration::from_secs(10);
        while home.written().len() < 2 {
            assert!(Instant::now() < deadline, "the steer never reached Claude Code");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let stopped = deliver_on(&home.socket(), "test", &session, &interrupt()).await;
        assert_eq!(stopped, HostAnswer::Delivered { session: session.clone() });
        let result: RunResult = tokio::time::timeout(Duration::from_secs(10), run).await.expect("the turn ended").unwrap().unwrap();
        drain.abort();
        (result, deliver_on(&home.socket(), "test", &session, &interrupt()).await)
    });
    assert_eq!(result.status, TurnStatus::Interrupted);
    let written = home.written();
    assert!(written[1].contains("also run the tests") && written[1].contains(&format!("steer from {GOD} on laptop")), "the steer reached the turn, framed: {written:?}");
    assert_eq!(code(&after), refusal::NOTHING_RUNNING, "{after:?}");
    daemon.join().unwrap().unwrap();
}
