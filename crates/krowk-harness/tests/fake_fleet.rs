//! The fake fleet (Harness P4 ticket 75), in this process: what it prints,
//! a second device of the person's attaching to its session, a browser
//! sign-in landing in the host's person, and the host taken away; and, as
//! a process, a phone pairing with it (ticket 76).

#![cfg(unix)]

#[path = "common/fleet.rs"]
mod fleet;
#[path = "common/mock.rs"]
mod mock;
#[path = "common/scratch.rs"]
mod scratch;

use krowk_api::types::LoginAction;
use krowk_client::device_chain::{Chain, Change, Kind, SignedEntry};
use krowk_client::e2e;
use krowk_client::user_key::UserKeys;
use krowk_harness::protocol::{ApprovalDecision, Command as Cmd, LiveEvent, StreamLine};
use krowk_harness::sync::viewer;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The demo's second prompt.
const MAKE: &str = "Please make the file approved.txt";

/// Reads updates until `done` says so, or panics after `wait`.
async fn until(v: &mut viewer::Viewer, wait: Duration, mut done: impl FnMut(&[viewer::Update]) -> bool) -> Vec<viewer::Update> {
    let deadline = Instant::now() + wait;
    let mut all = Vec::new();
    loop {
        let left = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| panic!("timed out; saw {all:#?}"));
        let batch = tokio::time::timeout(left, v.updates.recv()).await.unwrap_or_else(|_| panic!("timed out; saw {all:#?}")).expect("the viewer runs");
        all.extend(batch);
        if done(&all) {
            return all;
        }
    }
}

/// POSTs to `url`, `http://host:port/path`, with no body; the status line.
fn post(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_at(rest.find('/').unwrap());
    let mut s = std::net::TcpStream::connect(host).unwrap();
    write!(s, "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out.lines().next().unwrap_or_default().to_string()
}

/// R-IOS-15, R-RELAY-1, R-HAND-4: an app's tests run against the fake
/// fleet — the stand-in registry, the reference relay as the relay, a host
/// daemon and a mock model, on localhost — never api.krowk.com. It prints
/// what a test needs within 15 s; a second device of the person's attaches
/// to its session and sees the demo's transcript and its held approval; a
/// browser sign-in approved at `approve` lands in the host's person; B's
/// answer ends the held turn; and with the fleet's host stopped — its
/// registry and relay still up — the viewer is told no host is there.
#[test]
fn r_ios_15_a_second_device_attaches_to_the_fake_fleet_and_sees_the_demo_and_its_held_approval() {
    let started = Instant::now();
    let f = fleet::start(true);
    assert!(started.elapsed() < Duration::from_secs(15), "the fleet took {:?}", started.elapsed());
    let lines = f.lines();
    let keys: Vec<&str> = lines.iter().map(|l| l.split_once('=').unwrap().0).collect();
    assert_eq!(keys, ["api", "relay", "env", "approve", "session", "approval", "request"]);
    assert!(f.api.starts_with("http://127.0.0.1:") && f.api.ends_with("/v1"), "{}", f.api);
    assert!(f.relay.starts_with("ws://127.0.0.1:"), "{}", f.relay);
    assert_eq!(f.env, "development");
    assert!(f.approve.starts_with("http://127.0.0.1:") && f.approve.ends_with("/_approve/cli/authorizations/{code}/approval"), "{}", f.approve);
    assert_eq!(f.approval.as_deref(), Some(fleet::APPROVAL));

    // A browser sign-in, as an app makes it, lands in host A's person.
    let anon = krowk_api::Client::new(&f.api, "");
    let auth = anon.start_cli_authorization(LoginAction::Login, "iphone").unwrap();
    assert_eq!(post(&f.approve.replace("{code}", &auth.code)), "HTTP/1.1 200 OK");
    let token = anon.read_cli_authorization(&auth.slug).unwrap().token;
    assert!(!token.is_empty());
    let phone = krowk_api::Client::new(&f.api, &token).verify_key().unwrap().user_id;
    let host = krowk_api::Client::new(&f.api, &f.host.token).verify_key().unwrap().user_id;
    assert_eq!(phone, host, "the login is host A's person");

    // B joins the person's device list, added by A, as `sync join` does.
    let b = fleet::Device::new("machine-b");
    let (chain, batch) = f.chain.clone().batch(&f.user, vec![Change::Add(b.subject("machine-b"))], f.host.key.id(), &f.host.signing, fleet::T0 + 1).unwrap();
    f.host.client(&f.api).append_device_list(&fleet::post(&batch)).unwrap();
    b.client(&f.api).claim_key_device().unwrap();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let o = viewer::Options { relay: f.relay.clone(), env: f.env.clone(), api: b.client(&f.api), device: b.key.id(), signing: b.signing_copy(), keys: UserKeys::new(f.user.clone(), []).unwrap(), chain, session: f.session.clone(), known: None };
    let request = f.request.clone().unwrap();
    let mut v = rt.block_on(viewer::attach(o)).unwrap();
    let got = rt.block_on(until(&mut v, Duration::from_secs(10), |all| {
        let host = all.iter().any(|u| matches!(u, viewer::Update::Host(true)));
        host && format!("{all:?}").contains(MAKE) && all.iter().any(|u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) if r.request_id == request))
    }));
    // The first turn from the checkpoint; the second as the host catches
    // the viewer up past it.
    assert!(matches!(got[0], viewer::Update::Attached { .. }), "{got:?}");
    let transcript = format!("{got:?}");
    for said in ["Say hello", "Hello! I can read this project", MAKE] {
        assert!(transcript.contains(said), "the demo's transcript holds {said:?}: {transcript}");
    }
    let held = got.iter().find_map(|u| if let viewer::Update::Line(StreamLine::Live(LiveEvent::ApprovalRequested(r))) = u { Some(r.clone()) } else { None }).unwrap();
    assert_eq!((held.tool.as_str(), &held.input), ("bash", &serde_json::json!({"command": "touch approved.txt"})));
    assert!(!f.root.join("repo/approved.txt").exists(), "the approval is held");

    // Answered from B, as App Review would tap it, the demo's turn ends.
    v.commands.send(Cmd::Approve { session_id: held.session_id.clone(), request_id: held.request_id.clone(), decision: ApprovalDecision::Allow, answers: Vec::new() }).unwrap();
    let got = rt.block_on(until(&mut v, Duration::from_secs(10), |all| all.iter().any(|u| matches!(u, viewer::Update::Line(StreamLine::Live(LiveEvent::Result(_)))))));
    assert!(format!("{got:?}").contains("Done: approved.txt is in the project."), "{got:?}");
    assert!(f.root.join("repo/approved.txt").exists(), "B's answer ran the call");

    // R-HAND-4: stopping the fleet's bridge takes the host away, the
    // registry and the relay still up.
    f.stop_host();
    rt.block_on(until(&mut v, Duration::from_secs(10), |all| all.iter().any(|u| matches!(u, viewer::Update::Host(false)))));
    f.stop();
    assert!(started.elapsed() < Duration::from_secs(15), "the whole check took {:?}", started.elapsed());
}

// ------------------------------------------------- the process, pairing (ticket 76)

/// A binary cargo built beside this test's, `target/<profile>/<rel>`: the
/// example and the full debug `krowk` a workspace test build makes.
fn built(rel: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().and_then(Path::parent).unwrap().join(rel);
    assert!(path.exists(), "{} is not built: `cargo build -p krowk --features harness` and `cargo build -p krowk-harness --example fake_fleet` (make test builds both)", path.display());
    path
}

/// The fleet as an app's test runs it, its stdout read up to `ready`.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    out: Receiver<String>,
    /// Its stderr, every line, as it comes.
    err: Arc<Mutex<String>>,
    lines: HashMap<String, String>,
}

impl Proc {
    fn start(args: &[&str]) -> Proc {
        // A group of its own, so a failed test takes its `devices add` too.
        let mut child = Command::new(built("examples/fake_fleet")).args(args).env("KROWK_BIN", built("krowk")).process_group(0).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let (tx, out) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(l);
            }
        });
        let err = Arc::new(Mutex::new(String::new()));
        let (stderr, kept) = (child.stderr.take().unwrap(), err.clone());
        std::thread::spawn(move || {
            for l in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut k = kept.lock().unwrap();
                k.push_str(&l);
                k.push('\n');
            }
        });
        let mut p = Proc { stdin: child.stdin.take(), child, out, err, lines: HashMap::new() };
        loop {
            let l = p.line();
            if l == "ready" {
                return p;
            }
            let (k, v) = l.split_once('=').unwrap_or_else(|| panic!("not key=value: {l}"));
            p.lines.insert(k.into(), v.into());
        }
    }

    fn line(&self) -> String {
        self.out.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("the fleet printed nothing more; its stderr:\n{}", self.err.lock().unwrap()))
    }

    fn get(&self, k: &str) -> &str {
        self.lines.get(k).unwrap_or_else(|| panic!("no {k}= in {:?}", self.lines))
    }

    fn send(&mut self, line: &str) {
        writeln!(self.stdin.as_mut().unwrap(), "{line}").unwrap();
    }

    /// Waits until the fleet has logged a `devices add` child ending, with
    /// exit status 0 when `ok`, else with any other.
    fn ended(&self, ok: bool) {
        const ENDED: &str = "fleet: krowk devices add ended: ";
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.err.lock().unwrap().lines().filter_map(|l| l.strip_prefix(ENDED)).any(|s| (s == "exit status: 0") == ok) {
            assert!(Instant::now() < deadline, "no `devices add` ended {}; stderr:\n{}", if ok { "0" } else { "failing" }, self.err.lock().unwrap());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Closes stdin, as a test does when it is done: the fleet exits 0.
    fn stop(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the fleet did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(self.child.wait().unwrap().success(), "{}", self.err.lock().unwrap());
    }
}

/// A phone, as `krowk sync join` plays one: its own home, signed in through
/// the fleet's `approve` URL as the app signs in.
struct Phone {
    home: std::path::PathBuf,
    token: String,
}

impl Phone {
    fn sign_in(f: &Proc, name: &str) -> Phone {
        let anon = krowk_api::Client::new(f.get("api"), "");
        let auth = anon.start_cli_authorization(LoginAction::Login, name).unwrap();
        assert_eq!(post(&f.get("approve").replace("{code}", &auth.code)), "HTTP/1.1 200 OK");
        let token = anon.read_cli_authorization(&auth.slug).unwrap().token;
        let home = scratch::root(&format!("fleet-{name}"));
        Phone { home, token }
    }

    /// `krowk sync join`, `typed` on its stdin.
    fn join(&self, f: &Proc, typed: &str) -> std::process::Output {
        let mut c = Command::new(built("krowk"))
            .args(["sync", "join", "--json"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("KROWK_HOME", self.home.join(".krowk"))
            .env("KROWK_API_URL", f.get("api"))
            .env("KROWK_TOKEN", &self.token)
            .env("KROWK_DEVICE_NAME", "phone")
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1")
            .current_dir(&self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(format!("{typed}\n").as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while c.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = c.kill();
                panic!("`krowk sync join` did not finish");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        c.wait_with_output().unwrap()
    }

    /// The names on the person's device list, as the registry serves it,
    /// verified from seq 0.
    fn devices(&self, f: &Proc) -> Vec<String> {
        let list = krowk_api::Client::new(f.get("api"), &self.token).device_list_all().unwrap();
        let entries: Vec<SignedEntry> = list.entries.iter().map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).unwrap(), &e2e::unhex(&e.signatures).unwrap()).unwrap()).collect();
        let chain = Chain::verify(&entries, None).unwrap();
        chain.devices().iter().filter(|d| d.kind == Kind::Device).map(|d| d.name.clone()).collect()
    }
}

/// A test that fails leaves no fleet behind: EOF, and if it has not gone
/// within a few seconds its whole group — the fleet and its `devices add`
/// — is killed.
impl Drop for Proc {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            // SAFETY: kill(2) on the group led by our own child, which is
            // not reaped yet.
            unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        }
        let _ = self.child.wait();
    }
}

impl Drop for Phone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn said(out: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// R-E2E-3, R-IOS-15: the fake fleet approves a phone through the real
/// `krowk devices add`. Started with `--pair`, it prints a code before
/// `ready`; a device signed in through its `approve` URL types it into
/// `krowk sync join` and joins; A's `devices add` exits 0; and the person's
/// device list holds host A and the phone.
#[test]
fn r_e2e_3_r_ios_15_a_phone_joins_the_fake_fleet_with_the_code_its_krowk_devices_add_shows() {
    let f = Proc::start(&["--pair"]);
    let code = f.get("pair").to_string();
    assert!(code.len() == 9 && code.as_bytes()[4] == b'-', "{code}");
    let phone = Phone::sign_in(&f, "phone");
    let out = phone.join(&f, &code);
    assert!(out.status.success(), "{}\nfleet:\n{}", said(&out), f.err.lock().unwrap());
    f.ended(true);
    assert_eq!(phone.devices(&f), ["host-a", "phone"]);
    f.stop();
}

/// R-E2E-3: a code works once. After a wrong one has ended the pairing, a
/// `pair` line on the fleet's stdin gives a new, different code, and the
/// phone joins with it.
#[test]
fn r_e2e_3_after_a_wrong_code_a_pair_line_gives_the_fake_fleet_a_fresh_code_that_joins() {
    let mut f = Proc::start(&["--pair"]);
    let first = f.get("pair").to_string();
    let phone = Phone::sign_in(&f, "phone-again");
    let wrong = if first.starts_with('2') { "3" } else { "2" }.to_string() + &first[1..];
    let out = phone.join(&f, &wrong);
    assert!(!out.status.success(), "a wrong code joined: {}", said(&out));
    // The wrong code ended A's pairing: its `devices add` failed.
    f.ended(false);
    f.send("pair");
    let second = f.line();
    let second = second.strip_prefix("pair=").unwrap_or_else(|| panic!("not a pair= line: {second}")).to_string();
    assert_ne!(second, first, "a fresh code");
    let out = phone.join(&f, &second);
    assert!(out.status.success(), "{}\nfleet:\n{}", said(&out), f.err.lock().unwrap());
    f.ended(true);
    assert_eq!(phone.devices(&f), ["host-a", "phone"]);
    f.stop();
}

/// R-IOS-15: without `KROWK_BIN` the fleet starts nothing, and says which
/// variable and how to build the binary it names.
#[test]
fn r_ios_15_the_fake_fleet_refuses_to_start_without_krowk_bin() {
    let out = Command::new(built("examples/fake_fleet")).env_remove("KROWK_BIN").stdin(Stdio::null()).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(out.stdout.is_empty(), "printed {:?}", String::from_utf8_lossy(&out.stdout));
    assert!(err.contains("KROWK_BIN") && err.contains("cargo build -p krowk --features harness"), "{err}");
}
