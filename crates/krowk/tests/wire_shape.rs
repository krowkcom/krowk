//! The method and path of every call the CLI makes to a registry, pinned.
//!
//! The stand-in registry keeps the suite hermetic, and that is also its hazard:
//! a stand-in drifting with the client keeps every other test green while
//! api.krowk.com answers 404. This list is the one to hold against
//! `bin/rails routes` in krowk-registry when either side moves.
//!
//! The binary runs as a subprocess with an empty environment and a temporary
//! HOME — the credentials path is resolved from the process environment, and a
//! test must not write the developer's own. A recording proxy sits between it
//! and the in-process stand-in.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

#[test]
fn wire_shape_matches_the_registrys_routes() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let proxy = start_proxy(registry.addr(), calls.clone(), failures.clone());

    let dir = scratch();
    let file = dir.join("shot.png");
    std::fs::write(&file, "some bytes").unwrap();
    let file = file.display().to_string();
    let krowk = Krowk { home: dir.join("home"), api: format!("http://{proxy}/v1") };

    let pushed = krowk.ok(&["push", &file], true);
    let art = pushed["data"]["artifacts"][0]["slug"].as_str().unwrap().to_string();
    let run = pushed["data"]["run"]["slug"].as_str().unwrap().to_string();
    krowk.ok(&["uploads", "list"], true);
    krowk.ok(&["uploads", "show", &art], true);
    krowk.ok(&["runs", "list"], true);
    krowk.ok(&["runs", "show", &run], true);
    // A run's artifacts are a collection of the run, not a filter on the
    // listing above — so --run is a different endpoint, not a query parameter.
    krowk.ok(&["uploads", "list", &format!("--run={run}")], true);
    krowk.run(&["doctor"], true);

    // Claiming needs an anonymous artifact to claim.
    let anonymous = krowk.ok(&["push", &file], false);
    let anon = &anonymous["data"]["artifacts"][0];
    let (slug, token) = (anon["slug"].as_str().unwrap().to_string(), anon["claim_token"].as_str().unwrap().to_string());
    krowk.ok(&["claim", &slug, &token], true);
    krowk.ok(&["uploads", "attach", &slug, &format!("--run={run}")], true);
    // Taking it down again: a claimed artifact answers to the key that holds it.
    krowk.ok(&["uploads", "delete", &slug], true);

    // A browser login, approved by the proxy the instant it is opened so the
    // poll happens once. --no-browser: a test must not reach for the desktop.
    krowk.ok(&["auth", "login", "--no-browser"], true);

    assert!(failures.lock().unwrap().is_empty(), "standing in for the person at the browser: {:?}", failures.lock().unwrap());
    let want = [
        // push, with a key: open a run, declare, finalize, close the run. The
        // two creates name their attempt so a retry of either costs nothing;
        // +key marks the ones that carry an Idempotency-Key, and only they do.
        "POST /v1/runs +key",
        "POST /v1/artifacts +key",
        "PUT /v1/artifacts/{slug}/finalization",
        "PUT /v1/runs/{slug}/completion",
        "GET /v1/artifacts",
        "GET /v1/artifacts/{slug}",
        "GET /v1/runs",
        "GET /v1/runs/{slug}",
        "GET /v1/runs/{slug}/artifacts",
        // doctor: the reachability probe, then the key check.
        "GET /",
        "GET /v1/key",
        // push, keyless: no run to open or close.
        "POST /v1/artifacts +key",
        "PUT /v1/artifacts/{slug}/finalization",
        "POST /v1/artifacts/{slug}/claim",
        // and the run it never had, set afterwards.
        "PUT /v1/artifacts/{slug}/run",
        // Takedown is the REST delete of the artifact, not a nested resource.
        "DELETE /v1/artifacts/{slug}",
        // A browser login: open an authorization, then read it back until
        // somebody answers. No Idempotency-Key on the create, the one exception:
        // a lost response means the code was never seen, so the authorization
        // can never be approved and nothing is charged for it.
        "POST /v1/cli/authorizations",
        "GET /v1/cli/authorizations/{slug}",
    ];
    let got = calls.lock().unwrap().clone();
    assert_eq!(got, want, "calls:\n  {}", got.join("\n  "));
    let _ = std::fs::remove_dir_all(dir);
}

struct Krowk {
    home: PathBuf,
    api: String,
}

impl Krowk {
    fn run(&self, args: &[&str], keyed: bool) -> (bool, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_krowk"));
        cmd.args(args)
            .arg("--json")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("KROWK_API_URL", &self.api)
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .current_dir(self.home.parent().unwrap());
        if keyed {
            cmd.env("KROWK_TOKEN", "krowk_sk_test");
        }
        let out = cmd.output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }

    fn ok(&self, args: &[&str], keyed: bool) -> Value {
        let (ok, out) = self.run(args, keyed);
        assert!(ok, "krowk {} failed:\n{out}", args.join(" "));
        serde_json::from_str(&out).unwrap_or(Value::Null)
    }
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("krowk-wire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    dir.canonicalize().unwrap()
}

/// A proxy of one request per connection — `Connection: close` both ways, so
/// there is no keep-alive to follow — that records each API call as the pins
/// read it, and approves a browser login before handing its answer on.
fn start_proxy(registry: SocketAddr, calls: Arc<Mutex<Vec<String>>>, failures: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (calls, failures) = (calls.clone(), failures.clone());
            std::thread::spawn(move || {
                if let Err(e) = relay(conn, registry, &calls) {
                    failures.lock().unwrap().push(e);
                }
            });
        }
    });
    addr
}

fn relay(mut client: TcpStream, registry: SocketAddr, calls: &Mutex<Vec<String>>) -> Result<(), String> {
    let (head, body) = read_request(&mut client)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split(' ');
    let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
    let headers: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
    let keyed = headers.iter().any(|h| h.to_ascii_lowercase().starts_with("idempotency-key:"));
    if let Some(call) = wire_call(&method, &path, keyed) {
        calls.lock().unwrap().push(call);
    }

    let answer = exchange(registry, &request_line, &headers, &body)?;
    // Approved before the answer is handed on, so the CLI can never poll a
    // login nobody has answered yet and the sequence stays exact.
    if method == "POST" && path == "/v1/cli/authorizations" {
        approve(registry, &answer)?;
    }
    client.write_all(&answer).map_err(|e| e.to_string())
}

/// The request as the pins name it; None for what is not the API surface —
/// object storage, and the approval page the stand-in serves for the app.
fn wire_call(method: &str, path: &str, keyed: bool) -> Option<String> {
    let path = path.split('?').next().unwrap_or_default();
    if path.starts_with("/_storage") || path.starts_with("/_approve") {
        return None;
    }
    let slugged: Vec<String> = path
        .split('/')
        .map(|seg| {
            let slug = ["art_", "aut_", "run_", "ws_"].iter().any(|p| seg.starts_with(p) && seg.len() > p.len());
            if slug { "{slug}".to_string() } else { seg.to_string() }
        })
        .collect();
    Some(format!("{method} {}{}", slugged.join("/"), if keyed { " +key" } else { "" }))
}

/// One request to the registry, with `Connection: close`; the whole raw answer.
fn exchange(registry: SocketAddr, request_line: &str, headers: &[&str], body: &[u8]) -> Result<Vec<u8>, String> {
    let mut upstream = TcpStream::connect(registry).map_err(|e| e.to_string())?;
    let mut out = format!("{request_line}\r\n");
    for h in headers.iter().filter(|h| !h.to_ascii_lowercase().starts_with("connection:")) {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    upstream.write_all(out.as_bytes()).and_then(|()| upstream.write_all(body)).map_err(|e| e.to_string())?;
    let mut answer = Vec::new();
    upstream.read_to_end(&mut answer).map_err(|e| e.to_string())?;
    Ok(force_close(&answer))
}

/// The answer with its own Connection header replaced by `close`.
fn force_close(answer: &[u8]) -> Vec<u8> {
    let Some(end) = find(answer, b"\r\n\r\n") else { return answer.to_vec() };
    let head = String::from_utf8_lossy(&answer[..end]);
    let mut out: Vec<&str> = head.split("\r\n").filter(|l| !l.to_ascii_lowercase().starts_with("connection:")).collect();
    out.push("Connection: close");
    let mut bytes = out.join("\r\n").into_bytes();
    bytes.extend_from_slice(&answer[end..]);
    bytes
}

fn approve(registry: SocketAddr, created: &[u8]) -> Result<(), String> {
    let body = response_body(created);
    let login: Value = serde_json::from_slice(&body).map_err(|e| format!("opening a browser login answered no JSON: {e}"))?;
    let code = login["code"].as_str().filter(|c| !c.is_empty()).ok_or("opening a browser login answered no code")?;
    let line = format!("POST /_approve/cli/authorizations/{code}/approval HTTP/1.1");
    let answer = exchange(registry, &line, &["Host: localhost", "Content-Length: 0"], b"")?;
    let status = String::from_utf8_lossy(&answer).split(' ').nth(1).unwrap_or_default().to_string();
    if status != "200" {
        return Err(format!("approving {code} answered {status}"));
    }
    Ok(())
}

/// The body of a raw HTTP answer, de-chunked when it came chunked.
fn response_body(answer: &[u8]) -> Vec<u8> {
    let Some(end) = find(answer, b"\r\n\r\n") else { return Vec::new() };
    let head = String::from_utf8_lossy(&answer[..end]).to_ascii_lowercase();
    let body = &answer[end + 4..];
    if !head.contains("transfer-encoding: chunked") {
        return body.to_vec();
    }
    let (mut out, mut rest) = (Vec::new(), body);
    while let Some(nl) = find(rest, b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..nl]).trim(), 16).unwrap_or(0);
        if size == 0 || rest.len() < nl + 2 + size {
            break;
        }
        out.extend_from_slice(&rest[nl + 2..nl + 2 + size]);
        rest = &rest[(nl + 4 + size).min(rest.len())..];
    }
    out
}

/// Reads one request: its head, and a body of Content-Length bytes.
fn read_request(conn: &mut TcpStream) -> Result<(String, Vec<u8>), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let end = loop {
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            break end;
        }
        let n = conn.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the client closed before a whole request".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let length = head
        .split("\r\n")
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    let mut body = buf[end + 4..].to_vec();
    while body.len() < length {
        let n = conn.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok((head, body))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
