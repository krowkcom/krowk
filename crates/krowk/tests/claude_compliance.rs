//! R-BACK-2, non-negotiable: krowk never runs a Claude OAuth flow, never
//! reads Claude's credentials — the keychain entry or the credentials file
//! in Claude's config directory — and never passes itself off as Claude
//! Code. Three checks hold every build to it:
//!
//! - the source: no file in the workspace names Claude's credential paths,
//!   its keychain service, its OAuth endpoints or client, or Claude Code's
//!   own headers and prompt;
//! - a run: every command that touches a Claude Code instance — adding two
//!   accounts, listing them, a session on each, importing and listing
//!   sessions — runs with each account's credentials file replaced by a
//!   FIFO that is watched for a reader, and with the macOS and Linux
//!   keychain tools on PATH as tripwires.
//!   Opening the file for reading, or asking a keychain, fails the test;
//! - the dependency tree: no crate in `Cargo.lock` is a keychain or secret
//!   store client, save the macOS Security framework binding that
//!   rustls's platform verifier uses to read the system's trusted
//!   certificates — and only while that is all that uses it.
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

fn needles() -> Vec<String> {
    let c = |parts: &[&str]| parts.concat();
    vec![
        // Claude's credentials file, and its macOS keychain service.
        c(&[".claude/", ".credentials"]),
        c(&["\"", ".credentials", ".json\""]),
        c(&["Claude Code", "-credentials"]),
        // Asking a keychain for anything.
        c(&["find-generic", "-password"]),
        c(&["find-internet", "-password"]),
        c(&["SecItem", "CopyMatching"]),
        c(&["secret", "-tool"]),
        c(&["key", "ring"]),
        c(&["security", "-framework"]),
        c(&["secret", "-service"]),
        c(&["org.freedesktop", ".secrets"]),
        c(&["SecKey", "chain"]),
        c(&["/usr/bin/", "security"]),
        // Claude's OAuth: its endpoints, its client, its tokens.
        c(&["claude.ai/", "oauth"]),
        c(&["console.anthropic.com/v1/", "oauth"]),
        c(&["platform.claude.com/v1/", "oauth"]),
        c(&["9d1c250a-e61b-44d9", "-88ed-5944d1962f5e"]),
        c(&["CLAUDE_CODE_", "OAUTH_TOKEN"]),
        c(&["sk-ant-", "oat"]),
        // Claude Code's own headers and prompt.
        c(&["claude-code-", "20250219"]),
        c(&["oauth-2025", "-04-20"]),
        c(&["\"x-", "app\""]),
        c(&["You are Claude Code, ", "Anthropic's official CLI"]),
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
        // The sandbox's escape suite (R-PERM-3) plants fake copies of the
        // vendors' login files in a scratch home to prove the sandbox hides
        // them; it names the paths to write decoys, never to read the real
        // files. Exempt by its exact path, so nothing else hides here.
        if name == "r192b_escapes.rs" && dir.ends_with("crates/krowk-harness/tests") {
            continue;
        }
        // The schema of `codex app-server`, pinned byte for byte from Codex
        // (scripts/codex_schema.sh): Codex's own words for its own options,
        // one of them a login store krowk never uses — not krowk's source.
        if name == "codex" && dir.file_name().is_some_and(|d| d == "schema") {
            continue;
        }
        if p.is_dir() {
            walk(&p, out);
        } else if matches!(p.extension().and_then(|x| x.to_str()), Some("rs" | "sh" | "toml" | "js" | "mjs" | "py" | "json" | "jsonl" | "md")) || name == "fake-claude" {
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
fn r_back_2_no_source_file_names_claudes_credentials_its_oauth_or_its_headers() {
    let ws = workspace();
    let roots: Vec<PathBuf> = ["crates", "tests", "scripts", "npm", "skills"].iter().map(|d| ws.join(d)).collect();
    let found = scan(&roots);
    assert!(found.is_empty(), "R-BACK-2: krowk never reads Claude's credentials, runs its OAuth or spoofs Claude Code:\n{}", found.join("\n"));

    // The check itself: a file that reads the path is caught.
    let canary = std::env::temp_dir().join(format!("krowk-compliance-canary-{}", std::process::id()));
    std::fs::create_dir_all(&canary).unwrap();
    std::fs::write(canary.join("broken.rs"), ["let _ = std::fs::read(home.join(\"", ".claude/", ".credentials", ".json\"));"].concat()).unwrap();
    let caught = scan(std::slice::from_ref(&canary));
    let _ = std::fs::remove_dir_all(&canary);
    assert!(!caught.is_empty(), "the source check did not catch a read of the credentials path");
}

/// Keychain and secret-store clients, by crate name.
fn keychain_crates() -> Vec<String> {
    let c = |parts: &[&str]| parts.concat();
    vec![
        c(&["key", "ring"]),
        c(&["secret", "-service"]),
        c(&["dbus-secret", "-service"]),
        c(&["lib", "secret"]),
        c(&["oo", "7"]),
        c(&["keychain", "-services"]),
        c(&["security", "-framework"]),
        c(&["security", "-framework-sys"]),
        c(&["apple-native-key", "ring-store"]),
    ]
}

/// The crates that may bring in the Security framework binding: they read
/// the system's certificate trust settings, never a password.
const TRUST_STORE_READERS: &[&str] = &["rustls-platform-verifier", "rustls-native-certs"];

/// Each keychain crate in a lockfile, with what depends on it, unless all
/// that depends on it reads trust settings.
fn keychain_deps(lock: &str) -> Vec<String> {
    let packages: Vec<(String, Vec<String>)> = lock
        .split("[[package]]")
        .filter_map(|blk| {
            let name = blk.lines().find_map(|l| l.strip_prefix("name = \"")?.strip_suffix('"'))?.to_string();
            let deps = blk
                .split_once("dependencies = [")
                .map(|(_, d)| d.split(']').next().unwrap_or_default().split(',').filter_map(|x| x.trim().trim_matches('"').split(' ').next().filter(|n| !n.is_empty()).map(String::from)).collect())
                .unwrap_or_default();
            Some((name, deps))
        })
        .collect();
    let forbidden = keychain_crates();
    let mut found = Vec::new();
    for (name, _) in packages.iter().filter(|(n, _)| forbidden.contains(n)) {
        let users: Vec<&str> = packages.iter().filter(|(_, d)| d.contains(name)).map(|(n, _)| n.as_str()).collect();
        let only_trust = !users.is_empty() && users.iter().all(|u| TRUST_STORE_READERS.contains(u) || forbidden.iter().any(|f| f == u && f.starts_with("security")));
        if !(name.starts_with("security") && only_trust) {
            found.push(format!("{name} (used by {})", if users.is_empty() { "the workspace".to_string() } else { users.join(", ") }));
        }
    }
    found
}

#[test]
fn r_back_2_no_crate_krowk_links_is_a_keychain_client() {
    let lock = std::fs::read_to_string(workspace().join("Cargo.lock")).unwrap();
    let found = keychain_deps(&lock);
    assert!(found.is_empty(), "R-BACK-2: krowk links a keychain or secret-store client: {found:?}");
    // The check itself: a keychain crate, or the Security binding used by
    // anything but the trust-store readers, is caught.
    let kr = ["key", "ring"].concat();
    let broken = format!("[[package]]\nname = \"krowk\"\ndependencies = [\n \"{kr}\",\n]\n\n[[package]]\nname = \"{kr}\"\n");
    assert_eq!(keychain_deps(&broken).len(), 1);
    let sf = ["security", "-framework"].concat();
    let misused = format!("[[package]]\nname = \"krowk\"\ndependencies = [\n \"{sf} 3.5.1\",\n]\n\n[[package]]\nname = \"{sf}\"\n");
    assert_eq!(keychain_deps(&misused).len(), 1, "the Security binding used directly");
    let fine = format!("[[package]]\nname = \"rustls-platform-verifier\"\ndependencies = [\n \"{sf}\",\n]\n\n[[package]]\nname = \"{sf}\"\n");
    assert!(keychain_deps(&fine).is_empty());
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
fn r_back_2_a_whole_claude_workflow_never_opens_claudes_credentials_or_a_keychain() {
    let root = std::env::temp_dir().join(format!("krowk-compliance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["home", "repo/.git", "bin", "cfg-work"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    use std::os::unix::fs::PermissionsExt;
    let install = |name: &str, body: &str| {
        let p = root.join("bin").join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    install("claude", &std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude/fake-claude")).unwrap());
    let asked = root.join("keychain-asked");
    for tool in ["security".to_string(), ["secret", "-tool"].concat()] {
        install(&tool, &format!("#!/bin/sh\necho \"{tool} $*\" >> '{}'\nexit 44\n", asked.display()));
    }
    let file = [".credentials", ".json"].concat();
    let default_creds = root.join("home/.claude").join(&file);
    let work_creds = root.join("cfg-work").join(&file);
    let wire = Tripwire::arm(vec![default_creds.clone(), work_creds.clone()]);

    // The tripwire itself: a read of the file is caught.
    let _ = std::fs::read(&default_creds);
    assert!(wire.hits().contains(&default_creds), "the tripwire did not see a read");
    wire.reset();

    let krowk = |args: &[&str], scenario: Option<&str>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .current_dir(root.join("repo"))
            .stdin(Stdio::null());
        if let Some(s) = scenario {
            c.env("FAKE_CLAUDE_SCENARIO", Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude").join(s));
        }
        let out = c.output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let cfg_work = root.join("cfg-work").display().to_string();
    let steps: Vec<(Vec<&str>, Option<&str>, bool)> = vec![
        (vec!["providers", "add", "claude", "--json"], None, true),
        (vec!["providers", "add", "claude", "--name", "work", "--config-dir", &cfg_work, "--json"], None, true),
        (vec!["providers", "list", "--json"], None, true),
        (vec!["-p", "what session is this?", "--model", "claude:work/sonnet", "--trust", "--output-format", "json"], Some("session_info.jsonl"), true),
        (vec!["-p", "hello", "--model", "claude/haiku", "--trust", "--output-format", "json"], None, true),
        (vec!["sessions", "sync", "--json"], None, false),
        (vec!["sessions", "--json"], None, true),
        (vec!["providers", "remove", "claude:work", "--json"], None, true),
    ];
    for (args, scenario, must_succeed) in steps {
        let (ok, said) = krowk(&args, scenario);
        assert!(ok || !must_succeed, "krowk {args:?}: {said}");
        assert!(wire.hits().is_empty(), "R-BACK-2: `krowk {}` opened Claude's credentials file: {:?}", args.join(" "), wire.hits());
    }
    assert!(!asked.exists(), "R-BACK-2: krowk asked a keychain: {}", std::fs::read_to_string(&asked).unwrap_or_default());
    drop(wire);
    let _ = std::fs::remove_dir_all(&root);
}
