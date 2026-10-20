//! The fake fleet, as a process: a backend for an app's tests and the App
//! Review demo, on localhost, never api.krowk.com (R-IOS-15) — the stand-in
//! registry, the reference relay, a mock Anthropic model, a host daemon and
//! a sync bridge for one session:
//!
//! ```text
//! cargo run -p krowk-harness --example fake_fleet -- --demo
//! api=http://127.0.0.1:53011/v1
//! relay=ws://127.0.0.1:53012
//! env=development
//! approve=http://127.0.0.1:53011/_approve/cli/authorizations/{code}/approval
//! session=0199c0de-…
//! approval=toolu_01Touch
//! request=0199c0de-…
//! ready
//! ```
//!
//! A test reads lines until `ready`, points the app at `api` and `relay`,
//! approves the app's browser sign-in by POSTing to `approve` with the code
//! the app shows (the login lands in the host's person, so it sees the
//! session), and closes the fleet's stdin to stop it: the bridge stops and
//! the fleet exits 0. A line `stop-host` on stdin takes the host away
//! first — the bridge stops, the registry and the relay stay — so the app
//! can be seen read-only (R-HAND-4).
//!
//! The session has a first turn ("Say hello" and a short answer). `--demo`,
//! for screenshots and App Review, gives it a readable title and a second
//! turn, typed at the host and held on an approval for the model's `bash`
//! call `approval=`; `request=` is the request id a viewer's `approve`
//! command names to answer it.
//! Everything binds 127.0.0.1 and lives under `/tmp/krowk-fleet-<pid>`,
//! removed when stdin closes but not if the fleet is killed. Logs go to
//! stderr: read or discard it, so a full pipe never blocks the fleet.

#[cfg(unix)]
#[path = "../tests/common/fleet.rs"]
mod fleet;
#[cfg(unix)]
#[path = "../tests/common/mock.rs"]
mod mock;
#[cfg(unix)]
#[path = "../tests/common/scratch.rs"]
mod scratch;

#[cfg(unix)]
fn main() {
    use std::io::{BufRead, Write};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let demo = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => false,
        ["--demo"] => true,
        _ => {
            eprintln!("fake_fleet: unknown arguments {args:?}; the only one is --demo");
            std::process::exit(2);
        }
    };
    let fleet = fleet::start(demo);
    let mut out = std::io::stdout().lock();
    for l in fleet.lines() {
        writeln!(out, "{l}").unwrap();
    }
    writeln!(out, "ready").unwrap();
    out.flush().unwrap();
    drop(out);
    for line in std::io::stdin().lock().lines() {
        match line.as_deref().map(str::trim) {
            Ok("stop-host") => fleet.stop_host(),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    fleet.stop();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("the fake fleet needs a unix host daemon");
    std::process::exit(1);
}
