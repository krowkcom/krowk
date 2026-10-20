//! The fake fleet (Harness P4 ticket 75), in this process: what it prints,
//! a second device of the person's attaching to its session, a browser
//! sign-in landing in the host's person, and the host taken away.

#![cfg(unix)]

#[path = "common/fleet.rs"]
mod fleet;
#[path = "common/mock.rs"]
mod mock;
#[path = "common/scratch.rs"]
mod scratch;

use krowk_api::types::LoginAction;
use krowk_client::device_chain::Change;
use krowk_client::user_key::UserKeys;
use krowk_harness::protocol::{LiveEvent, StreamLine};
use krowk_harness::sync::viewer;
use std::io::{Read, Write};
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
/// browser sign-in approved at `approve` lands in the host's person; and
/// with the fleet stopped the host is gone and the viewer is read-only.
#[test]
fn r_ios_15_a_second_device_attaches_to_the_fake_fleet_and_sees_the_demo_and_its_held_approval() {
    let started = Instant::now();
    // Its own thread: the fleet runs runtimes of its own.
    let f = std::thread::spawn(|| fleet::start(true)).join().unwrap();
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

    // R-HAND-4: stopping the fleet's bridge takes the host away.
    f.stop();
    rt.block_on(until(&mut v, Duration::from_secs(10), |all| all.iter().any(|u| matches!(u, viewer::Update::Host(false)))));
    assert!(started.elapsed() < Duration::from_secs(15), "the whole check took {:?}", started.elapsed());
}
