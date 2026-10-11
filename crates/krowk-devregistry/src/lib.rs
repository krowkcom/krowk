//! An in-memory stand-in for api.krowk.com, so the CLI can be tested and
//! demoed without Postgres, object storage or a Rails process.
//!
//! It implements the contract the real registry does — declare, upload,
//! finalize; runs; the claim flow; sync's device lists, user keys, pairings,
//! sessions and leases; one error envelope — including the parts
//! that exist to catch a broken client: it refuses a finalize for bytes that
//! never arrived, and bytes whose length or digest is not what was declared.
//! A client that passes against this one is exercising the real sequence.
//!
//! A port of the Go stand-in that lived at `internal/registry`: the golden
//! cases recorded against that one passed against this one unchanged.
//!
//! In-process use, from another crate's test:
//!
//! ```no_run
//! let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
//! let registry = krowk_devregistry::start(listener, krowk_devregistry::Config::default()).unwrap();
//! let base = format!("{}/v1", registry.url()); // stops when dropped
//! ```

mod artifacts;
mod auth;
mod conn;
mod devices;
mod encode;
mod errors;
mod http;
mod image;
mod json;
mod moves;
mod page;
mod pairings;
mod runs;
mod store;
mod sync;
pub use sync::{ticket_public_key, TICKET_KID, TICKET_SEED};
mod uploads;
mod view;
mod xml;

use encode::Json;
use http::{Req, Resp, clean_path, parseable, redirect, unescape};
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use store::App;

pub use store::{
    CLI_AUTHORIZATION_GRACE, CLI_AUTHORIZATION_LIFETIME, Clock, DEFAULT_LIMIT_BYTES, EPHEMERAL_LIFETIME,
    UPLOAD_URL_LIFETIME,
};

/// How a registry is started: the `--limit-bytes` and `--site` flags, and the
/// clock — Go's `HandlerWithClock`. `limit_bytes <= 0` is the default limit;
/// an empty `site` means links name whatever host a request arrived on.
#[derive(Default, Clone)]
pub struct Config {
    pub limit_bytes: i64,
    pub site: String,
    pub clock: Option<Clock>,
    /// The most synced sessions a workspace holds; 0 is the registry's
    /// 10,000.
    pub max_sessions: usize,
}

/// A registry serving on its own thread, stopped when dropped.
pub struct Running {
    addr: SocketAddr,
    open: Arc<conn::Open>,
    thread: Option<JoinHandle<io::Error>>,
}

impl Running {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The origin, `http://127.0.0.1:{port}` — the API is under `/v1`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stops serving and says why the loop ended, as Go's `Serve` returns once
    /// its listener closes.
    pub fn stop(mut self) -> io::Error {
        self.open.stop(self.addr);
        self.thread.take().unwrap().join().unwrap_or_else(|_| io::Error::other("the registry thread panicked"))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.open.stop(self.addr);
            let _ = t.join();
        }
    }
}

fn app(config: Config) -> Arc<App> {
    let clock = config.clock.unwrap_or_else(|| Arc::new(jiff::Timestamp::now));
    let app = App::new(config.limit_bytes, &config.site, clock);
    app.lock().sync.max_sessions = config.max_sessions;
    Arc::new(app)
}

/// A thread a connection (conn.rs says why), each request on it routed.
fn handler(app: Arc<App>) -> Arc<conn::Handler> {
    Arc::new(move |head: &conn::Head, body: &mut dyn Read| respond(&app, head, body))
}

/// Serves on an already-bound listener from a background thread.
pub fn start(listener: TcpListener, config: Config) -> io::Result<Running> {
    spawn(listener, app(config))
}

fn spawn(listener: TcpListener, app: Arc<App>) -> io::Result<Running> {
    let addr = listener.local_addr()?;
    let open = Arc::new(conn::Open::default());
    let o = Arc::clone(&open);
    let thread = thread::spawn(move || conn::accept(&listener, &o, &handler(app)));
    Ok(Running { addr, open, thread: Some(thread) })
}

/// Serves on an already-bound listener until accepting fails, and returns why.
pub fn serve(listener: TcpListener, config: Config) -> io::Error {
    conn::accept(&listener, &Arc::new(conn::Open::default()), &handler(app(config)))
}

fn respond(app: &Arc<App>, rq: &conn::Head, body: &mut dyn Read) -> conn::Answer {
    let mut target = rq.target.clone();
    let headers = rq.headers.clone();
    let mut host = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("Host")).map(|h| h.1.clone()).unwrap_or_default();
    // An absolute-form target names the host itself, and Go prefers it.
    if let Some(rest) = target.strip_prefix("http://").or_else(|| target.strip_prefix("https://")) {
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        host = authority.to_owned();
        target = path.to_owned();
    }
    let (path, query) = target.split_once('?').map_or((target.clone(), String::new()), |(p, q)| (p.into(), q.into()));
    let mut req = Req { method: rq.method.clone(), path, query, headers, host, remote: rq.remote.ip().to_string(), body };
    let resp = route(app, &mut req);
    conn::Answer { status: resp.status, headers: resp.headers, body: resp.body, undo: resp.undo }
}

/// The origin baked into links: `--site` when given, else the request's host.
fn site(req: &Req, over: &str) -> String {
    if over.is_empty() { format!("http://{}", req.host) } else { over.trim_end_matches('/').to_owned() }
}

/// What Go's ServeMux would do with the request: the same patterns, the same
/// redirects for a path sent uncleaned, and GET routes answering HEAD. A path
/// that matches nothing — a known path with the wrong verb included, since a
/// catch-all is registered — is `no_such_endpoint`, not a 405.
fn route(app: &Arc<App>, req: &mut Req) -> Resp {
    if !parseable(&req.path, &req.query) {
        let mut r = Resp::new(400, "text/plain; charset=utf-8", "400 Bad Request");
        r.headers.push(("Connection", "close".to_owned()));
        return r;
    }
    let clean = clean_path(&req.path);
    let segs: Vec<String> = clean[1..].split('/').map(|s| unescape(s, false).unwrap_or_else(|| s.to_owned())).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let m = req.method.clone();
    let get = m == "GET" || m == "HEAD";
    if segs == ["_storage"] && (get || m == "PUT") {
        return redirect(&m, &format!("{clean}/"), &req.query);
    }
    if clean != req.path {
        return redirect(&m, &clean, &req.query);
    }
    // A single wildcard never matches the empty segment a trailing slash leaves.
    let trailing = segs.len() > 1 && segs.last() == Some(&"");
    let a = &**app;
    match (m.as_str(), segs.as_slice()) {
        (_, ["_storage", ..]) if get || m == "PUT" => {
            // Everything after the first segment, as sent, then unescaped whole.
            let rest = &clean[1..];
            let key = unescape(&rest[rest.find('/').map_or(rest.len(), |i| i + 1)..], false).unwrap_or_default();
            if get { uploads::get_object(a, &key) } else { uploads::put_object(a, req, &key) }
        }
        _ if trailing => no_such_endpoint(),
        (_, [""]) if get => {
            Resp::json(200, &Json::map([("service", Json::str("krowk-registry")), ("versions", Json::Arr(vec![Json::str("v1")]))]))
        }
        ("POST", ["v1", "artifacts"]) => artifacts::create(a, req, &site(req, &a.site)),
        (_, ["v1", "artifacts"]) if get => artifacts::list(a, req),
        (_, ["v1", "artifacts", slug]) if get => artifacts::show(a, req, slug),
        ("DELETE", ["v1", "artifacts", slug]) => artifacts::destroy(a, req, slug),
        ("POST", ["v1", "artifacts", slug, "upload"]) => uploads::presign(a, req, slug),
        ("PUT" | "PATCH", ["v1", "artifacts", slug, "finalization"]) => artifacts::finalize(a, req, slug),
        ("POST", ["v1", "artifacts", slug, "claim"]) => moves::claim(a, req, slug),
        ("PUT" | "PATCH", ["v1", "artifacts", slug, "run"]) => moves::attach_run(a, req, slug),
        ("PUT" | "PATCH", ["v1", "artifacts", slug, "visibility"]) => {
            let site = site(req, &a.site);
            moves::update_visibility(a, req, slug, &site)
        }
        (_, ["v1", "key"]) if get => auth::show_key(req),
        ("PUT" | "PATCH", ["v1", "key", "device"]) => devices::claim(a, req),
        // The approval screen is served by this process and nothing else, so its
        // link names the request's own host, never --site.
        ("POST", ["v1", "cli", "authorizations"]) => {
            let site = site(req, "");
            auth::create_cli_authorization(a, req, &site)
        }
        (_, ["v1", "cli", "authorizations", slug]) if get => auth::show_cli_authorization(app, slug),
        (_, ["_approve", "cli", "authorizations", "new"]) if get => auth::cli_authorization_page(a, req),
        ("POST", ["_approve", "cli", "authorizations", code, "approval"]) => auth::decide_cli_authorization(a, code, true),
        ("POST", ["_approve", "cli", "authorizations", code, "denial"]) => auth::decide_cli_authorization(a, code, false),
        ("POST", ["v1", "runs"]) => runs::create(a, req),
        (_, ["v1", "runs"]) if get => runs::list(a, req),
        (_, ["v1", "runs", slug]) if get => runs::show(a, req, slug),
        ("PUT" | "PATCH", ["v1", "runs", slug, "completion"]) => runs::finish(a, req, slug),
        (_, ["v1", "runs", slug, "artifacts"]) if get => runs::artifacts(a, req, slug),
        // The account-key design's calls, gone as the registry has them:
        // registering a device with the workspace, and the approval mailbox,
        // for any key. Its device listing has no route left at all.
        ("POST", ["v1", "devices"]) | (_, ["v1", "device_approvals", ..]) => auth::require_key(req).map_or_else(|r| r, |_| sync::sync_reset()),
        (_, ["v1", "users", user, rest @ ..]) => users_route(a, req, &m, user, rest),
        // Session reads are signed, as the registry has them: the key alone
        // reads nothing.
        (_, ["v1", "sessions"]) if get => sync::signed(a, req, |a, req, _| sync::list_sessions(a, req)),
        (_, ["v1", "sessions", id]) if get => sync::signed(a, req, |a, req, _| sync::show_session(a, req, id)),
        ("PUT" | "PATCH", ["v1", "sessions", id]) => sync::signed(a, req, |a, req, by| sync::put_session(a, req, id, by)),
        // Keyless: the ticket public keys are public, as the registry's are.
        (_, ["v1", "relay", "ticket_keys"]) if get => sync::ticket_keys(),
        (_, ["v1", "sessions", id, "relay_ticket"]) if get => sync::signed(a, req, |a, req, by| sync::viewer_ticket(a, req, id, by)),
        ("POST", ["v1", "sessions", id, "lease"]) => sync::signed(a, req, |a, req, by| sync::acquire_lease(a, req, id, by)),
        ("PUT" | "PATCH", ["v1", "sessions", id, "lease"]) => sync::signed(a, req, |a, req, by| sync::renew_lease(a, req, id, by)),
        ("DELETE", ["v1", "sessions", id, "lease"]) => sync::signed(a, req, |a, req, by| sync::release_lease(a, req, id, by)),
        ("POST", ["v1", "sessions", id, "chunks"]) => {
            let site = site(req, &a.site);
            sync::signed(a, req, |a, req, by| sync::declare_chunk(a, req, id, &site, by))
        }
        (_, ["v1", "sessions", id, "chunks"]) if get => sync::signed(a, req, |a, req, _| sync::list_chunks(a, req, id, &site(req, &a.site))),
        ("PUT" | "PATCH", ["v1", "sessions", id, "chunks", index, "finalization"]) => sync::signed(a, req, |a, req, by| sync::finalize_chunk(a, req, id, index, by)),
        ("POST", ["v1", "vintages"]) => {
            let site = site(req, &a.site);
            sync::signed(a, req, |a, req, by| sync::declare_vintage(a, req, &site, by))
        }
        (_, ["v1", "vintages"]) if get => sync::list_vintages(a, req, &site(req, &a.site)),
        ("PUT" | "PATCH", ["v1", "vintages", slug, "finalization"]) => sync::signed(a, req, |a, req, by| sync::finalize_vintage(a, req, slug, by)),
        ("POST", ["v1", "vintages", slug, "restoration"]) => sync::signed(a, req, |a, req, by| sync::restore_vintage(a, req, slug, by)),
        ("POST", ["v1", "sessions", id, "transports"]) => {
            let site = site(req, &a.site);
            sync::signed(a, req, |a, req, by| sync::declare_transport(a, req, id, &site, by))
        }
        ("PUT" | "PATCH", ["v1", "transports", slug, "finalization"]) => sync::signed(a, req, |a, req, by| sync::finalize_transport(a, req, slug, by)),
        (_, ["v1", "transports", slug]) if get => {
            let site = site(req, &a.site);
            sync::signed(a, req, |a, req, by| sync::show_transport(a, req, slug, &site, by))
        }
        ("DELETE", ["v1", "transports", slug]) => sync::signed(a, req, |a, req, by| sync::spend_transport(a, req, slug, by)),
        ("POST", ["_settings", "devices", id, "revocation"]) => sync::revoke(a, req, id),
        (_, ["a", slug]) if get => page::artifact_page(a, req, slug),
        _ => no_such_endpoint(),
    }
}

/// `/v1/users/:user_id/…`: a person's device list, the user key wrapped to
/// each device, and their one pairing. `:user_id` is the key's own person
/// (`GET /v1/key`'s `user_id`); any other is not there to be found.
fn users_route(a: &App, req: &mut Req, m: &str, user: &str, rest: &[&str]) -> Resp {
    if auth::token(req).is_some_and(|t| store::person_for(&t) != user) {
        return errors::not_found();
    }
    let get = m == "GET" || m == "HEAD";
    match (m, rest) {
        (_, ["devices"]) if get => devices::show(a, req),
        ("POST", ["devices"]) => devices::post_devices(a, req),
        ("POST", ["devices", "reset"]) => devices::create(a, req, true),
        (_, ["devices", id, "key"]) if get => devices::user_key(a, req, id),
        ("POST", ["pairing"]) => pairings::open(a, req),
        (_, ["pairing"]) if get => pairings::read(a, req),
        ("DELETE", ["pairing"]) => pairings::destroy(a, req, &pairings::latest(a, req)),
        ("PUT" | "PATCH", ["pairing", step]) => {
            let (side, field) = match *step {
                "join" => (pairings::Side::Joiner, "joiner_message"),
                "answer" => (pairings::Side::Initiator, "initiator_message"),
                "confirmation" => (pairings::Side::Joiner, "joiner_confirmation"),
                "reply" => (pairings::Side::Initiator, "sealed_reply"),
                "acknowledgement" => (pairings::Side::Joiner, "joiner_ack"),
                _ => return no_such_endpoint(),
            };
            pairings::step(a, req, &pairings::latest(a, req), side, field)
        }
        _ => no_such_endpoint(),
    }
}

/// Not `not_found`: an unknown slug and an unknown path are different mistakes,
/// and telling them apart keeps someone from hunting a typo in a slug when the
/// base URL is wrong.
fn no_such_endpoint() -> Resp {
    errors::error(
        404,
        "no_such_endpoint",
        "No such endpoint. Check the path and the method — GET / lists the versions this API serves.",
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn call(addr: SocketAddr, method: &str, path: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(s, "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    fn field(text: &str, key: &str) -> String {
        let at = text.find(&format!("\"{key}\": \"")).unwrap() + key.len() + 5;
        text[at..].split('"').next().unwrap().to_owned()
    }

    /// One-shot means delivered once, not destroyed once: a response that never
    /// leaves hands nothing over, so the key stays collectable. The undo is the
    /// seam a failed write takes.
    #[test]
    fn a_browser_login_keeps_its_key_when_the_response_never_lands() {
        let app = app(Config::default());
        let running = spawn(TcpListener::bind("127.0.0.1:0").unwrap(), Arc::clone(&app)).unwrap();
        let opened = call(running.addr(), "POST", "/v1/cli/authorizations");
        let (slug, code) = (field(&opened, "slug"), field(&opened, "code"));
        assert!(call(running.addr(), "POST", &format!("/_approve/cli/authorizations/{code}/approval")).starts_with("HTTP/1.1 200"));

        let mut body: &[u8] = b"";
        let mut req = Req {
            method: "GET".into(),
            path: format!("/v1/cli/authorizations/{slug}"),
            query: String::new(),
            headers: vec![],
            host: String::new(),
            remote: String::new(),
            body: &mut body,
        };
        let resp = route(&app, &mut req);
        assert_eq!(resp.status, 200);
        (resp.undo.expect("a delivery carries its undo"))();

        let again = call(running.addr(), "GET", &format!("/v1/cli/authorizations/{slug}"));
        assert!(field(&again, "token").starts_with("krowk_sk_"), "{again}");
        let spent = call(running.addr(), "GET", &format!("/v1/cli/authorizations/{slug}"));
        assert!(spent.starts_with("HTTP/1.1 410") && spent.contains("\"spent\""), "{spent}");
    }

    /// One GET on a connection kept open, its response read whole.
    fn keep_alive_get(s: &mut TcpStream, addr: SocketAddr) -> io::Result<String> {
        write!(s, "GET / HTTP/1.1\r\nHost: {addr}\r\n\r\n")?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let len: usize = head
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
            .unwrap_or(0);
        let mut body = vec![0; len];
        s.read_exact(&mut body)?;
        Ok(head)
    }

    /// Every connection is served while others sit idle on keep-alive, as
    /// ureq's pool leaves them: none waits on another's closing. A burst of
    /// connections opened at once, each kept open until the whole burst is
    /// answered, more of them than the server has threads waiting, round
    /// after round. tiny_http's pool left one of a burst unread here, every
    /// run, until another closed.
    #[test]
    fn a_new_connection_is_answered_while_others_idle_on_keep_alive() {
        let running = start(TcpListener::bind("127.0.0.1:0").unwrap(), Config::default()).unwrap();
        let addr = running.addr();
        for _ in 0..20 {
            let at_once = Arc::new(std::sync::Barrier::new(12));
            let clients: Vec<_> = (0..12)
                .map(|_| {
                    let at_once = Arc::clone(&at_once);
                    thread::spawn(move || -> Result<TcpStream, String> {
                        at_once.wait();
                        let mut s = TcpStream::connect(addr).unwrap();
                        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                        let started = std::time::Instant::now();
                        let head = keep_alive_get(&mut s, addr).map_err(|e| format!("unanswered after {:?}: {e}", started.elapsed()))?;
                        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
                        // And again on the same connection, which stays usable.
                        keep_alive_get(&mut s, addr).map_err(|e| format!("the second request: {e}"))?;
                        Ok(s)
                    })
                })
                .collect();
            // Every stream is held here until the whole burst has been joined.
            let answered: Vec<_> = clients.into_iter().map(|c| c.join().unwrap()).collect();
            for a in &answered {
                assert!(a.is_ok(), "{a:?}");
            }
        }
    }

    /// A stopped registry answers nothing more, on a connection kept open
    /// from before the stop either.
    #[test]
    fn a_stop_closes_the_connections_kept_open() {
        let running = start(TcpListener::bind("127.0.0.1:0").unwrap(), Config::default()).unwrap();
        let addr = running.addr();
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        keep_alive_get(&mut s, addr).unwrap();
        running.stop();
        assert!(keep_alive_get(&mut s, addr).is_err(), "answered after the stop");
        assert!(TcpStream::connect(addr).is_err(), "still accepting after the stop");
    }
}
