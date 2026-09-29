//! R-PERM-3, second review: escapes from the OS sandbox, tried for real.
//!
//! Every test asserts an escape FAILS. Each attempt prints
//! `ATTEMPT <name>: BLOCKED|ESCAPED` so a run with `--no-capture` is the
//! review's table, and a test fails listing every attempt that escaped.
//!
//! The fixtures live under `CARGO_TARGET_TMPDIR`, never under `/tmp`: the
//! sandbox mounts a private tmpfs on `/tmp`, so a fake home under it is
//! invisible for that reason alone and a "hidden" check there proves
//! nothing. Each fixture has a positive control showing the planted file is
//! reachable where the sandbox does not hide it.
//!
//! Nothing here reads the real `~/.ssh`, the real credentials or the real
//! home: the planted secrets hold `FAKE-KEY-MARKER` / `fake-*` values.
#![cfg(target_os = "linux")]

use krowk_harness::permissions::{self, Access, Call, Gate, Policy, Verdict};
use krowk_harness::protocol::PermissionMode;
use krowk_harness::sandbox::{self, By, Profile, Sandbox};
use krowk_harness::toolset::EditTool;
use krowk_harness::tools::{self, Scope, ToolEnv, BASH, GREP, READ, STR_REPLACE, WRITE};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MARKER: &str = "FAKE-KEY-MARKER";

fn enforced(test: &str) -> bool {
    match sandbox::enforcer() {
        Ok(_) => true,
        Err(why) => {
            assert!(std::env::var_os("CI").is_none(), "{test}: CI must run the escape tests: {why}");
            eprintln!("{test} skipped: {why}");
            false
        }
    }
}

/// The attempts of one test: each printed, and the escaped ones failing it.
#[derive(Default)]
struct Tally(Vec<(String, bool, String)>);

impl Tally {
    fn blocked(&mut self, name: &str, blocked: bool, detail: impl Into<String>) {
        let detail = detail.into();
        eprintln!("ATTEMPT {name}: {}{}", if blocked { "BLOCKED" } else { "ESCAPED" }, if blocked { String::new() } else { format!(" — {}", detail.replace('\n', " | ")) });
        self.0.push((name.into(), blocked, detail));
    }
    /// A positive control: the fixture itself works, or the test is vacuous.
    fn control(&self, what: &str, ok: bool, detail: &str) {
        assert!(ok, "control failed ({what}), so the attempts prove nothing: {detail}");
    }
    fn done(self) {
        let escaped: Vec<_> = self.0.iter().filter(|(_, b, _)| !b).map(|(n, _, d)| format!("{n}: {d}")).collect();
        assert!(escaped.is_empty(), "escapes worked:\n{}", escaped.join("\n"));
    }
}

/// A scratch base outside `/tmp`, a workspace in it that is a git
/// repository, and a fake home with planted secrets.
struct Fx {
    base: PathBuf,
    ws: PathBuf,
    home: PathBuf,
    scope: Scope,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn base(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("r192b-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn git(ws: &Path, home: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(ws)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

fn plant(home: &Path) {
    let w = |rel: &str, body: &str| {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    w(".ssh/id_ed25519", &format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{MARKER}\n-----END OPENSSH PRIVATE KEY-----\n"));
    w(".cargo/credentials.toml", &format!("[registry]\ntoken = \"{MARKER}-cargo\"\n"));
    w(".gitconfig", "[user]\n\tname = fake\n");
    // Not on the sandbox's credential list: what else a home holds.
    w(".git-credentials", &format!("https://fake:{MARKER}-gitcreds@github.com\n"));
    w(".netrc", &format!("machine api.github.com login fake password {MARKER}-netrc\n"));
    w(".claude/.credentials.json", &format!("{{\"claudeAiOauth\":{{\"accessToken\":\"{MARKER}-claude\"}}}}\n"));
    w(".codex/auth.json", &format!("{{\"OPENAI_API_KEY\":\"{MARKER}-codex\"}}\n"));
    w(".npmrc", &format!("//registry.npmjs.org/:_authToken={MARKER}-npm\n"));
    w(".config/gh/hosts.yml", &format!("github.com:\n  oauth_token: {MARKER}-gh\n"));
    w(".local/share/keyrings/login.keyring", &format!("{MARKER}-keyring\n"));
    w(".bash_history", &format!("export GITHUB_TOKEN={MARKER}-history\n"));
    w("control/readable.txt", &format!("{MARKER}-control\n"));
}

/// The fixture, its scope built the way a turn's is: settings loaded,
/// the gate in `mode`, and the verdict's openings as wide as they go — a
/// person approving every fence and path, or bypass.
fn fx(name: &str, profile: Profile, by: By, mode: PermissionMode) -> Fx {
    let base = base(name);
    let (ws, home) = (base.join("ws"), base.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    plant(&home);
    git(&ws, &home, &["init", "-q"]);
    std::fs::create_dir_all(ws.join(".claude")).unwrap();
    std::fs::write(ws.join(".claude/settings.json"), "{}\n").unwrap();
    std::fs::write(ws.join("a.txt"), "hello\n").unwrap();
    let cfg = permissions::Config { home: Some(home.clone()), claude_dir: Some(home.join(".claude")), krowk_dir: Some(home.join(".krowk")), sandbox: Some(Sandbox { profile, by }), ..Default::default() };
    let policy = Policy::load(&cfg, &ws).unwrap();
    let gate = Gate::new(policy, mode, Arc::new(Mutex::new(Vec::new())), None, None, "r192b", "t");
    let opens = match gate.verdict(&Call { tool: "Write".into(), access: Access::Edit(vec![base.join("x")]), subject: None }, None) {
        Verdict::Allow(o) => o,
        _ => permissions::Opens { outside: true, fences: true },
    };
    let opens = permissions::Opens { outside: true, fences: true }.max_with(opens);
    let scope = gate.scope(opens);
    assert!(scope.sandbox.is_some(), "the scope carries the sandbox");
    Fx { base, ws, home, scope }
}

trait MaxWith {
    fn max_with(self, o: Self) -> Self;
}
impl MaxWith for permissions::Opens {
    fn max_with(self, o: Self) -> Self {
        permissions::Opens { outside: self.outside || o.outside, fences: self.fences || o.fences }
    }
}

fn bw(name: &str, profile: Profile) -> Fx {
    fx(name, profile, By::Bubblewrap, PermissionMode::BypassPermissions)
}

async fn run(f: &Fx, name: &str, input: Value) -> (String, bool) {
    let env = ToolEnv { cwd: &f.ws, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
    tools::execute(name, &input, &env, f.scope.clone()).await
}

async fn sh(f: &Fx, cmd: &str) -> (String, bool) {
    run(f, BASH, json!({ "command": cmd })).await
}

fn q(p: &Path) -> String {
    format!("'{}'", p.display())
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

// ---------------------------------------------------------------- 1

#[tokio::test(flavor = "current_thread")]
async fn r192b_1_writes_outside_the_workspace_fail() {
    if !enforced("r192b_1") {
        return;
    }
    let f = bw("w1", Profile::Workspace);
    let mut t = Tally::default();
    let (out, err) = sh(&f, "echo in > inside.txt && cat inside.txt").await;
    t.control("the workspace is writable", !err && out.starts_with("in"), &out);
    std::fs::write(f.base.join("victim.txt"), "original\n").unwrap();

    let (out, _) = sh(&f, "echo x > ../dotdot.txt").await;
    t.blocked("1a write via ..", !f.base.join("dotdot.txt").exists(), out);
    let (out, _) = sh(&f, &format!("echo x > {}", q(&f.base.join("abs.txt")))).await;
    t.blocked("1b write via absolute path", !f.base.join("abs.txt").exists(), out);
    let (out, _) = sh(&f, "ln -s .. up && echo x > up/via-symlink.txt").await;
    t.blocked("1c write via symlink in workspace", !f.base.join("via-symlink.txt").exists(), out);
    let (out, _) = sh(&f, &format!("ln {} hl && echo pwned >> hl", q(&f.base.join("victim.txt")))).await;
    t.blocked("1d write via hardlink", read(&f.base.join("victim.txt")) == "original\n", out);
    let (out, _) = sh(&f, &format!("echo x > /proc/self/root{}", f.base.join("procroot.txt").display())).await;
    t.blocked("1e write via /proc/self/root", !f.base.join("procroot.txt").exists(), out);
    let (out, _) = sh(&f, "echo x > /proc/1/cwd/../proc1cwd.txt; echo x > /proc/1/root/proc1root.txt").await;
    t.blocked("1f write via /proc/1/cwd and /proc/1/root", !f.base.join("proc1cwd.txt").exists() && !Path::new("/proc1root.txt").exists(), out);
    let shm = format!("/dev/shm/r192b-{}", std::process::id());
    let (out, _) = sh(&f, &format!("echo x > {shm}")).await;
    let seen = Path::new(&shm).exists();
    let _ = std::fs::remove_file(&shm);
    t.blocked("1g write to /dev/shm reaches the host", !seen, out);
    let tmp = format!("/tmp/r192b-{}", std::process::id());
    let (out, _) = sh(&f, &format!("echo x > {tmp}")).await;
    let seen = Path::new(&tmp).exists();
    let _ = std::fs::remove_file(&tmp);
    t.blocked("1h write to /tmp reaches the host", !seen, out);
    t.done();
}

// ---------------------------------------------------------------- 2

#[tokio::test(flavor = "current_thread")]
async fn r192b_2_protected_dirs_stay_read_only() {
    if !enforced("r192b_2") {
        return;
    }
    let f = bw("w2", Profile::Workspace);
    let mut t = Tally::default();
    let config = read(&f.ws.join(".git/config"));
    let global = read(&f.home.join(".gitconfig"));

    let (out, _) = sh(&f, "echo 'echo pwned' > .git/hooks/pre-commit").await;
    t.blocked("2a write .git/hooks/pre-commit", !f.ws.join(".git/hooks/pre-commit").exists(), out);

    let (out, _) = sh(&f, "mkdir -p myhooks && echo 'echo pwned' > myhooks/pre-commit && chmod +x myhooks/pre-commit; git -c core.hooksPath=$PWD/myhooks config core.hooksPath $PWD/myhooks; git config --global core.hooksPath $PWD/myhooks; git config --get core.hooksPath").await;
    t.blocked("2b persist core.hooksPath (repo + host global config unchanged)", read(&f.ws.join(".git/config")) == config && read(&f.home.join(".gitconfig")) == global, out);

    let (out, _) = sh(&f, "rm -rf .git 2>&1; echo 'gitdir: /tmp/evil' > .git").await;
    t.blocked("2c replace .git with a gitdir file", f.ws.join(".git").is_dir() && f.ws.join(".git/HEAD").exists(), out);

    let (out, _) = sh(&f, "mv .git .git-old && mkdir -p .git/hooks && echo pwned > .git/hooks/pre-commit").await;
    t.blocked("2d rename .git away and recreate", f.ws.join(".git/HEAD").exists() && !f.ws.join(".git-old").exists() && !f.ws.join(".git/hooks/pre-commit").exists(), out);

    let (out, _) = sh(&f, "echo '{\"hooks\":{\"x\":1}}' > .claude/settings.json").await;
    t.blocked("2e write .claude/settings.json", read(&f.ws.join(".claude/settings.json")) == "{}\n", out);

    let (out, _) = sh(&f, "printf '[core]\\n\\tfsmonitor = echo pwned\\n' >> .git/config").await;
    t.blocked("2f write .git/config", read(&f.ws.join(".git/config")) == config, out);

    // A repository that already names a hooks directory inside the tree
    // (husky, lefthook, `.githooks`): git on the host runs what is there.
    git(&f.ws, &f.home, &["config", "core.hooksPath", ".githooks"]);
    std::fs::create_dir_all(f.ws.join(".githooks")).unwrap();
    let (out, _) = sh(&f, "echo 'echo pwned' > .githooks/pre-commit && chmod +x .githooks/pre-commit").await;
    t.blocked("2g write the repo's configured core.hooksPath (.githooks)", !f.ws.join(".githooks/pre-commit").exists(), out);
    let (out, _) = run(&f, WRITE, json!({"path": ".githooks/post-checkout", "content": "echo pwned"})).await;
    t.blocked("2g' file tool writes the configured core.hooksPath", !f.ws.join(".githooks/post-checkout").exists(), out);

    // A repository nested in the workspace: its own .git is not fenced.
    let sub = f.ws.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &f.home, &["init", "-q"]);
    let (out, _) = sh(&f, "echo 'echo pwned' > sub/.git/hooks/pre-commit; printf '[core]\\n\\tfsmonitor = echo pwned\\n' >> sub/.git/config").await;
    t.blocked("2h write a nested repo's .git/hooks and .git/config", !sub.join(".git/hooks/pre-commit").exists() && !read(&sub.join(".git/config")).contains("fsmonitor"), out);
    let (out, _) = run(&f, WRITE, json!({"path": "sub/.git/hooks/pre-push", "content": "echo pwned"})).await;
    t.blocked("2h' file tool writes a nested repo's .git/hooks", !sub.join(".git/hooks/pre-push").exists(), out);
    t.done();
}

// ---------------------------------------------------------------- 3

#[tokio::test(flavor = "current_thread")]
async fn r192b_3_planted_secrets_stay_unreadable() {
    if !enforced("r192b_3") {
        return;
    }
    // The default layout: no CARGO_HOME or RUSTUP_HOME named, so the fake
    // home's `.cargo` is cargo's home and its credentials are hidden.
    // SAFETY: the test's own process (nextest), before the plan reads them.
    unsafe {
        std::env::remove_var("CARGO_HOME");
        std::env::remove_var("RUSTUP_HOME");
    }
    let f = bw("w3", Profile::Workspace);
    let mut t = Tally::default();
    let key = f.home.join(".ssh/id_ed25519");
    // Amended (#192 round 3, the coordinator's decision): the home binds
    // back nothing, so the positive control is a fixture outside it, and
    // the home's own non-dot file is an attempt that must be blocked.
    std::fs::create_dir_all(f.base.join("control")).unwrap();
    std::fs::write(f.base.join("control/readable.txt"), format!("{MARKER}-control\n")).unwrap();
    let (out, _) = sh(&f, &format!("cat {}", q(&f.base.join("control/readable.txt")))).await;
    t.control("an unhidden file outside the home is readable in the sandbox", out.contains(MARKER), &out);
    let (out, _) = sh(&f, &format!("cat {}", q(&f.home.join("control/readable.txt")))).await;
    t.blocked("3j cat a non-dot file in the home (~/control)", !out.contains(MARKER), out);

    let (out, _) = sh(&f, &format!("cat {}", q(&key))).await;
    t.blocked("3a cat ~/.ssh key", !out.contains(MARKER), out);
    let (out, _) = sh(&f, &format!("cat /proc/self/root{k} /proc/1/root{k} 2>&1; for p in /proc/[0-9]*; do cat $p/root{k} 2>/dev/null; done", k = key.display())).await;
    t.blocked("3b read key via /proc/*/root", !out.contains(MARKER), out);
    let (out, _) = sh(&f, &format!("ln -sf {} k && cat k", q(&key))).await;
    t.blocked("3c read key via symlink in workspace", !out.contains(MARKER), out);
    let (out, _) = run(&f, READ, json!({"path": key})).await;
    t.blocked("3d read tool on key", !out.contains(MARKER), out);
    let (out, _) = run(&f, READ, json!({"path": "k"})).await;
    t.blocked("3d' read tool on workspace symlink to key", !out.contains(MARKER), out);
    let (out, _) = run(&f, GREP, json!({"pattern": "FAKE", "path": f.home.join(".ssh")})).await;
    t.blocked("3d'' grep tool over ~/.ssh", !out.contains(MARKER), out);
    let _ = std::fs::remove_file(f.ws.join("k"));

    // The race: a regular file when the tool checks, a link to the key when
    // it opens — what a concurrent sandboxed command (a parallel subagent's
    // bash) could do to the workspace.
    let won = race_read(&f, &key, READ).await;
    t.blocked("3e read tool, symlink swapped during the call", won.is_none(), won.unwrap_or_default());
    let won = race_read(&f, &key, GREP).await;
    t.blocked("3e' grep tool, symlink swapped during the call", won.is_none(), won.unwrap_or_default());

    let (out, _) = sh(&f, &format!("cat {}", q(&f.home.join(".cargo/credentials.toml")))).await;
    t.blocked("3f cat ~/.cargo/credentials.toml", !out.contains(MARKER), out);
    let (out, _) = run(&f, READ, json!({"path": f.home.join(".cargo/credentials.toml")})).await;
    t.blocked("3f' read tool on ~/.cargo/credentials.toml", !out.contains(MARKER), out);

    // Credentials a home holds outside the sandbox's list, under the
    // workspace profile — which keeps the network.
    for rel in [".git-credentials", ".netrc", ".claude/.credentials.json", ".codex/auth.json", ".npmrc", ".config/gh/hosts.yml", ".local/share/keyrings/login.keyring", ".bash_history"] {
        let (out, _) = sh(&f, &format!("cat {}", q(&f.home.join(rel)))).await;
        t.blocked(&format!("3g cat ~/{rel} (workspace profile)"), !out.contains(MARKER), out);
    }
    t.done();
}

/// Races `tool` against a host thread flipping `ws/r` between a regular
/// file and a symlink to `target`; the output that leaked, if any did.
async fn race_read(f: &Fx, target: &Path, tool: &str) -> Option<String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ws, target) = (f.ws.clone(), target.to_path_buf());
    let flipper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let (r, l, g) = (ws.join("r"), ws.join(".r.link"), ws.join(".r.reg"));
            while !stop.load(Ordering::Relaxed) {
                let _ = std::fs::write(&g, "FAKE nothing here\n");
                let _ = std::fs::rename(&g, &r);
                let _ = std::os::unix::fs::symlink(&target, &l);
                let _ = std::fs::rename(&l, &r);
            }
        })
    };
    let mut leaked = None;
    for _ in 0..10000 {
        let input = if tool == READ { json!({"path": "r"}) } else { json!({"pattern": "FAKE", "glob": "r"}) };
        let (out, _) = run(f, tool, input).await;
        if out.contains(MARKER) {
            leaked = Some(out);
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    let _ = std::fs::remove_file(f.ws.join("r"));
    leaked
}

#[tokio::test(flavor = "current_thread")]
async fn r192b_3_write_tool_symlink_race_stays_in_the_workspace() {
    if !enforced("r192b_3w") {
        return;
    }
    let f = bw("w3w", Profile::Workspace);
    let mut t = Tally::default();
    let (hook, outside) = (f.ws.join(".git/hooks/pre-commit"), f.base.join("outside.txt"));
    for (name, target) in [("3h write tool, swap to .git/hooks/pre-commit", &hook), ("3h' write tool, swap to a file outside the workspace", &outside)] {
        let stop = Arc::new(AtomicBool::new(false));
        let (ws, tgt) = (f.ws.clone(), target.clone());
        let flipper = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (w, l, g) = (ws.join("w"), ws.join(".w.link"), ws.join(".w.reg"));
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::write(&g, "plain\n");
                    let _ = std::fs::rename(&g, &w);
                    let _ = std::os::unix::fs::symlink(&tgt, &l);
                    let _ = std::fs::rename(&l, &w);
                }
            })
        };
        for _ in 0..3000 {
            let _ = run(&f, WRITE, json!({"path": "w", "content": "echo pwned\n"})).await;
            if target.exists() {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        let landed = target.exists();
        let _ = std::fs::remove_file(target);
        t.blocked(name, !landed, format!("{} was written from the file tool", target.display()));
    }
    t.done();
}

#[tokio::test(flavor = "current_thread")]
async fn r192b_3_provider_env_does_not_reach_the_sandbox() {
    if !enforced("r192b_3env") {
        return;
    }
    // Set on this process — its own under nextest — as krowk's would be.
    let planted = [("ANTHROPIC_API_KEY", "fake-anthropic"), ("GITHUB_TOKEN", "fake-github"), ("SSH_AUTH_SOCK", "/run/user/1000/fake-agent.sock")];
    for (k, v) in planted {
        // SAFETY: the test's own process, before any thread reads the environment for it.
        unsafe { std::env::set_var(k, v) };
    }
    let f = bw("w3env", Profile::Workspace);
    let mut t = Tally::default();
    let (out, _) = sh(&f, "env; echo ---; tr '\\0' '\\n' < /proc/1/environ; echo ---; for p in /proc/[0-9]*; do tr '\\0' '\\n' < $p/environ 2>/dev/null; done").await;
    t.control("env was dumped", out.contains("PATH="), &out);
    for (k, v) in planted {
        t.blocked(&format!("3i {k} via env and /proc/*/environ"), !out.contains(v) && !out.contains(&format!("{k}=")), out.clone());
    }
    t.done();
}

/// With `CARGO_HOME` named elsewhere, a `~/.cargo/credentials.toml` left in
/// the home is not cargo's any more and is not hidden: a note.
#[tokio::test(flavor = "current_thread")]
async fn r192b_3_note_cargo_home_elsewhere_leaves_home_cargo_credentials() {
    if !enforced("r192b_3cargo") {
        return;
    }
    let b = base("w3cargo-home");
    std::fs::create_dir_all(b.join("elsewhere")).unwrap();
    // SAFETY: the test's own process (nextest), before the plan reads it.
    unsafe { std::env::set_var("CARGO_HOME", b.join("elsewhere")) };
    let f = bw("w3cargo", Profile::Workspace);
    let (out, _) = sh(&f, &format!("cat {}", q(&f.home.join(".cargo/credentials.toml")))).await;
    eprintln!("NOTE 3f'' ~/.cargo/credentials.toml readable when CARGO_HOME is elsewhere: {}", out.contains(MARKER));
    let _ = std::fs::remove_dir_all(&b);
}

// ---------------------------------------------------------------- 4

#[tokio::test(flavor = "current_thread")]
async fn r192b_4_strict_and_read_only_have_no_network() {
    if !enforced("r192b_4") {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let uport = udp.local_addr().unwrap().port();
    let mut t = Tally::default();
    let tcp = format!("timeout 3 bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected'");
    let open = bw("w4open", Profile::Workspace);
    let (out, _) = sh(&open, &tcp).await;
    t.control("the workspace profile reaches the host's loopback", out.contains("connected"), &out);
    for profile in [Profile::Strict, Profile::ReadOnly] {
        let f = bw(&format!("w4{}", profile.name()), profile);
        let (out, _) = sh(&f, &tcp).await;
        t.blocked(&format!("4a {} TCP to host 127.0.0.1", profile.name()), !out.contains("connected"), out);
        let (out, _) = sh(&f, &format!("echo leak > /dev/udp/127.0.0.1/{uport}")).await;
        let mut buf = [0u8; 16];
        t.blocked(&format!("4b {} UDP to host 127.0.0.1", profile.name()), udp.recv_from(&mut buf).is_err(), out);
        // A name no cache holds, so a resolution is a query that left the host.
        let (out, _) = sh(&f, &format!("timeout 5 getent ahosts r192b-{}-exfil.example.com; timeout 5 getent ahosts example.com && echo resolved; ls -l /run/systemd/resolve 2>&1", std::process::id())).await;
        t.blocked(&format!("4c {} DNS (getent, via nss-resolve)", profile.name()), !out.contains("resolved"), out);
        // (A UDP send to 127.0.0.53 "succeeds" inside too, but only onto the
        // namespace's own loopback, which nothing listens on — 4b shows the
        // host hears nothing; the leak is the resolver's unix socket.)
    }
    // Workspace keeps the network namespace, so abstract unix sockets on the
    // host (X11's `@/tmp/.X11-unix/X0`, others) are reachable: a note, not a
    // failure.
    use std::os::linux::net::SocketAddrExt;
    let name = format!("r192b-{}", std::process::id());
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let _ul = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
    let (out, _) = sh(&open, &format!("python3 -c \"import socket; s=socket.socket(socket.AF_UNIX); s.connect(b'\\0{name}'); print('ABS'+'OK')\" 2>&1")).await;
    eprintln!("NOTE 4d workspace abstract unix socket reachable: {}", out.contains("ABSOK"));
    let strict = bw("w4abs", Profile::Strict);
    let (out, _) = sh(&strict, &format!("python3 -c \"import socket; s=socket.socket(socket.AF_UNIX); s.connect(b'\\0{name}'); print('ABS'+'OK')\" 2>&1")).await;
    t.blocked("4e strict abstract unix socket", !out.contains("ABSOK"), out);
    drop(listener);
    t.done();
}

// ---------------------------------------------------------------- 5

#[tokio::test(flavor = "current_thread")]
async fn r192b_5_isolation() {
    if !enforced("r192b_5") {
        return;
    }
    let f = bw("w5", Profile::Workspace);
    let mut t = Tally::default();
    // A descriptor this process holds without close-on-exec.
    let fd = std::os::fd::IntoRawFd::into_raw_fd(std::fs::File::open(f.home.join("control/readable.txt")).unwrap());
    // SAFETY: the descriptor was just opened here and is closed below.
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    let (out, _) = sh(&f, "sleep 5 & p=$!; ls /proc/$p/fd > /tmp/fds; kill $p; sort -n /tmp/fds | tr '\\n' ' '").await;
    unsafe { libc::close(fd) };
    t.blocked("5a only fds 0/1/2", out.lines().next().map(str::trim) == Some("0 1 2"), out);

    let (out, _) = sh(&f, "ps -o sid= -p $$").await;
    // SAFETY: getsid(0) reads this process's session id.
    let ours = unsafe { libc::getsid(0) }.to_string();
    let sid = out.lines().next().unwrap_or("").trim().to_string();
    t.blocked("5b session differs from the parent", !sid.is_empty() && sid != ours, out);

    let (out, _) = sh(&f, "setsid sleep 9.871 > /dev/null 2>&1 < /dev/null & (setsid -f sleep 9.872 &) ; nohup sleep 9.873 >/dev/null 2>&1 & disown; echo started").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let alive = std::process::Command::new("pgrep").args(["-f", "sleep 9.87"]).output().map(|o| o.status.success()).unwrap_or(false);
    t.blocked("5c setsid/nohup children killed after the call", !alive, out);

    let (out, _) = sh(&f, "ls /dev/pts; python3 - <<'EOF' 2>&1\nimport fcntl, termios, os\nfor path in ['/dev/tty', '/proc/1/fd/0', '/proc/1/fd/1', '/proc/1/fd/2']:\n    try:\n        fd = os.open(path, os.O_RDWR)\n        fcntl.ioctl(fd, termios.TIOCSTI, b'x')\n        print('INJECTED', path)\n    except Exception as e:\n        print('refused', path, e)\nfor fd in (0, 1, 2):\n    try:\n        fcntl.ioctl(fd, termios.TIOCSTI, b'x'); print('INJECTED fd', fd)\n    except Exception as e:\n        print('refused fd', fd, e)\nEOF").await;
    t.blocked("5d TIOCSTI into a tty", !out.contains("INJECTED"), out);

    // --die-with-parent: krowk's stand-in (a shell) killed hard while the
    // sandbox runs; nothing inside survives it.
    let (program, args) = sandbox::bash(f.scope.sandbox.as_deref().unwrap(), "sleep 9.874 & sleep 9.875").unwrap();
    let quoted: Vec<String> = std::iter::once(program.display().to_string()).chain(args).map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect();
    let mut parent = std::process::Command::new("bash").arg("-c").arg(format!("{} & wait", quoted.join(" "))).current_dir(&f.ws).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let started = std::process::Command::new("pgrep").args(["-f", "sleep 9.875"]).output().map(|o| o.status.success()).unwrap_or(false);
    t.control("the sandboxed sleep started", started, "no sleep 9.875");
    // SAFETY: SIGKILL to the child just spawned.
    unsafe { libc::kill(parent.id() as i32, libc::SIGKILL) };
    let _ = parent.wait();
    std::thread::sleep(Duration::from_millis(500));
    let alive = std::process::Command::new("pgrep").args(["-f", "sleep 9.87[45]"]).output().map(|o| o.status.success()).unwrap_or(false);
    let _ = std::process::Command::new("pkill").args(["-f", "sleep 9.87"]).status();
    t.blocked("5e nothing survives the parent's death (--die-with-parent)", !alive, "a sandboxed sleep outlived its parent");
    t.done();
}

// ---------------------------------------------------------------- 6 / 7

async fn file_fences(label: &str, f: &Fx, t: &mut Tally) {
    std::fs::create_dir_all(f.base.join("outdir")).unwrap();
    let _ = std::os::unix::fs::symlink(f.base.join("outdir"), f.ws.join("lnk-out"));
    let _ = std::os::unix::fs::symlink(f.ws.join(".git/hooks"), f.ws.join("lnk-hooks"));
    let config = read(&f.ws.join(".git/config"));
    let cases: [(&str, Value, PathBuf); 5] = [
        ("outside the workspace", json!({"path": f.base.join("out.txt"), "content": "x"}), f.base.join("out.txt")),
        ("into .git/hooks", json!({"path": ".git/hooks/pre-commit", "content": "echo pwned"}), f.ws.join(".git/hooks/pre-commit")),
        ("through a symlinked parent out", json!({"path": "lnk-out/x.txt", "content": "x"}), f.base.join("outdir/x.txt")),
        ("through a symlinked parent into .git/hooks", json!({"path": "lnk-hooks/post-merge", "content": "echo pwned"}), f.ws.join(".git/hooks/post-merge")),
        ("into ~/.ssh/authorized_keys", json!({"path": f.home.join(".ssh/authorized_keys"), "content": "ssh-ed25519 AAAA fake"}), f.home.join(".ssh/authorized_keys")),
    ];
    for (what, input, landed) in cases {
        let (out, _) = run(f, WRITE, input).await;
        t.blocked(&format!("{label} write {what}"), !landed.exists(), out);
    }
    let (out, _) = run(f, STR_REPLACE, json!({"path": ".git/config", "old_str": "[core]", "new_str": "[core]\n\tfsmonitor = echo pwned\n"})).await;
    t.blocked(&format!("{label} str_replace .git/config"), read(&f.ws.join(".git/config")) == config, out);
    let (out, _) = run(f, READ, json!({"path": f.home.join(".ssh/id_ed25519")})).await;
    t.blocked(&format!("{label} read ~/.ssh key"), !out.contains(MARKER), out);
    let (out, err) = run(f, WRITE, json!({"path": "ok.txt", "content": "x"})).await;
    t.control("a workspace write works", !err && f.ws.join("ok.txt").exists(), &out);
}

#[tokio::test(flavor = "current_thread")]
async fn r192b_6_file_tool_fences_hold_in_every_mode() {
    let mut t = Tally::default();
    for mode in [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan, PermissionMode::BypassPermissions, PermissionMode::Unhinged] {
        let f = fx(&format!("w6{}", mode.name()), Profile::Workspace, By::Bubblewrap, mode);
        file_fences(&format!("6 [{}]", mode.name()), &f, &mut t).await;
    }
    t.done();
}

#[tokio::test(flavor = "current_thread")]
async fn r192b_7_container_mode_keeps_the_file_tool_fences() {
    let mut t = Tally::default();
    for mode in [PermissionMode::BypassPermissions, PermissionMode::Unhinged] {
        let f = fx(&format!("w7{}", mode.name()), Profile::Workspace, By::Container, mode);
        assert!(!f.scope.sandbox.as_ref().unwrap().kernel, "container: no bubblewrap");
        file_fences(&format!("7 container [{}]", mode.name()), &f, &mut t).await;
    }
    t.done();
}

// ---------------------------------------------------------------- 8

#[tokio::test(flavor = "current_thread")]
async fn r192b_8_cargo_builds_offline_and_cannot_write_its_homes() {
    if !enforced("r192b_8") {
        return;
    }
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    let rustup = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).or_else(|| real_home.map(|h| h.join(".rustup")));
    let Some(rustup) = rustup.filter(|r| r.is_dir()) else {
        eprintln!("r192b_8 skipped: no rustup");
        return;
    };
    // A cargo home of the test's own, with a registry token; the real
    // rustup home, which is only read.
    let b = base("w8home");
    let cargo_home = b.join("cargo");
    std::fs::create_dir_all(cargo_home.join("registry")).unwrap();
    std::fs::write(cargo_home.join("credentials.toml"), format!("[registry]\ntoken = \"{MARKER}-cargo\"\n")).unwrap();
    // SAFETY: the test's own process (nextest), before the plan reads them.
    unsafe {
        std::env::set_var("CARGO_HOME", &cargo_home);
        std::env::set_var("RUSTUP_HOME", &rustup);
    }
    let f = bw("w8", Profile::Workspace);
    let mut t = Tally::default();
    std::fs::write(f.ws.join("Cargo.toml"), "[package]\nname = \"r192b\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n").unwrap();
    std::fs::create_dir_all(f.ws.join("src")).unwrap();
    std::fs::write(f.ws.join("src/main.rs"), "fn main() { println!(\"built\"); }\n").unwrap();
    let (out, err) = sh(&f, "cargo build --offline -q 2>&1 && ./target/debug/r192b").await;
    t.blocked("8a cargo build --offline works inside (functional, not an escape)", !err && out.contains("built"), out);
    let probe = format!("r192b-probe-{}", std::process::id());
    let (out, _) = sh(&f, &format!("echo x > \"$CARGO_HOME/{probe}\"; echo x > \"$CARGO_HOME/registry/{probe}\"; cat \"$CARGO_HOME/credentials.toml\"")).await;
    let wrote = cargo_home.join(&probe).exists() || cargo_home.join("registry").join(&probe).exists();
    t.blocked("8b write into CARGO_HOME", !wrote, out.clone());
    t.blocked("8c read CARGO_HOME/credentials.toml", !out.contains(MARKER), out);
    let (out, _) = sh(&f, &format!("echo x > \"$RUSTUP_HOME/{probe}\"")).await;
    let wrote = rustup.join(&probe).exists();
    let _ = std::fs::remove_file(rustup.join(&probe));
    t.blocked("8d write into RUSTUP_HOME", !wrote, out);
    let _ = std::fs::remove_dir_all(&b);
    t.done();
}
