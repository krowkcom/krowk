//! The fake fleet, as a process: a backend for an app's tests and the App
//! Review demo, on localhost, never api.krowk.com (R-IOS-15) — the stand-in
//! registry, the reference relay, a mock Anthropic model, a host daemon and
//! a sync bridge for one session:
//!
//! ```text
//! cargo build -p krowk --features harness
//! KROWK_BIN=target/debug/krowk cargo run -p krowk-harness --example fake_fleet -- --demo
//! api=http://127.0.0.1:53011/v1
//! relay=ws://127.0.0.1:53012
//! env=development
//! approve=http://127.0.0.1:53011/_approve/cli/authorizations/{code}/approval
//! session=0199c0de-…
//! approval=toolu_01Touch
//! request=0199c0de-…
//! pair=7KQ2-M9XD
//! pair_url=http://127.0.0.1:53013/pair
//! sessions_url=http://127.0.0.1:53013/sessions
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
//! A phone joins host A's device list with the code on `pair=` (R-E2E-3):
//! with `--pair`, which `--demo` implies, the fleet runs `krowk devices add`
//! as host A — the real CLI, from a debug build at `KROWK_BIN`, its keys in
//! the fleet's home, saying yes to the device that joins — and prints its
//! code before `ready`. A code works once: a line `pair` on stdin ends the
//! running `devices add` and prints a fresh `pair=` line. A test that
//! cannot write to the fleet's stdin (an iPhone simulator's, which only
//! shares the Mac's loopback) POSTs to `pair_url` instead: the same fresh
//! `devices add`, its code the reply's body, text. Without `KROWK_BIN` the
//! fleet exits 1 saying how to build one.
//!
//! A test that wants a session's lease to lapse (R-IOS-7) without taking
//! the fleet's host from every other test POSTs to `sessions_url`: host A
//! hosts a new session (titled by the request's body, if any) and replies
//! with its id once the registry shows it leased; a POST to
//! `sessions_url/<id>/release` stops that session's bridge and replies once
//! its lease is gone.
//!
//! The session has a first turn ("Say hello" and a short answer). `--demo`,
//! for screenshots and App Review, gives it a readable title, a second turn
//! with a tool call that runs at once (a `read` of the project's README),
//! its result and an answer, and a third turn, typed at the host and held
//! on an approval for the model's `bash` call `approval=`; `request=` is
//! the request id a viewer's `approve` command names to answer it.
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

/// What the fleet says when it has no `krowk` to pair with.
#[cfg(unix)]
const NO_BIN: &str = "fake_fleet: set KROWK_BIN to a debug krowk, which answers the phone's pairing as host A: build it with `cargo build -p krowk --features harness` (never --release) and pass KROWK_BIN=target/debug/krowk";

#[cfg(unix)]
fn main() {
    use std::io::{BufRead, Write};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut demo, mut pair) = (false, false);
    for a in &args {
        match a.as_str() {
            "--demo" => demo = true,
            "--pair" => pair = true,
            _ => {
                eprintln!("fake_fleet: unknown arguments {args:?}; the only ones are --demo and --pair");
                std::process::exit(2);
            }
        }
    }
    pair |= demo;
    // Made absolute here: the fleet runs it in host A's home.
    let bin = std::env::var_os("KROWK_BIN").filter(|b| !b.is_empty()).unwrap_or_else(|| {
        eprintln!("{NO_BIN}");
        std::process::exit(1);
    });
    let bin = std::fs::canonicalize(&bin).unwrap_or_else(|e| {
        eprintln!("fake_fleet: KROWK_BIN={} cannot be found ({e})\n{NO_BIN}", bin.to_string_lossy());
        std::process::exit(1);
    });
    let fleet = fleet::start(demo);
    let pairer = match fleet::Pairer::new(&fleet, &bin) {
        Ok(p) => std::sync::Arc::new(std::sync::Mutex::new(p)),
        Err(e) => fail(None, fleet, &e),
    };
    let mut out = std::io::stdout().lock();
    for l in fleet.lines() {
        writeln!(out, "{l}").unwrap();
    }
    if pair {
        let code = pairer.lock().unwrap().pair();
        match code {
            Ok(code) => writeln!(out, "pair={code}").unwrap(),
            Err(e) => fail(Some(&pairer), fleet, &e),
        }
    }
    // Loopback only, like the rest of the fleet.
    let (asked, hosting) = (std::sync::Arc::clone(&pairer), std::sync::Arc::clone(&fleet.hosting));
    let control = mock::serve_seen(move |seen, _| control(seen, pair, &asked, &hosting));
    if pair {
        writeln!(out, "pair_url={}/pair", control.url).unwrap();
    }
    writeln!(out, "sessions_url={}/sessions", control.url).unwrap();
    writeln!(out, "ready").unwrap();
    out.flush().unwrap();
    drop(out);
    for line in std::io::stdin().lock().lines() {
        match line.as_deref().map(str::trim) {
            Ok("stop-host") => fleet.stop_host(),
            Ok("pair") => {
                let code = pairer.lock().unwrap().pair();
                let code = match code {
                    Ok(code) => code,
                    Err(e) => fail(Some(&pairer), fleet, &e),
                };
                let mut out = std::io::stdout().lock();
                writeln!(out, "pair={code}").unwrap();
                out.flush().unwrap();
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    pairer.lock().unwrap().end();
    fleet.stop();
}

/// The fleet's control, for a test that cannot write to its stdin:
/// `POST /pair` a fresh code (with `--pair`), `POST /sessions` a session of
/// the test's own, hosted, its id the reply's body (the request's body, if
/// any, its title), and `POST /sessions/<id>/release` its lease let go.
#[cfg(unix)]
fn control(seen: &mock::Seen, pair: bool, pairer: &std::sync::Mutex<fleet::Pairer>, hosting: &fleet::Hosting) -> mock::Reply {
    let text = |status: u16, body: &str| mock::Reply { content_type: Some("text/plain"), ..mock::Reply::status(status, body) };
    let answered = |what: &str, r: Result<String, String>| match r {
        Ok(body) => text(200, &body),
        Err(e) => {
            eprintln!("fake_fleet: {what}: {e}");
            text(500, &e)
        }
    };
    let release = seen.path.strip_prefix("/sessions/").and_then(|r| r.strip_suffix("/release"));
    match (seen.method.as_str(), seen.path.as_str(), release) {
        ("POST", "/pair", _) if pair => answered("POST /pair", pairer.lock().unwrap().pair()),
        ("POST", "/sessions", _) => {
            let title = seen.raw.trim();
            answered("POST /sessions", hosting.host(if title.is_empty() { "a test's session" } else { title }))
        }
        ("POST", _, Some(id)) if !id.is_empty() && !id.contains('/') => answered(&format!("POST {}", seen.path), hosting.release(id).map(|()| "released".into())),
        _ => text(404, "only POST /pair, POST /sessions and POST /sessions/<id>/release"),
    }
}

/// Stops everything and exits 1, saying why: no `pair=` line can follow.
#[cfg(unix)]
fn fail(pairer: Option<&std::sync::Mutex<fleet::Pairer>>, fleet: fleet::Fleet, why: &str) -> ! {
    eprintln!("fake_fleet: {why}");
    if let Some(p) = pairer {
        p.lock().unwrap_or_else(std::sync::PoisonError::into_inner).end();
    }
    fleet.stop();
    std::process::exit(1);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("the fake fleet needs a unix host daemon");
    std::process::exit(1);
}
