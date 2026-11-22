//! The first machine's setup from bare `krowk` (D4, D5, R-EASE-1..6), the
//! built binary on a pseudo-terminal against the in-process stand-in
//! registry and the fake `claude`: sign in or use krowk locally, connect a
//! model with `krowk connect`'s pickers, the always-on question and the
//! recovery kit, each as plain prompts; resumed by running `krowk` again;
//! an account that already has machines joining them with a code; and the
//! lines the overview adds (D9, D5).

#![cfg(all(feature = "harness", unix))]

#[path = "common/pty.rs"]
mod pty;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);
const REMINDER: &str = "One machine, no recovery kit: losing it loses your sessions. krowk machines recovery.";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-firstrun-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        // The fake `claude` and `codex`, signed in to nothing, and a service
        // manager that only logs: nothing real is reached or installed.
        for (dir, bin, to) in [("claude", "fake-claude", "claude"), ("codex", "fake-codex", "codex")] {
            let at = root.join("bin").join(to);
            std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures").join(dir).join(bin), &at).unwrap();
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let stub = root.join("bin/systemctl");
        std::fs::write(&stub, format!("#!/bin/sh\necho \"$@\" >> {}\n", root.join("systemctl.log").display())).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    /// `krowk args…` on this home, signing in at the stand-in registry
    /// `api`; headless, so no real browser opens.
    fn command(&self, api: &str, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.root.join("home"))
            .env("TERM", "xterm-256color")
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("GRUNTAS_API_URL", api)
            .env("SSH_CONNECTION", "x 1 y 2")
            .env("GRUNTAS_DEVICE_NAME", "first")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            // The fake's login waits for a line on the terminal, as the
            // real one waits on a person.
            .env("FAKE_CLAUDE_LOGIN", "ask")
            .current_dir(self.root.join("repo"));
        c
    }

    /// The fake `claude` signed in, as Claude Code is once a person has
    /// logged in: a model ready here.
    fn claude_signed_in(&self) {
        std::fs::create_dir_all(self.root.join("home/.claude")).unwrap();
        std::fs::write(self.root.join("home/.claude/fake-login"), "").unwrap();
    }

    fn state(&self) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.root.join("home/.krowk/first-run.json")).unwrap_or_else(|_| "{}".into())).unwrap()
    }

    fn keystore(&self) -> gruntas_client::keystore::Keystore {
        gruntas_client::keystore::Keystore::new(&self.root.join("home/.krowk"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn registry(config: gruntas_devregistry::Config) -> (gruntas_devregistry::Running, String) {
    let r = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).unwrap();
    let api = format!("{}/v1", r.url());
    (r, api)
}

/// The person at the browser: approves the login whose page the terminal
/// shows.
fn signs_in(r: &gruntas_devregistry::Running, t: &pty::Pty) {
    decides(r, t, "approval");
}

/// Answers the login whose page the terminal shows: `approval` or `denial`.
fn decides(r: &gruntas_devregistry::Running, t: &pty::Pty, decision: &str) {
    let deadline = Instant::now() + WAIT;
    let code = loop {
        let text = t.text();
        if let Some(at) = text.find("authorizations/new?code=") {
            let code: String = text[at + "authorizations/new?code=".len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
            if code.len() >= 4 {
                break code;
            }
        }
        assert!(Instant::now() < deadline, "no sign-in page shown: {:?}", t.text());
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut s = TcpStream::connect(r.addr()).unwrap();
    write!(s, "POST /_approve/cli/authorizations/{code}/{decision} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
}

fn says(t: &pty::Pty, needle: &str) {
    assert!(t.wait_for(needle, WAIT).is_some(), "{needle:?} not shown: {:?}", t.text());
}

/// Types `keys` once what came before has settled.
fn types(t: &mut pty::Pty, keys: &[u8]) {
    std::thread::sleep(Duration::from_millis(300));
    t.write(keys);
}

fn ends(t: &mut pty::Pty) {
    let status = t.wait(WAIT).expect("krowk ended");
    assert!(status.success(), "{status:?}: {:?}", t.text());
}

/// `krowk connect`'s pickers, through to the fake Claude's login.
fn connects_claude(t: &mut pty::Pty) {
    says(t, "Connect a model");
    for (question, keys) in [("Connect which provider?", &b"\r"[..]), ("How do you connect anthropic?", b"\r"), ("not signed in, reconnect", b"\r"), ("Press Enter to sign in to Claude", b"\r")] {
        says(t, question);
        types(t, keys);
    }
    says(t, "Connected claude");
}

fn wide(c: Command) -> pty::Pty {
    pty::Pty::spawn(c, 200, 40)
}

/// A fresh home goes sign-in, registered for sync with a workspace key, then
/// the model, with no other prompt. Cancelled at the model step, the next
/// `krowk` goes straight back to it; then the always-on question, declined,
/// and the recovery kit, saved.
#[test]
fn a_fresh_home_signs_in_registers_for_sync_and_then_connects_a_model() {
    let (r, api) = registry(Default::default());
    let b = Sandbox::new("fresh");
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Sign in to krowk");
    says(&t, "[Enter] open the browser   [l] use krowk locally, without an account");
    types(&mut t, b"\r");
    signs_in(&r, &t);
    says(&t, "Signed in. 'first' is set up to sync.");
    says(&t, "Connect which provider?");
    assert!(b.keystore().device_list().unwrap().is_some_and(|c| c.devices().len() == 1), "registered on a list of one");
    assert_eq!(b.state()["pending"], false);
    // Cancelled at the model step: said, and the overview follows.
    types(&mut t, b"\x1b");
    says(&t, "run krowk again to connect one");
    says(&t, "krowk help agents");
    ends(&mut t);
    assert_eq!(b.state()["always_on_asked"], false, "nothing past the model step was asked");

    // Run again: no account question, the model step.
    let mut t = wide(b.command(&api, &[]));
    connects_claude(&mut t);
    assert!(!t.text().contains("Sign in to krowk"), "{:?}", t.text());
    says(&t, "Keep this machine available while you're away?");
    // `[Y/n]` on a desktop, `[y/N]` on a machine with a battery: answered
    // either way.
    says(&t, "It syncs your sessions and takes work from your other machines. [");
    types(&mut t, b"n\r");
    says(&t, "Not enabled. `krowk machines always-on on` does it later.");
    says(&t, "12 words that stand in for a device of yours");
    says(&t, "Nobody else can do this for you, krowk included.");
    says(&t, "[Enter] saved   [s] later");
    assert!(b.keystore().device_list().unwrap().is_some_and(|c| c.recovery().is_none()), "no kit until it is saved");
    types(&mut t, b"\r");
    says(&t, "Your recovery kit is on your device list.");
    says(&t, "krowk help agents");
    ends(&mut t);
    assert!(b.keystore().device_list().unwrap().is_some_and(|c| c.recovery().is_some()), "the kit is on the list");
    let state = b.state();
    assert!(state["always_on_asked"] == true && state["kit_offered"] == true, "{state}");
    assert!(!b.root.join("home/.config/systemd/user/gruntas-host.service").exists(), "declined: nothing installed");
    assert!(!t.text().contains(REMINDER), "a kit: no reminder");

    // Set up: the overview alone.
    let mut t = wide(b.command(&api, &[]));
    says(&t, "krowk help agents");
    ends(&mut t);
    assert!(!t.text().contains("Connect which provider?") && !t.text().contains("Keep this machine"), "{:?}", t.text());
}

/// "Later" at the kit leaves the list without one, and the overview reminds
/// the person while it is their only machine, as `krowk machines` does; the
/// kit is not offered again. A yes at the always-on question installs the
/// service (the service manager here only logs): on Linux only, where that
/// stand-in is `systemctl`.
#[cfg(target_os = "linux")]
#[test]
fn later_at_the_kit_is_not_asked_again_and_the_reminder_follows_while_there_is_one_machine() {
    let (r, api) = registry(Default::default());
    let b = Sandbox::new("later");
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Sign in to krowk");
    types(&mut t, b"\r");
    signs_in(&r, &t);
    connects_claude(&mut t);
    says(&t, "It syncs your sessions and takes work from your other machines. [");
    types(&mut t, b"y\r");
    says(&t, "This machine stays available");
    says(&t, "[Enter] saved   [s] later");
    types(&mut t, b"s\r");
    says(&t, "Later: `krowk machines recovery` makes one when you want it.");
    says(&t, REMINDER);
    ends(&mut t);
    let unit = std::fs::read_to_string(b.root.join("home/.config/systemd/user/gruntas-host.service")).unwrap_or_default();
    assert!(unit.contains("GRUNTAS_HOST_IDLE=0"), "a yes installs the service: {unit}");
    let log = std::fs::read_to_string(b.root.join("systemctl.log")).unwrap_or_default();
    assert!(log.contains("--user enable --now gruntas-host.service"), "{log}");
    assert!(b.keystore().device_list().unwrap().is_some_and(|c| c.recovery().is_none()), "later: no kit");

    // The next run: reminded again, never offered the kit again; piped too.
    let mut t = wide(b.command(&api, &[]));
    says(&t, REMINDER);
    ends(&mut t);
    assert!(!t.text().contains("12 words"), "{:?}", t.text());
    let out = b.command(&api, &[]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(REMINDER), "{}", String::from_utf8_lossy(&out.stdout));
    let out = b.command(&api, &["machines", "--format", "human"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && said.contains(REMINDER), "{said} {}", String::from_utf8_lossy(&out.stderr));
    // `krowk help` is not the overview: no reminder there.
    let out = b.command(&api, &["help"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains(REMINDER));
}

/// "Use locally" goes to the model step and is not offered again;
/// `krowk login` afterwards runs the account step: signed in, and
/// registered.
#[test]
fn using_krowk_locally_is_not_offered_again_and_krowk_login_runs_the_account_step() {
    let (r, api) = registry(Default::default());
    let b = Sandbox::new("locally");
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Sign in to krowk");
    types(&mut t, b"l\r");
    says(&t, "Connect which provider?");
    assert!(!t.text().contains("Waiting for approval"), "{:?}", t.text());
    assert_eq!(b.state()["local"], true);
    types(&mut t, b"\x1b");
    ends(&mut t);
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Connect which provider?");
    assert!(!t.text().contains("Sign in to krowk"), "{:?}", t.text());
    types(&mut t, b"\x1b");
    ends(&mut t);
    // `krowk login`: the account step from the top.
    let mut t = wide(b.command(&api, &["login"]));
    signs_in(&r, &t);
    says(&t, "is set up to sync");
    ends(&mut t);
    assert_eq!(b.state()["local"], false);
    assert!(b.keystore().device_list().unwrap().is_some());
}

/// The first machine of an account, set up as `krowk sync init` does, its
/// key `krowk_sk_first#laptop`.
fn first_machine(name: &str, api: &str) -> Sandbox {
    let other = Sandbox::new(name);
    let out = Command::new(env!("CARGO_BIN_EXE_gruntas"))
        .args(["sync", "init", "--save"])
        .arg(other.root.join("kit"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", other.root.join("home"))
        .env("GRUNTAS_NO_UPDATE_CHECK", "1")
        .env("GRUNTAS_API_URL", api)
        .env("GRUNTAS_TOKEN", "krowk_sk_first#laptop")
        .env("GRUNTAS_DEVICE_NAME", "laptop")
        .env("GRUNTAS_TEST_UNATTENDED_DEVICE_APPROVAL", "1")
        .current_dir(&other.root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    other
}

/// A machine signed in to the laptop's account — the browser's sign-in
/// stands in as a key of that person's — with its registration owed, as
/// the sign-in step leaves it: the next `krowk` takes it up.
fn signed_in_owing(name: &str, api: &str) -> Sandbox {
    let b = Sandbox::new(name);
    let login = b.command(api, &["login", "--token", "krowk_sk_first#second"]).output().unwrap();
    assert!(login.status.success(), "{}", String::from_utf8_lossy(&login.stderr));
    std::fs::write(b.root.join("home/.krowk/first-run.json"), "{\"pending\": true}\n").unwrap();
    b
}

/// An account that already has machines is not given a device list of its
/// own: this machine asks the others for a pairing (R-EASE-5, D6), the
/// laptop's host opens it and shows the code, the code is typed here, the
/// laptop says yes, and the model step follows. Nothing is owed after.
#[test]
fn an_account_with_machines_goes_to_the_add_a_machine_flow() {
    use gruntas_client::device_chain::SignedEntry;
    use gruntas_client::e2e::{unhex, DeviceSigner, SigningKey};
    use gruntas_client::protocol::OfferStage;
    use gruntas_client::user_key::UserKeys;
    use gruntas_harness::sync::introduce::{Introducer, Introductions, Timing};
    let (_r, api) = registry(Default::default());
    let other = first_machine("laptop-joins", &api);
    // The laptop's host, as its daemon is: keys and list read, presence
    // refreshed, each request handed to its introductions.
    let store = other.keystore();
    let device = store.device().unwrap().unwrap().id();
    let secret = store.signing_key().unwrap().secret_bytes().to_vec();
    let signing = move || SigningKey::from_secret(&secret).unwrap();
    let laptop = {
        let (api, signing) = (api.clone(), signing.clone());
        move || gruntas_api::Client::new(&api, "krowk_sk_first#laptop").signed_by(DeviceSigner::new(device, signing()).shared())
    };
    let entries: Vec<SignedEntry> = laptop().device_list_all().unwrap().entries.iter().map(|e| SignedEntry::from_parts(unhex(&e.entry).unwrap(), &unhex(&e.signatures).unwrap()).unwrap()).collect();
    let (chain, user) = (store.device_list().unwrap().unwrap(), store.user_keys().unwrap().unwrap().newest().clone());
    let host = {
        let (laptop, signing) = (laptop.clone(), signing.clone());
        Introductions::with_timing(
            Box::new(move || Ok(Introducer { api: Arc::new(laptop()), keys: UserKeys::new(user.clone(), []).unwrap(), chain: chain.clone(), entries: entries.clone(), device, signing: signing(), keep: None })),
            Timing { poll: Duration::from_millis(50), wait: Duration::from_secs(60) },
        )
    };
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refresher = {
        let (host, stop) = (host.clone(), stop.clone());
        std::thread::spawn(move || {
            let c = laptop();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                if let Ok(put) = c.put_presence(b"laptop", 30) {
                    host.deliver(&put.pairing_requests);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let b = signed_in_owing("second", &api);
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Your account already has machines.");
    says(&t, "Type the code shown on your other machine: ____-____");
    assert_eq!(b.state()["pending"], true, "owed until it has joined");
    let code = {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(o) = host.offer().filter(|o| o.stage == OfferStage::Waiting) {
                break o.code;
            }
            assert!(Instant::now() < deadline, "the laptop's host showed no code: {:?}", t.text());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    types(&mut t, format!("{code}\r").as_bytes());
    let asked = {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(o) = host.offer().filter(|o| o.stage == OfferStage::Confirm) {
                break o;
            }
            assert!(Instant::now() < deadline, "the laptop asked nothing: {:?}", t.text());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert_eq!(asked.name, "first");
    assert!(host.answer(&asked.pairing, true));
    says(&t, "Joined as 'first'");
    says(&t, "Connect which provider?");
    assert!(b.keystore().device_list().unwrap().is_some_and(|c| ["laptop", "first"].iter().all(|n| c.devices().iter().any(|d| d.name == *n))), "on the laptop's list, not one of its own");
    assert_eq!(b.state()["pending"], false, "nothing owed once joined");
    types(&mut t, b"\x1b");
    ends(&mut t);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = refresher.join();
}

/// No machine of the person's answers: the new machine says so, still owes
/// the join (it asks again at the next `krowk`), and the model step follows.
#[test]
fn a_new_machine_with_no_host_online_says_so_and_keeps_the_join_owed() {
    let (_r, api) = registry(Default::default());
    let _other = first_machine("laptop-nohost", &api);
    let b = signed_in_owing("nohost", &api);
    let mut cmd = b.command(&api, &[]);
    cmd.env("GRUNTAS_TEST_NO_HOST_SECS", "2");
    let mut t = wide(cmd);
    assert!(t.wait_for("None of your machines is online to show a code. Run krowk machines add on one of them.", Duration::from_secs(60)).is_some(), "{:?}", t.text());
    says(&t, "Connect which provider?");
    assert_eq!(b.state()["pending"], true, "still owed");
    assert!(b.keystore().device_list().unwrap().is_none(), "no list of its own");
    types(&mut t, b"\x1b");
    ends(&mut t);
}

/// Any answer but Enter or `l` at the sign-in question is "not now": nothing
/// is kept, and the overview follows. A registration left owed — signed in,
/// then stopped or failed — is taken up by the next `krowk`, even on a
/// machine that can run a model by now.
#[test]
fn a_registration_left_owed_is_finished_by_the_next_krowk_and_any_other_answer_is_not_now() {
    let (_r, api) = registry(Default::default());
    let b = Sandbox::new("owed");
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Sign in to krowk");
    types(&mut t, b"x\r");
    says(&t, "krowk help agents");
    ends(&mut t);
    assert!(!t.text().contains("Connect which provider?"), "{:?}", t.text());
    assert_eq!(b.state(), serde_json::json!({}));
    // A key stored, the registration owed, and a model ready.
    let login = b.command(&api, &["login", "--token", "krowk_sk_owed#a"]).output().unwrap();
    assert!(login.status.success(), "{}", String::from_utf8_lossy(&login.stderr));
    std::fs::write(b.root.join("home/.krowk/first-run.json"), "{\"pending\": true, \"always_on_asked\": true, \"kit_offered\": true}\n").unwrap();
    assert!(b.keystore().device_list().unwrap().is_none());
    b.claude_signed_in();
    let mut t = wide(b.command(&api, &[]));
    says(&t, "is set up to sync");
    says(&t, "krowk help agents");
    ends(&mut t);
    assert!(!t.text().contains("Sign in to krowk"), "{:?}", t.text());
    assert!(b.keystore().device_list().unwrap().is_some());
    assert_eq!(b.state()["pending"], false);
}

/// A sign-in that fails is said, and nothing after it is asked: the next
/// `krowk` asks to sign in again rather than going on without an account.
#[test]
fn a_failed_sign_in_is_asked_again_and_nothing_after_it_is() {
    let (r, api) = registry(Default::default());
    let b = Sandbox::new("denied");
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Sign in to krowk");
    types(&mut t, b"\r");
    decides(&r, &t, "denial");
    says(&t, "run krowk again to retry");
    says(&t, "krowk help agents");
    ends(&mut t);
    assert!(!t.text().contains("Connect which provider?"), "{:?}", t.text());
    let mut t = wide(b.command(&api, &[]));
    says(&t, "Sign in to krowk");
    types(&mut t, b"x\r");
    ends(&mut t);
}

/// D9, R-EASE-8: the overview bare `krowk` prints names the most important
/// problem with the setup and its fix; a machine with nothing wrong says
/// nothing. Read from the disk, so it is there piped too.
#[test]
fn the_overview_shows_the_top_problem_and_nothing_when_nothing_is_wrong() {
    let (_r, api) = registry(Default::default());
    // Signed in with a key, and never registered for sync.
    let b = Sandbox::new("top-problem");
    let login = b.command(&api, &["login", "--token", "krowk_sk_top#a"]).output().unwrap();
    assert!(login.status.success(), "{}", String::from_utf8_lossy(&login.stderr));
    let out = b.command(&api, &[]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && said.contains("This machine is not registered for sync — krowk login"), "{said}");
    // Not in `krowk help`, which is the same page without the setup's state.
    let out = b.command(&api, &["help"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("not registered"));

    // Using krowk locally, with a model: nothing is wrong, so nothing is said.
    let b = Sandbox::new("no-problem");
    std::fs::create_dir_all(b.root.join("home/.krowk")).unwrap();
    std::fs::write(b.root.join("home/.krowk/first-run.json"), "{\"local\": true, \"always_on_asked\": true}\n").unwrap();
    b.claude_signed_in();
    let out = b.command(&api, &[]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && said.contains("krowk help agents"), "{said}");
    assert!(!said.contains("not registered"), "{said}");
    assert!(!said.contains(REMINDER), "{said}");
}
