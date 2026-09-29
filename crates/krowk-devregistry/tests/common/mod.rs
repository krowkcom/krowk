//! A registry on a real socket with a movable clock, and a bare HTTP client:
//! these tests pin the server's contract, so they speak wire shapes rather than
//! going through a client that already passes.
#![allow(dead_code)]

use jiff::{SignedDuration, Timestamp};
use krowk_devregistry::{Config, Running};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const TEST_KEY: &str = "krowk_sk_test";

pub struct Server {
    running: Running,
    pub url: String,
    clock: Arc<Mutex<Timestamp>>,
}

impl Server {
    pub fn new() -> Server {
        Server::with_limit(0)
    }

    pub fn with_limit(limit_bytes: i64) -> Server {
        let clock = Arc::new(Mutex::new(Timestamp::now()));
        let c = Arc::clone(&clock);
        let config = Config { limit_bytes, site: String::new(), clock: Some(Arc::new(move || *c.lock().unwrap())), max_sessions: 0 };
        let running = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), config).unwrap();
        Server { url: running.url(), running, clock }
    }

    pub fn advance(&self, d: SignedDuration) {
        let mut at = self.clock.lock().unwrap();
        *at += d;
    }

    pub fn at(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap_or_default().to_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// One request on its own connection. `headers` go out as given, so a test can
/// send one empty, and a body is always framed with its length.
pub fn send(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Response {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let mut s = TcpStream::connect(host).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v == "chunked") {
        body = dechunk(&body);
    }
    Response { status, headers, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").unwrap();
        let n = usize::from_str_radix(std::str::from_utf8(&b[..eol]).unwrap().trim(), 16).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&b[eol + 2..eol + 2 + n]);
        b = &b[eol + 4 + n..];
    }
}

/// A bare call, keyless when `token` is empty.
pub fn request(method: &str, url: &str, token: &str, content_type: &str, body: &str) -> Response {
    let auth = format!("Bearer {token}");
    let mut headers = vec![];
    if !token.is_empty() {
        headers.push(("Authorization", auth.as_str()));
    }
    if !content_type.is_empty() {
        headers.push(("Content-Type", content_type));
    }
    send(method, url, &headers, body.as_bytes())
}

/// A request with an Idempotency-Key; `None` leaves the header off entirely,
/// which is not the same as sending it empty.
pub fn keyed(method: &str, url: &str, token: &str, key: Option<&str>, body: &str) -> Response {
    let auth = format!("Bearer {token}");
    let mut headers = vec![("Content-Type", "application/json")];
    if !token.is_empty() {
        headers.push(("Authorization", auth.as_str()));
    }
    if let Some(key) = key {
        headers.push(("Idempotency-Key", key));
    }
    send(method, url, &headers, body.as_bytes())
}

pub fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

pub fn declare(s: &Server, token: &str, filename: &str, body: &str) -> Value {
    declare_typed(s, token, filename, "text/plain", body.len())
}

/// Declared with its content type, which matters wherever being an image is
/// the thing under test.
pub fn declare_typed(s: &Server, token: &str, filename: &str, content_type: &str, len: usize) -> Value {
    let body = json!({"artifact": {"filename": filename, "content_type": content_type, "byte_size": len}});
    let r = request("POST", &s.at("/v1/artifacts"), token, "application/json", &body.to_string());
    assert_eq!(r.status, 201, "declare = {}", r.text());
    r.json()
}

pub fn upload_url(payload: &Value) -> String {
    let url = payload["upload"]["url"].as_str().unwrap_or_default();
    assert!(!url.is_empty(), "no upload url in {payload}");
    url.to_owned()
}

pub fn put(url: &str, body: &str) -> u16 {
    request("PUT", url, "", "text/plain", body).status
}

/// Uploads with exactly the headers the declare handed back, which is what a
/// client does and what storage checks.
pub fn put_signed(payload: &Value, body: &[u8]) -> u16 {
    let headers: Vec<(String, String)> = payload["upload"]["headers"]
        .as_object()
        .map(|h| h.iter().filter(|(k, _)| *k != "Content-Length").map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned())).collect())
        .unwrap_or_default();
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    send("PUT", &upload_url(payload), &headers, body).status
}

pub fn finalize(s: &Server, token: &str, payload: &Value) -> Response {
    request("PUT", &s.at(&format!("/v1/artifacts/{}/finalization", str_of(payload, "slug"))), token, "", "")
}

pub fn show(s: &Server, token: &str, slug: &str) -> Response {
    request("GET", &s.at(&format!("/v1/artifacts/{slug}")), token, "", "")
}

pub fn must_show(s: &Server, token: &str, slug: &str) -> Value {
    let r = show(s, token, slug);
    assert_eq!(r.status, 200, "show = {}", r.text());
    r.json()
}

/// Asks for the upload of an artifact again, with no body when `claim_token`
/// is empty.
pub fn presign(s: &Server, token: &str, slug: &str, claim_token: &str) -> Response {
    let body = if claim_token.is_empty() { String::new() } else { json!({"claim_token": claim_token}).to_string() };
    request("POST", &s.at(&format!("/v1/artifacts/{slug}/upload")), token, "application/json", &body)
}

pub fn claim(s: &Server, token: &str, slug: &str, claim_token: &str) -> Response {
    let body = json!({"claim_token": claim_token}).to_string();
    request("POST", &s.at(&format!("/v1/artifacts/{slug}/claim")), token, "application/json", &body)
}

pub fn take_down(s: &Server, token: &str, slug: &str, claim_token: &str) -> Response {
    let (body, ct) =
        if claim_token.is_empty() { (String::new(), "") } else { (json!({"claim_token": claim_token}).to_string(), "application/json") };
    request("DELETE", &s.at(&format!("/v1/artifacts/{slug}")), token, ct, &body)
}

/// Declared, uploaded and finalized: the state a takedown meets in practice.
pub fn ready_artifact(s: &Server, token: &str, body: &str) -> Value {
    let payload = declare(s, token, "a.txt", body);
    assert_eq!(put(&upload_url(&payload), body), 200);
    let r = finalize(s, token, &payload);
    assert_eq!(r.status, 200, "finalize = {}", r.text());
    payload
}

pub fn open_run(s: &Server, token: &str) -> String {
    let r = request("POST", &s.at("/v1/runs"), token, "application/json", r#"{"run":{}}"#);
    assert_eq!(r.status, 201, "open run = {}", r.text());
    str_of(&r.json(), "slug").to_owned()
}

pub fn attach(s: &Server, token: &str, slug: &str, run: &str) -> Response {
    request("PUT", &s.at(&format!("/v1/artifacts/{slug}/run")), token, "application/json", &json!({"run": run}).to_string())
}

pub fn slugs_of(payload: &Value, key: &str) -> Vec<String> {
    payload[key].as_array().map(|rows| rows.iter().map(|r| str_of(r, "slug").to_owned()).collect()).unwrap_or_default()
}

pub fn run_slug_of(payload: &Value) -> &str {
    payload["run"]["slug"].as_str().unwrap_or_default()
}

/// A PNG header of the given size — IHDR with a valid CRC, which is all a
/// measurement reads.
pub fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n\0\0\0\x0d".to_vec();
    out.extend_from_slice(&ihdr);
    out.extend_from_slice(&crc32(&ihdr).to_be_bytes());
    out.extend_from_slice(b"\0\0\0\0IEND\xae\x42\x60\x82");
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in data {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}
