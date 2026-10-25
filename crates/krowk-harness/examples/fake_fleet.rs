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
//! devices_url=http://127.0.0.1:53013/devices
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
//! its lease is gone; `sessions_url/<id>/host` hosts it again (a host coming
//! back, R-HAND-4) and replies `hosted` once it is leased. A POST to `sessions_url/<id>/turn` types a prompt (the
//! request's body, else "Please stream a long answer") at host A in that
//! session and replies `started`: on "stream a long answer" the model makes
//! a `read` call, then streams 120 numbered lines at 200 tokens a second, a
//! turn a phone's tests follow live (ticket 89); "stream a very long
//! answer", 300 lines. A POST to `sessions_url/<id>/turn-ended` replies
//! `ended` once every turn typed there has ended at host A, or `running`
//! if one still runs after 5 s (the control is served one request at a
//! time, so it is never held longer): a starved CI runner streams far
//! slower than 200 tokens a second, so a test asks until the host is done,
//! then times only how soon the phone shows the end.
//!
//! A test that removes its phone from host A's devices (R-E2E-3) POSTs the
//! phone's name to `devices_url/remove`: the fleet runs `krowk devices
//! remove <name>` as host A, saying yes, and replies `removed` once the
//! list no longer names it (its keys refused, the user key rotated to the
//! devices left). The running `devices add` is ended first, so the next
//! pairing asks `pair_url` for a fresh code.
//!
//! With `--direct-lan` every session's bridge offers a direct path too: its
//! LAN listener on 127.0.0.1, named in each welcome as a `lan` candidate
//! (`KROWK_DIRECT_LAN` with no Tailscale), so a phone in the simulator
//! races it and moves onto it (R-NET-2, ticket 133). A POST to
//! `sessions_url/<id>/direct/close` closes that session's listener, as a
//! LAN path lost would: its viewers go back to the relay.
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
    let (mut demo, mut pair, mut direct_lan) = (false, false, false);
    for a in &args {
        match a.as_str() {
            "--demo" => demo = true,
            "--pair" => pair = true,
            "--direct-lan" => direct_lan = true,
            _ => {
                eprintln!("fake_fleet: unknown arguments {args:?}; the only ones are --demo, --pair and --direct-lan");
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
    let fleet = fleet::start_with(demo, direct_lan);
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
    writeln!(out, "devices_url={}/devices", control.url).unwrap();
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
/// any, its title), `POST /sessions/<id>/release` its lease let go,
/// `POST /sessions/<id>/host` hosted again, `POST /sessions/<id>/turn`
/// a turn typed at host A in it, `POST /sessions/<id>/turn-ended`:
/// `ended` once those turns have ended, or `running` after 5 s,
/// `POST /sessions/<id>/direct/close` its direct listener closed (with
/// `--direct-lan`), and `POST /devices/remove` the device the body names
/// removed from host A's list.
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
    let turn = seen.path.strip_prefix("/sessions/").and_then(|r| r.strip_suffix("/turn")).filter(|id| !id.is_empty() && !id.contains('/'));
    let again = seen.path.strip_prefix("/sessions/").and_then(|r| r.strip_suffix("/host")).filter(|id| !id.is_empty() && !id.contains('/'));
    let ended = seen.path.strip_prefix("/sessions/").and_then(|r| r.strip_suffix("/turn-ended")).filter(|id| !id.is_empty() && !id.contains('/'));
    let direct = seen.path.strip_prefix("/sessions/").and_then(|r| r.strip_suffix("/direct/close")).filter(|id| !id.is_empty() && !id.contains('/'));
    if let ("POST", Some(id)) = (seen.method.as_str(), direct) {
        return answered(&format!("POST {}", seen.path), hosting.close_direct(id).map(|()| "closed".into()));
    }
    if let ("POST", Some(id)) = (seen.method.as_str(), ended) {
        let ended = hosting.turn_ended(id, std::time::Duration::from_secs(5));
        return answered(&format!("POST {}", seen.path), ended.map(|done| if done { "ended" } else { "running" }.into()));
    }
    if let ("POST", Some(id)) = (seen.method.as_str(), again) {
        return answered(&format!("POST {}", seen.path), hosting.host_again(id).map(|()| "hosted".into()));
    }
    if let ("POST", Some(id)) = (seen.method.as_str(), turn) {
        let prompt = seen.raw.trim();
        return answered(&format!("POST {}", seen.path), hosting.turn(id, if prompt.is_empty() { fleet::LONG } else { prompt }).map(|()| "started".into()));
    }
    match (seen.method.as_str(), seen.path.as_str(), release) {
        ("POST", "/pair", _) if pair => answered("POST /pair", pairer.lock().unwrap().pair()),
        ("POST", "/devices/remove", _) => {
            let name = seen.raw.trim();
            if name.is_empty() {
                return text(400, "POST /devices/remove names the device to remove in its body");
            }
            answered("POST /devices/remove", pairer.lock().unwrap().remove(name).map(|()| "removed".into()))
        }
        ("POST", "/sessions", _) => {
            let title = seen.raw.trim();
            answered("POST /sessions", hosting.host(if title.is_empty() { "a test's session" } else { title }))
        }
        ("POST", _, Some(id)) if !id.is_empty() && !id.contains('/') => answered(&format!("POST {}", seen.path), hosting.release(id).map(|()| "released".into())),
        _ => text(404, "only POST /pair, POST /sessions, POST /sessions/<id>/release, POST /sessions/<id>/host, POST /sessions/<id>/turn, POST /sessions/<id>/turn-ended, POST /sessions/<id>/direct/close and POST /devices/remove"),
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
