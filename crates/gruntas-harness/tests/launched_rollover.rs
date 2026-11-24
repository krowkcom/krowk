//! Rollover for launched sessions, RL02 (D4, R-RL-4): an unattended Claude
//! Code worker whose account runs out of usage, against a daemon in this
//! process and a fake `claude` that writes down what it was asked to stop
//! and resume, and under which account. This test process plays the
//! session's harness, sending what its plugin's hooks and mod would.
//! RL06 (R-RL-7) runs the ladder end to end here: one worker across two
//! Claude Code accounts and on to Codex, and a key rung stopping at a
//! small cap.

#![cfg(unix)]

use gruntas_harness::daemon::{server, tasks::Tasks};
use gruntas_harness::protocol::{Activity, PluginHarness, Registration, UsageWindow};
use gruntas_client::status::TaskState;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const ID: &str = "1234abcd";
const SESSION: &str = "1234abcd-5678-4000-8000-000000000000";

struct Rig {
    root: PathBuf,
    socket: PathBuf,
}

impl Rig {
    /// Two Claude Code accounts, `claude:a` (where the worker runs) and
    /// `claude:b`, on the fake `claude`, `rollover` as given; the worker
    /// launched on `a` and registered, its transcript `transcript`.
    fn new(name: &str, rollover: &str, transcript: &[&str]) -> Rig {
        Rig::with(name, rollover, transcript, &["claude:b"])
    }

    /// `new`, its other accounts `others` (`claude:<dir>` or `codex:<dir>`,
    /// each at home in `<dir>`). A continuation on the other harness is
    /// written down in `continued`.
    fn with(name: &str, rollover: &str, transcript: &[&str], others: &[&str]) -> Rig {
        // Under /tmp, so the socket's path fits `sun_path`.
        let root = PathBuf::from("/tmp").join(format!("gruntas-rl02-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a/projects/-work", "work", "run"].into_iter().chain(others.iter().map(|o| o.split_once(':').unwrap().1)) {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let out = root.display();
        let claude = root.join("claude");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = stop ]; then printf '%s %s\\n' \"$2\" \"$CLAUDE_CONFIG_DIR\" >> '{out}/stopped'; exit 0; fi\n{{ printf 'CLAUDE_CONFIG_DIR=%s\\n' \"$CLAUDE_CONFIG_DIR\"; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; }} > '{out}/resumed'\necho 'backgrounded · {ID}'\n"
        );
        std::fs::write(&claude, script).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("a/projects/-work").join(format!("{SESSION}.jsonl")), transcript.join("\n") + "\n").unwrap();
        let config = serde_json::json!({
            "instances": {
                "claude:a": { "kind": "claude-code", "binary": claude, "configDir": root.join("a") },
            },
            "rollover": rollover,
        });
        let mut config = config;
        for o in others {
            config["instances"][o] = match o.split_once(':').unwrap() {
                ("codex", d) => serde_json::json!({ "kind": "codex-app-server", "binary": claude, "codexHome": root.join(d) }),
                (_, d) => serde_json::json!({ "kind": "claude-code", "binary": claude, "configDir": root.join(d) }),
            };
        }
        // The default account, `claude`, shares a's login: never a move;
        // the default `codex` is out of usage too. a's home is known, so
        // the mod's report of it counts against that login.
        let login = serde_json::json!({ "login": "me", "seenAtMs": 1 });
        let a = serde_json::json!({ "login": "me", "seenAtMs": 1, "configHome": gruntas_harness::usage::home_key(&root.join("a").display().to_string()) });
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        let spent = serde_json::json!({ "five_hour": { "status": "limited", "usedPercent": 100.0, "resetsAtMs": now + 3_600_000, "seenAtMs": now } });
        std::fs::write(root.join("usage.json"), serde_json::json!({ "instances": { "claude": login, "claude:a": a, "codex": { "login": "spent", "seenAtMs": 1 } }, "logins": { "spent": spent } }).to_string()).unwrap();
        let cfg = gruntas_harness::instances::from_config_json(&config).unwrap();
        let registry = gruntas_harness::instances::Registry::resolve(&cfg, &|_| String::new());
        let run = root.join("run").display().to_string();
        let home = root.display().to_string();
        let env = move |k: &str| match k {
            "HOME" => home.clone(),
            "XDG_RUNTIME_DIR" => run.clone(),
            _ => String::new(),
        };
        let socket = gruntas_harness::daemon::socket(&env).unwrap();
        let continued = root.join("continued");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let opts = server::Options {
            socket: socket.clone(),
            idle: None,
            gruntas_version: "test".into(),
            usage: Some(gruntas_harness::usage::Usage::at(root.join("usage.json"))),
            accounts: Some(std::sync::Arc::new(move || Ok(registry.clone()))),
            cross: Some(std::sync::Arc::new(move |step: &gruntas_harness::ladder::Step, from: &str, cwd: Option<&std::path::Path>| {
                std::fs::write(&continued, format!("{} {} {from} {}", step.to.instance, step.to.model, cwd.map(|d| d.display().to_string()).unwrap_or_default())).unwrap();
                Ok("019a-codex-thread".to_string())
            })),
            ..Default::default()
        };
        std::thread::spawn(move || {
            let factory: server::Factory = Box::new(|_, _| Err(gruntas_harness::engine::EngineError::new("no_sessions", "this test runs none")));
            server::run(opts, factory)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(5));
        }
        let rig = Rig { root, socket };
        rig.send(Activity::Launched { id: ID.into(), program: claude.display().to_string(), config_home: Some(rig.home("a")), args: vec!["--bg".into(), "--plugin-dir".into(), "/p".into(), "--model".into(), "sonnet".into()] });
        rig.register("a");
        rig
    }

    fn home(&self, account: &str) -> String {
        self.root.join(account).display().to_string()
    }

    fn send(&self, a: Activity) {
        gruntas_harness::hook::send(&self.socket, "/r", "test", a).unwrap();
    }

    /// The session's SessionStart, on `account`.
    fn register(&self, account: &str) {
        let transcript = self.root.join(account).join("projects/-work").join(format!("{SESSION}.jsonl"));
        self.send(Activity::Register(Registration { session_id: SESSION.into(), harness: PluginHarness::Claude, harness_pid: Some(std::process::id()), config_home: Some(self.home(account)), cwd: Some(self.root.join("work").display().to_string()), repository: None, transcript_path: Some(transcript.display().to_string()), attendance: None }));
    }

    /// The mod's report: `account` used up.
    fn limited(&self, account: &str) {
        self.send(Activity::Usage { config_home: self.home(account), windows: vec![UsageWindow { kind: "fiveHour".into(), used_percent: 100.0, resets_at_ms: None }] });
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.root.join(file)).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn tasks(&self) -> Tasks {
        Tasks::read(self.socket.parent().unwrap())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"fix the bug"}}"#;
const TOOL: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
const LIMIT: &str = r#"{"type":"assistant","isApiErrorMessage":true,"error":"rate_limit","message":{"role":"assistant","content":[{"type":"text","text":"You've hit your limit"}]}}"#;
const ANSWER: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done."}]}}"#;

fn resumed(rig: &Rig) -> Vec<String> {
    rig.read("resumed").lines().map(String::from).collect()
}

/// With `auto`: stopped on `a`, its transcript copied into `b`, resumed
/// there in the background, the prompt the limit interrupted sent again;
/// and, done on `b`, stopped there.
#[test]
fn rl02_with_auto_the_worker_moves_and_the_interrupted_prompt_is_sent_again() {
    let rig = Rig::new("auto", "auto", &[PROMPT, TOOL, LIMIT]);
    rig.limited("a");
    rig.wait_for("the worker was never resumed", || rig.root.join("resumed").exists());
    assert_eq!(rig.read("stopped"), format!("{ID} {}\n", rig.home("a")), "stopped on its old account");
    assert_eq!(resumed(&rig), [format!("CLAUDE_CONFIG_DIR={}", rig.home("b")), "--bg".into(), "--plugin-dir".into(), "/p".into(), "--model".into(), "sonnet".into(), "--resume".into(), SESSION.into(), "fix the bug".into()]);
    let copied = rig.root.join("b/projects/-work").join(format!("{SESSION}.jsonl"));
    assert_eq!(std::fs::read_to_string(copied).unwrap(), [PROMPT, TOOL, LIMIT].join("\n") + "\n", "its transcript, under b");

    // It runs on b now, whatever the old session's late SessionEnd says:
    // done there, it is stopped there.
    rig.register("b");
    rig.send(Activity::Deregister { session_id: SESSION.into() });
    let report = gruntas_client::status::Report { state: Some(TaskState::Completed), timestamp: "2026-11-24T00:00:00Z".into(), ..Default::default() };
    rig.send(Activity::Report { session_id: SESSION.into(), summary: "fixed".into(), report });
    rig.send(Activity::TurnEnd { session_id: SESSION.into() });
    rig.wait_for("the moved worker, done, was never stopped", || rig.read("stopped").lines().count() == 2);
    assert_eq!(rig.read("stopped").lines().nth(1), Some(format!("{ID} {}", rig.home("b")).as_str()));
}

/// A turn that had ended before the limit was seen interrupted nothing: it
/// is resumed with no prompt.
#[test]
fn rl02_an_ended_turn_is_resumed_with_no_prompt() {
    let rig = Rig::new("ended", "auto", &[PROMPT, ANSWER]);
    rig.send(Activity::TurnEnd { session_id: SESSION.into() });
    rig.limited("a");
    rig.wait_for("the worker was never resumed", || rig.root.join("resumed").exists());
    assert_eq!(resumed(&rig).last().map(String::as_str), Some(SESSION), "{:?}", resumed(&rig));
}

/// Without `auto` (never acknowledged): nothing stopped or started; the
/// Task waits on the person, naming the next account.
#[test]
fn rl02_without_auto_the_worker_waits_naming_the_next_account() {
    let rig = Rig::new("offer", "offer", &[PROMPT, LIMIT]);
    rig.limited("a");
    rig.wait_for("the Task never waited on the person", || rig.tasks().get(SESSION).is_some_and(|t| t.status.state == TaskState::InputRequired));
    let t = rig.tasks().get(SESSION).cloned().unwrap();
    let why = t.status.message.map(|m| m.summary()).unwrap_or_default();
    assert!(why.contains("the next account is claude:b") && why.contains("rollover auto"), "{why}");
    assert!(!rig.root.join("stopped").exists() && !rig.root.join("resumed").exists(), "nothing moved");
}

/// The ladder's next account on the other harness (only `codex:b` there):
/// with `auto`, the worker is stopped and continued there from a handoff,
/// at the equivalent tier's model, as the launcher's `continue_on` does.
#[test]
fn rl02_on_the_other_harness_rung_the_worker_is_stopped_and_continued_there() {
    let rig = Rig::with("cross", "auto", &[PROMPT, LIMIT], &["codex:b"]);
    rig.limited("a");
    rig.wait_for("the worker was never continued", || rig.root.join("continued").exists());
    assert_eq!(rig.read("continued"), format!("codex:b gpt-5.6-terra {SESSION} {}", rig.root.join("work").display()), "sonnet's tier on Codex, in its directory");
    assert_eq!(rig.read("stopped"), format!("{ID} {}\n", rig.home("a")), "stopped on its old account");
    assert!(!rig.root.join("resumed").exists(), "not resumed on Claude Code");
}

/// RL06 (R-RL-7), the ladder end to end for one unattended worker: out of
/// usage on `claude:a`, it moves to `claude:b` (another subscription, the
/// same session resumed there); out of usage on `b` too, with `a` still
/// spent, it is stopped there and continued on `codex:c`, the other
/// harness, at sonnet's tier, in its directory.
#[test]
fn rl06_a_worker_crosses_claude_code_accounts_then_to_codex() {
    let rig = Rig::with("ladder", "auto", &[PROMPT, TOOL, LIMIT], &["claude:b", "codex:c"]);
    rig.limited("a");
    rig.wait_for("the worker never moved to b", || rig.root.join("resumed").exists());
    assert_eq!(resumed(&rig)[0], format!("CLAUDE_CONFIG_DIR={}", rig.home("b")), "the same harness's rung first");
    assert!(!rig.root.join("continued").exists(), "not on Codex while b has room");

    // On b now (the move done): its SessionStart there, then b runs out
    // as well.
    rig.wait_for("the move never finished", || rig.tasks().launched().iter().any(|w| w.launch.config_home == Some(rig.home("b"))));
    rig.register("b");
    rig.limited("b");
    rig.wait_for("the worker never continued on Codex", || rig.root.join("continued").exists());
    assert_eq!(rig.read("continued"), format!("codex:c gpt-5.6-terra {SESSION} {}", rig.root.join("work").display()));
    assert_eq!(rig.read("stopped"), format!("{ID} {}\n{ID} {}\n", rig.home("a"), rig.home("b")), "stopped on a, then on b");
}

/// RL06 (R-RL-7), a key rung stopping at a small cap: every subscription
/// spent (in the usage records), the ladder offers the API-key account,
/// its notice naming account, rung and the $0.50 cap; spend readings fed
/// as the mod's session cost would be (none reaches the daemon yet), the
/// session stops at the cap with its Task input-required and the reason,
/// and the key rung is closed to it.
#[test]
fn rl06_a_key_rung_stops_at_a_small_cap() {
    use gruntas_harness::ladder::{keys_open, notice, KeySpend, Rung};
    use gruntas_harness::launch::Harness;
    use gruntas_harness::protocol::ModelRef;
    let config = serde_json::json!({
        "instances": {
            "claude:b": { "kind": "claude-code", "configDir": "/h/b" },
            "claude:key": { "kind": "claude-code", "configDir": "/h/k", "apiKeyEnv": "ANTHROPIC_API_KEY" },
        },
        "ladder": { "keyCapUsd": 0.5 },
    });
    let registry = gruntas_harness::instances::Registry::resolve(&gruntas_harness::instances::from_config_json(&config).unwrap(), &|k| if k == "HOME" { "/h".into() } else { String::new() });
    let spent = serde_json::json!({ "five_hour": { "status": "limited", "usedPercent": 100.0, "seenAtMs": 1_000 } });
    let rec = |login: &str| serde_json::json!({ "login": login, "seenAtMs": 1_000 });
    let data: gruntas_harness::usage::Data = serde_json::from_value(serde_json::json!({
        "instances": { "claude": rec("me"), "claude:b": rec("b"), "codex": rec("cx") },
        "logins": { "me": spent, "b": spent, "cx": spent },
    }))
    .unwrap();
    let from = ModelRef { instance: "claude".into(), model: "sonnet".into() };
    // $3.20 spent on subscriptions before the move: the cap counts from it.
    let reading = 3.2;
    assert_eq!(registry.ladder.next(&registry, &data, &from, keys_open(Harness::Claude, false, None, None), 2_000), None, "no spend reading: no key rung");
    let step = registry.ladder.next(&registry, &data, &from, keys_open(Harness::Claude, false, Some(reading), None), 2_000).expect("the key rung");
    assert_eq!((step.rung, step.to.instance.as_str()), (Rung::ApiKey, "claude:key"));
    assert_eq!(notice(&step, registry.ladder.key_cap_usd), format!("Continuing on claude:key ({}): an API-key account, billed per use, up to $0.50 this session.", step.to.model));

    let on_key = KeySpend::enter(&step, reading, registry.ladder.key_cap_usd);
    let mut tasks = Tasks::default();
    tasks.apply(&Activity::Register(Registration { session_id: SESSION.into(), harness: PluginHarness::Claude, harness_pid: None, config_home: Some("/h/k".into()), cwd: None, repository: None, transcript_path: None, attendance: None }), "2026-11-24T00:00:00Z");
    for r in [3.2, 3.45, 3.69] {
        assert_eq!(on_key.capped(r), None, "under the cap at ${r}");
        assert!(keys_open(Harness::Claude, false, Some(r), Some(&on_key)));
    }
    let reason = on_key.capped(3.7).expect("at the cap");
    assert!(reason.contains("$0.50 spent on claude:key") && reason.contains("cap $0.50"), "{reason}");
    assert!(tasks.capped(SESSION, &reason, "2026-11-24T00:01:00Z"));
    let t = tasks.get(SESSION).unwrap();
    assert_eq!(t.status.state, TaskState::InputRequired);
    assert_eq!(t.status.message.as_ref().map(|m| m.summary()), Some(reason));
    assert!(!keys_open(Harness::Claude, false, Some(3.7), Some(&on_key)), "the key rung closed to it");
}
