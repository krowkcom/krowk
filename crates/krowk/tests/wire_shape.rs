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
    let proxy = start_proxy(registry.addr(), calls.clone(), failures.clone(), Arc::new(approve_logins));

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

/// The sync calls (R-SYNC-1): a device registered by `krowk sync recover`,
/// the listing, and the session and lease calls krowk-api makes for the
/// host (ticket 19 puts them behind a command).
#[cfg(all(feature = "harness", unix))]
#[test]
fn sync_wire_shape_matches_the_registrys_routes() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    // Its own directory: the test beside it clears `scratch()` as it goes.
    let dir = std::env::temp_dir().join(format!("krowk-wire-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("laptop/home")).unwrap();
    std::fs::create_dir_all(dir.join("desktop/home")).unwrap();
    let api = Arc::new(Mutex::new(String::new()));
    let laptop_home = dir.join("laptop/home");
    // Pairing is sync_pairing.rs, which runs both sides of it.
    let hook: Hook = Arc::new(|_registry, _method: &str, _path: &str, _answer: &[u8]| Ok(()));
    let proxy = start_proxy(registry.addr(), calls.clone(), failures.clone(), hook);
    *api.lock().unwrap() = format!("http://{proxy}/v1");
    let laptop = Krowk { home: laptop_home, api: api.lock().unwrap().clone() };

    // Sync on the account key, as the lease calls still run on it: set up in
    // the laptop's home and registered, as `sync recover` did from the phrase.
    let keys = krowk_client::keystore::Keystore::new(&laptop.home.join(".krowk"));
    let (setup, _) = keys.recover(krowk_client::e2e::AccountKey::generate()).unwrap();
    let signing = keys.signing_key().unwrap();
    let signing_public = krowk_client::e2e::hex(&signing.public().0);
    krowk_api::Client::new(&laptop.api, "krowk_sk_test")
        .signed_by(krowk_client::e2e::DeviceSigner::new(setup.device.id(), signing).shared())
        .register_device(&krowk_client::e2e::hex(&setup.device.public().0), &signing_public, "laptop", &setup.account.id().to_string())
        .unwrap();

    let store = krowk_client::keystore::Keystore::new(&laptop.home.join(".krowk"));
    let device_id = store.device().unwrap().unwrap().id();
    let signer = krowk_client::e2e::DeviceSigner::new(device_id, store.signing_key().unwrap()).shared();
    let client = krowk_api::Client::new(&laptop.api, "krowk_sk_test").signed_by(signer);
    let device = device_id.to_string();
    let id = "0190f3a8-7c1e-7a9b-8c2d-3e4f5a6b7c8d";
    client.put_sync_session(id, &"00".repeat(74), None, Some(&"00".repeat(40)), None).unwrap();
    let lease = client.acquire_lease(id, &device, 60, "production").unwrap();
    client.renew_lease(id, &device, &lease.token, 60, "production").unwrap();
    assert!(!lease.relay_ticket.is_empty(), "the lease carries its holder's relay ticket");
    assert!(!client.relay_ticket(id, &device, "development").unwrap().relay_ticket.is_empty());
    client.list_sync_sessions("", 50).unwrap();
    client.show_sync_session(id).unwrap();
    let sealed = b"sealed bytes stand-in";
    let chunk = client.put_chunk(id, 0, sealed, &lease.token).unwrap();
    let listed = client.list_chunks(id, None, 50).unwrap();
    assert_eq!(client.read_chunk(&listed.chunks[0]).unwrap(), sealed);
    assert_eq!(chunk.index, 0);
    client.release_lease(id, &lease.token).unwrap();

    assert!(failures.lock().unwrap().is_empty(), "{:?}", failures.lock().unwrap());
    let want = [
        // The laptop's device, registered. +signed marks
        // the calls that act as a device, signed by its key (crypto.md →
        // Signed registry requests), and only they are.
        "POST /v1/devices +signed",
        // A session is PUT under the id its client minted; its lease is a
        // singular resource: POST acquires, PUT renews or hands over, DELETE
        // lets go.
        "PUT /v1/sessions/{id} +signed",
        "POST /v1/sessions/{id}/lease +signed",
        "PUT /v1/sessions/{id}/lease +signed",
        // A viewer's relay ticket (relay.md → Tickets); a host's comes
        // with the lease.
        "GET /v1/sessions/{id}/relay_ticket +signed",
        "GET /v1/sessions",
        "GET /v1/sessions/{id}",
        // A chunk of the session's log: declared under an Idempotency-Key,
        // its bytes put to storage (not pinned, as for an artifact), then
        // finalized; read back through the listing and storage.
        "POST /v1/sessions/{id}/chunks +key +signed",
        "PUT /v1/sessions/{id}/chunks/0/finalization +signed",
        "GET /v1/sessions/{id}/chunks",
        "DELETE /v1/sessions/{id}/lease +signed",
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
            // The debug build's stand-in for the person saying yes at
            // `devices approve` and `sync join`.
            .env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1")
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

/// What the proxy does with an answer before handing it on: stand in for
/// the person on the other side of a flow that waits for one, so the CLI
/// polls once and the sequence stays exact.
type HookFn = dyn Fn(SocketAddr, &str, &str, &[u8]) -> Result<(), String> + Send + Sync;
type Hook = Arc<HookFn>;

/// A proxy of one request per connection — `Connection: close` both ways, so
/// there is no keep-alive to follow — that records each API call as the pins
/// read it, and runs `hook` on each answer before handing it on.
fn start_proxy(registry: SocketAddr, calls: Arc<Mutex<Vec<String>>>, failures: Arc<Mutex<Vec<String>>>, hook: Hook) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (calls, failures, hook) = (calls.clone(), failures.clone(), hook.clone());
            std::thread::spawn(move || {
                if let Err(e) = relay(conn, registry, &calls, &*hook) {
                    failures.lock().unwrap().push(e);
                }
            });
        }
    });
    addr
}

/// Approves a browser login the moment it is opened.
fn approve_logins(registry: SocketAddr, method: &str, path: &str, answer: &[u8]) -> Result<(), String> {
    if method == "POST" && path.split('?').next() == Some("/v1/cli/authorizations") {
        // Every login says what it is for, and `krowk login` is a login.
        if !path.contains("action=login") {
            return Err(format!("a browser login opened without its action: {path}"));
        }
        approve(registry, answer)?;
    }
    Ok(())
}

fn relay(mut client: TcpStream, registry: SocketAddr, calls: &Mutex<Vec<String>>, hook: &HookFn) -> Result<(), String> {
    let (head, body) = read_request(&mut client)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split(' ');
    let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
    let headers: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
    let has = |name: &str| headers.iter().any(|h| h.to_ascii_lowercase().starts_with(name));
    let keyed = has("idempotency-key:");
    let signed = has("x-krowk-device:") && has("x-krowk-timestamp:") && has("x-krowk-signature:");
    if let Some(call) = wire_call(&method, &path, keyed, signed) {
        calls.lock().unwrap().push(call);
    }

    let answer = exchange(registry, &request_line, &headers, &body)?;
    // Answered before the answer is handed on, so the CLI can never poll a
    // login or an approval nobody has answered yet.
    hook(registry, &method, &path, &answer)?;
    client.write_all(&answer).map_err(|e| e.to_string())
}

/// The request as the pins name it; None for what is not the API surface —
/// object storage, and the approval page the stand-in serves for the app.
fn wire_call(method: &str, path: &str, keyed: bool, signed: bool) -> Option<String> {
    let path = path.split('?').next().unwrap_or_default();
    if path.starts_with("/_storage") || path.starts_with("/_approve") {
        return None;
    }
    let slugged: Vec<String> = path
        .split('/')
        .map(|seg| {
            let slug = ["art_", "aut_", "run_", "ws_", "dap_"].iter().any(|p| seg.starts_with(p) && seg.len() > p.len());
            // A synced session is named by the UUID its client minted.
            let uuid = seg.len() == 36 && seg.bytes().filter(|b| *b == b'-').count() == 4;
            if slug { "{slug}".to_string() } else if uuid { "{id}".to_string() } else { seg.to_string() }
        })
        .collect();
    Some(format!("{method} {}{}{}", slugged.join("/"), if keyed { " +key" } else { "" }, if signed { " +signed" } else { "" }))
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
