//! R-BACK-3, non-negotiable: krowk drives the official `codex app-server`,
//! the person signs in with `codex login`, and krowk never reuses Codex's
//! OAuth client id, never runs an OAuth flow against OpenAI's sign-in
//! server, never reads Codex's login file, and never passes itself off as a
//! Codex client. Two checks hold every build to it:
//!
//! - the source: no file in the workspace names Codex's login file, its
//!   OAuth client id or sign-in server, the ChatGPT backend Codex calls with
//!   that login, or the headers and originator Codex's own clients send —
//!   and none feeds a key or a token to `codex login`;
//! - a run: every command that touches a Codex instance — adding two
//!   accounts, listing them, a session on each, importing and listing
//!   sessions, removing one — runs with each account's login file, and the
//!   person's own, replaced by a FIFO watched for a reader. Opening one for
//!   reading fails the test.
//!
//! The needles are assembled from pieces, so this file does not trip its
//! own check.

#![cfg(all(feature = "harness", unix))]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn login_file() -> String {
    ["auth", ".json"].concat()
}

fn needles() -> Vec<String> {
    let c = |parts: &[&str]| parts.concat();
    vec![
        // Codex's login file.
        login_file(),
        // Codex's OAuth client, and OpenAI's sign-in server it talks to.
        c(&["app_EMoamEEZ73f0", "CkXaXp7hrann"]),
        c(&["auth.openai", ".com"]),
        // The ChatGPT backend a Codex login is spent on, and the account
        // header that goes with it.
        c(&["chatgpt.com/backend", "-api"]),
        c(&["chatgpt-account", "-id"]),
        // Codex's own clients, as the originator a request would claim.
        c(&["codex_cli", "_rs"]),
        c(&["codex_vs", "code"]),
        // Handing `codex login` a key or a token instead of letting the
        // person sign in.
        c(&["--with-api", "-key"]),
        c(&["--with-access", "-token"]),
    ]
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        if matches!(name.to_str(), Some("target" | ".git" | "node_modules" | "dist" | "bin")) {
            continue;
        }
        if p.is_dir() {
            walk(&p, out);
        } else if matches!(p.extension().and_then(|x| x.to_str()), Some("rs" | "sh" | "toml" | "js" | "mjs" | "py" | "json" | "jsonl" | "md" | "txt")) || name == "fake-codex" {
            out.push(p);
        }
    }
}

/// The findings, as `file: needle`, over every source file under `roots`.
fn scan(roots: &[PathBuf]) -> Vec<String> {
    let mut files = Vec::new();
    for r in roots {
        walk(r, &mut files);
    }
    let needles = needles();
    let mut found = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let lower = text.to_lowercase();
        for n in &needles {
            if lower.contains(&n.to_lowercase()) {
                found.push(format!("{}: {n}", f.display()));
            }
        }
    }
    found
}

#[test]
fn r_back_3_no_source_file_names_codexs_login_file_or_its_oauth_client() {
    let ws = workspace();
    let roots: Vec<PathBuf> = ["crates", "tests", "scripts", "npm", "skills"].iter().map(|d| ws.join(d)).collect();
    let found = scan(&roots);
    assert!(found.is_empty(), "R-BACK-3: krowk never reads Codex's login, reuses its OAuth client or poses as a Codex client:\n{}", found.join("\n"));

    // The check itself: a file that reads the login, or names the client,
    // is caught.
    let canary = std::env::temp_dir().join(format!("krowk-codex-compliance-canary-{}", std::process::id()));
    std::fs::create_dir_all(&canary).unwrap();
    std::fs::write(canary.join("broken.rs"), format!("let _ = std::fs::read(codex_home.join(\"{}\"));", login_file())).unwrap();
    std::fs::write(canary.join("client.rs"), ["const CLIENT_ID: &str = \"app_EMoamEEZ73f0", "CkXaXp7hrann\";"].concat()).unwrap();
    let caught = scan(std::slice::from_ref(&canary));
    let _ = std::fs::remove_dir_all(&canary);
    assert_eq!(caught.len(), 2, "the source check did not catch a read of the login file and the client id: {caught:?}");
}

/// Watches FIFOs for a reader: a writer's non-blocking open succeeds only
/// while something has the FIFO open for reading, and unblocks it, so a
/// read is both seen and never left hanging.
struct Tripwire {
    stop: Arc<AtomicBool>,
    hits: Arc<Mutex<Vec<PathBuf>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Tripwire {
    fn arm(paths: Vec<PathBuf>) -> Tripwire {
        for p in &paths {
            let _ = std::fs::remove_file(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            // SAFETY: a valid C path; mkfifo only creates a file.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo {}", p.display());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let (s, h) = (stop.clone(), hits.clone());
        let thread = std::thread::spawn(move || {
            let cs: Vec<(PathBuf, CString)> = paths.iter().map(|p| (p.clone(), CString::new(p.as_os_str().as_bytes()).unwrap())).collect();
            while !s.load(Ordering::Relaxed) {
                for (p, c) in &cs {
                    // SAFETY: a valid C path; the descriptor is closed at once.
                    let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
                    if fd >= 0 {
                        h.lock().unwrap().push(p.clone());
                        unsafe { libc::close(fd) };
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        Tripwire { stop, hits, thread: Some(thread) }
    }

    fn hits(&self) -> Vec<PathBuf> {
        self.hits.lock().unwrap().clone()
    }

    fn reset(&self) {
        self.hits.lock().unwrap().clear();
    }
}

impl Drop for Tripwire {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn r_back_3_a_whole_codex_workflow_never_opens_codexs_login_file() {
    let root = std::env::temp_dir().join(format!("krowk-codex-compliance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["home/.codex", "repo/.git", "bin", "home-work"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join("home/.codex/config.toml"), "model = \"gpt-5.5\"\n").unwrap();
    let bin = root.join("bin/codex");
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/codex/fake-codex"), &bin).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = root.join("home/.krowk/accounts");
    let own = root.join("home/.codex").join(login_file());
    let team = data.join("codex-team").join(login_file());
    let work = root.join("home-work").join(login_file());
    let wire = Tripwire::arm(vec![own.clone(), team.clone(), work.clone()]);

    // The tripwire itself: a read of the file is caught.
    let _ = std::fs::read(&own);
    assert!(wire.hits().contains(&own), "the tripwire did not see a read");
    wire.reset();

    let krowk = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .current_dir(root.join("repo"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let home_work = root.join("home-work").display().to_string();
    let steps: Vec<(Vec<&str>, bool)> = vec![
        (vec!["providers", "add", "codex", "--name", "team", "--json"], true),
        (vec!["providers", "add", "codex", "--name", "work", "--config-dir", &home_work, "--json"], true),
        (vec!["providers", "list", "--json"], true),
        (vec!["-p", "hello", "--model", "codex:team/gpt-5.5", "--trust", "--output-format", "json"], true),
        (vec!["-p", "hello", "--model", "codex:work/gpt-5.5", "--trust", "--output-format", "json"], true),
        (vec!["sessions", "sync", "--json"], false),
        (vec!["sessions", "--json"], true),
        (vec!["providers", "remove", "codex:work", "--json"], true),
    ];
    for (args, must_succeed) in steps {
        let (ok, said) = krowk(&args);
        assert!(ok || !must_succeed, "krowk {args:?}: {said}");
        assert!(wire.hits().is_empty(), "R-BACK-3: `krowk {}` opened Codex's login file: {:?}", args.join(" "), wire.hits());
    }
    drop(wire);
    let _ = std::fs::remove_dir_all(&root);
}
