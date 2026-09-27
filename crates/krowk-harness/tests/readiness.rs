//! The readiness check against fake vendor binaries (no real login):
//! where a vendor is asked — a trusted repository's settings can make an
//! account ready, and nothing outside krowk's own directory can when no
//! repository is trusted — and how long it may take: one deadline per
//! check, a Codex fallback included, with the vendor and everything it
//! started stopped when it passes.

#![cfg(unix)]

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstanceKind, InstancesConfig, Registry};
use krowk_harness::protocol::{Command, ModelRef};
use krowk_harness::readiness::{self, Probe, Readiness};
use krowk_harness::{log, trust};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

struct Home {
    root: PathBuf,
}

impl Home {
    fn new(name: &str) -> Home {
        let root = std::env::temp_dir().join(format!("krowk-readiness-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "repo/.git", "bin", "cfg"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Home { root: root.canonicalize().unwrap() }
    }

    fn install(&self, name: &str, script: &str) -> PathBuf {
        let bin = self.root.join("bin").join(name);
        std::fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "PATH" => "/usr/bin:/bin".into(),
            _ => String::new(),
        }
    }

    fn claude(&self, bin: &Path) -> InstanceKind {
        InstanceKind::ClaudeCode { binary: Some(bin.display().to_string()), config_dir: Some(self.root.join("cfg").display().to_string()), env: BTreeMap::new(), args: Vec::new(), api_key_env: None, effort: None }
    }

    fn registry(&self, instances: Vec<(&str, InstanceKind)>) -> Registry {
        Registry::resolve(&InstancesConfig { instances: instances.into_iter().map(|(n, k)| (n.to_string(), k)).collect(), ..Default::default() }, &self.env())
    }

    fn host(&self, registry: Registry, gate: trust::Gate) -> Host {
        self.host_in(registry, gate, self.root.join("repo"))
    }

    /// A host whose sessions start in `cwd`.
    fn host_in(&self, registry: Registry, gate: trust::Gate, cwd: PathBuf) -> Host {
        Host::new(HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd,
            registry,
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.config/krowk/providers/credentials.json"),
            trust: gate,
            publisher: None,
            permissions: Default::default(),
            agents: krowk_harness::subagent::AgentsConfig::none(),
        })
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("fake.log")).unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A `claude` signed in only where its working directory's project
/// settings say so — as Claude Code is for a project that sets Bedrock,
/// Vertex or an `apiKeyHelper`.
fn project_claude(log: &Path) -> String {
    format!(
        "#!/bin/bash\necho \"argv $* pwd $PWD\" >>'{}'\nif [ \"$1 $2\" = 'auth status' ]; then\n  if [ -f \"$PWD/.claude/settings.json\" ]; then echo '{{\"loggedIn\":true,\"authMethod\":\"bedrock\"}}'; exit 0; fi\n  echo '{{\"loggedIn\":false,\"authMethod\":\"none\"}}'; exit 1\nfi\nexit 2\n",
        log.display()
    )
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 checks only whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn switch(host: &Host, model: &str) -> Result<(), krowk_harness::engine::EngineError> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let (i, m) = model.split_once('/').unwrap();
    rt.block_on(host.execute(Command::SwitchModel { session_id: None, model: ModelRef { instance: i.into(), model: m.into() } }, tx)).map(drop)
}

#[test]
fn readiness_a_trusted_repositorys_settings_count_and_an_untrusted_ones_are_never_read() {
    let h = Home::new("project");
    let bin = h.install("claude", &project_claude(&h.root.join("fake.log")));
    // The repository configures Claude Code for Bedrock in its settings.
    std::fs::create_dir_all(h.root.join("repo/.claude")).unwrap();
    std::fs::write(h.root.join("repo/.claude/settings.json"), r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#).unwrap();
    let reg = h.registry(vec![("claude:bedrock", h.claude(&bin))]);

    // Trusted: asked in the repository's root, where the turn will run, and
    // not refused.
    switch(&h.host(reg.clone(), trust::allow_all()), "claude:bedrock/sonnet").unwrap();
    assert!(h.log().contains(&format!("argv auth status --json pwd {}", h.root.join("repo").display())), "{}", h.log());

    // In a subdirectory of the repository, the vendor is asked where the
    // turn will start it — the session's own directory, whose settings
    // alone Claude Code reads, not its parents': its own Bedrock settings
    // count, and the root's do not.
    let sub = h.root.join("repo/sub");
    std::fs::create_dir_all(&sub).unwrap();
    let _ = std::fs::remove_file(h.root.join("fake.log"));
    let e = switch(&h.host_in(reg.clone(), trust::allow_all(), sub.clone()), "claude:bedrock/sonnet").unwrap_err();
    assert_eq!(e.code, "not_authenticated", "the root's settings do not reach a turn started in sub/: {e:?}");
    assert!(h.log().contains(&format!("argv auth status --json pwd {}", sub.display())), "{}", h.log());
    std::fs::create_dir_all(sub.join(".claude")).unwrap();
    std::fs::write(sub.join(".claude/settings.json"), r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#).unwrap();
    switch(&h.host_in(reg.clone(), trust::allow_all(), sub.clone()), "claude:bedrock/sonnet").unwrap();

    // Untrusted: refused for trust, and the vendor never started there.
    let _ = std::fs::remove_file(h.root.join("fake.log"));
    let gate: trust::Gate = Arc::new(|root: &Path| Err(trust::untrusted(root, "Pass --trust.")));
    assert_eq!(switch(&h.host(reg.clone(), gate), "claude:bedrock/sonnet").unwrap_err().code, "untrusted_directory");
    assert_eq!(h.log(), "", "nothing was spawned for an untrusted repository");

    // Outside any repository — `krowk status` — krowk's own directory: the
    // project's settings are not read, so the account is not signed in.
    let data = h.root.join("home/.local/share/krowk");
    let neutral = readiness::neutral_dir(&data).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&neutral).unwrap().permissions().mode() & 0o777, 0o700);
    let r = readiness::check(reg.get("claude:bedrock").unwrap(), &h.root.join("creds.json"), &Probe::at(&neutral));
    assert_eq!(r.readiness, Readiness::NotSignedIn);
    // A directory of krowk's that someone made world-writable is closed again;
    // a symlink in its place is refused.
    std::fs::set_permissions(&neutral, std::fs::Permissions::from_mode(0o777)).unwrap();
    readiness::neutral_dir(&data).unwrap();
    assert_eq!(std::fs::metadata(&neutral).unwrap().permissions().mode() & 0o777, 0o700);
    std::fs::remove_dir(&neutral).unwrap();
    std::os::unix::fs::symlink(h.root.join("repo"), &neutral).unwrap();
    assert!(readiness::neutral_dir(&data).unwrap_err().contains("not a directory"));
}

#[test]
fn readiness_a_vendor_that_outlives_the_deadline_is_unknown_and_stopped_with_what_it_started() {
    let h = Home::new("timeout");
    let pids = h.root.join("pids");
    // Answers nothing: sleeps, with a child of its own, past any deadline.
    let script = format!("#!/bin/bash\necho $$ >>'{p}'\nsleep 60 &\necho $! >>'{p}'\nwait\n", p = pids.display());
    let claude = h.install("claude", &script);
    let codex = h.install("codex", &script);
    let reg = h.registry(vec![
        ("claude:slow", h.claude(&claude)),
        ("codex:slow", InstanceKind::CodexAppServer { binary: Some(codex.display().to_string()), codex_home: Some(h.root.join("cfg").display().to_string()), env: BTreeMap::new(), args: Vec::new(), api_key_env: None, effort: None }),
    ]);
    let neutral = readiness::neutral_dir(&h.root.join("data")).unwrap();
    let probe = Probe { dir: neutral, within: Duration::from_millis(800) };
    for name in ["claude:slow", "codex:slow"] {
        let _ = std::fs::remove_file(&pids);
        let started = Instant::now();
        let r = readiness::check(reg.get(name).unwrap(), &h.root.join("creds.json"), &probe);
        let took = started.elapsed();
        assert!(matches!(&r.readiness, Readiness::Unknown { reason } if reason.contains("did not answer")), "{name}: {:?}", r.readiness);
        // Codex's structured check and its `login status` fallback share the
        // one deadline, so neither vendor takes longer than it.
        assert!(took < Duration::from_millis(2000), "{name} took {took:?} against a deadline of 800 ms");
        let started_pids: Vec<i32> = std::fs::read_to_string(&pids).unwrap().lines().map(|l| l.trim().parse().unwrap()).collect();
        assert!(started_pids.len() >= 2, "{name}: {started_pids:?}");
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            started_pids.iter().all(|p| !alive(*p))
        });
        assert!(gone, "{name}: the vendor or its child outlived the check: {started_pids:?}");
    }
}

#[test]
fn readiness_a_vendor_that_answers_and_leaves_a_process_behind_does_not_hold_the_check() {
    let h = Home::new("leftover");
    let pids = h.root.join("pids");
    // Answers at once and exits 0, leaving a sleeper that holds its stdout.
    let claude = h.install("claude", &format!("#!/bin/bash\nsleep 60 &\necho $! >>'{}'\necho '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}'\nexit 0\n", pids.display()));
    let reg = h.registry(vec![("claude:leaves", h.claude(&claude))]);
    let neutral = readiness::neutral_dir(&h.root.join("data")).unwrap();
    let started = Instant::now();
    let r = readiness::check(reg.get("claude:leaves").unwrap(), &h.root.join("creds.json"), &Probe { dir: neutral, within: Duration::from_secs(5) });
    let took = started.elapsed();
    assert!(r.readiness.is_ready(), "{:?}", r.readiness);
    assert!(took < Duration::from_secs(2), "the sleeper held the check for {took:?}");
    let sleeper: i32 = std::fs::read_to_string(&pids).unwrap().trim().parse().unwrap();
    let gone = (0..50).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        !alive(sleeper)
    });
    assert!(gone, "the sleeper ({sleeper}) outlived the check");
}

// Linux only: macOS has no setsid(1), and without it nothing escapes the
// group, so the test would pass without exercising the fix.
#[cfg(target_os = "linux")]
#[test]
fn readiness_a_leftover_that_escaped_the_group_cannot_hold_a_check_past_its_deadline() {
    let h = Home::new("setsid");
    let pids = h.root.join("pids");
    // Answers and exits 0, leaving a sleeper in a session of its own —
    // out of reach of the group kill — that holds its stdout.
    let claude = h.install("claude", &format!("#!/bin/bash\nsetsid sleep 60 &\necho $! >>'{}'\necho '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}'\nexit 0\n", pids.display()));
    let reg = h.registry(vec![("claude:escapes", h.claude(&claude))]);
    let neutral = readiness::neutral_dir(&h.root.join("data")).unwrap();
    let started = Instant::now();
    let r = readiness::check(reg.get("claude:escapes").unwrap(), &h.root.join("creds.json"), &Probe { dir: neutral, within: Duration::from_millis(800) });
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "the escaped sleeper held the check for {took:?}");
    // What had been read by the deadline is the answer.
    assert!(r.readiness.is_ready(), "{:?}", r.readiness);
    if let Ok(p) = std::fs::read_to_string(&pids) {
        for pid in p.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
            // SAFETY: the test's own sleeper, which the check could not reach.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}
