//! A stand-in HTTP server for the MCP and OAuth tests: HTTP/1.1 on a
//! loopback port, answering each request with what the test's closure
//! returns (an event stream, JSON, or a redirect), and keeping every
//! request it was sent so a test can pin what krowk asked. Standard library
//! only, and a thread per server, so the harness tests and the CLI's tests
//! run the same one.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One request as it arrived.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
    /// The body as sent, for a form.
    pub raw: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// What the server answers a request with. A 200 is an event stream unless
/// `content_type` says otherwise; anything else is JSON.
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub content_type: Option<&'static str>,
    pub headers: Vec<(String, String)>,
}

impl Reply {
    pub fn sse(body: &str) -> Reply {
        Reply::status(200, body)
    }

    pub fn status(status: u16, body: &str) -> Reply {
        Reply { status, body: body.to_string(), content_type: None, headers: Vec::new() }
    }

    pub fn json(status: u16, body: &serde_json::Value) -> Reply {
        Reply { content_type: Some("application/json"), ..Reply::status(status, &body.to_string()) }
    }

    pub fn redirect(to: &str) -> Reply {
        Reply { headers: vec![("location".into(), to.into())], ..Reply::status(302, "") }
    }
}

pub struct Mock {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

/// Serves `answer(request, n)` for the n-th request (from 0), forever.
pub fn serve(answer: impl Fn(&serde_json::Value, usize) -> Reply + Send + 'static) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    serve_on(listener, answer)
}

pub fn serve_on(listener: TcpListener, answer: impl Fn(&serde_json::Value, usize) -> Reply + Send + 'static) -> Mock {
    serve_seen_on(listener, move |seen, n| answer(&seen.body, n))
}

/// Serves `answer(request, n)`, handing it the whole request.
pub fn serve_seen(answer: impl Fn(&Seen, usize) -> Reply + Send + 'static) -> Mock {
    serve_seen_on(TcpListener::bind("127.0.0.1:0").expect("bind a loopback port"), answer)
}

pub fn serve_seen_on(listener: TcpListener, answer: impl Fn(&Seen, usize) -> Reply + Send + 'static) -> Mock {
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for (n, conn) in listener.incoming().enumerate() {
            let Ok(mut conn) = conn else { continue };
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let method = line.split_whitespace().next().unwrap_or_default().to_string();
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut length = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
                if let Some((k, v)) = h.trim_end().split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let raw = String::from_utf8_lossy(&body).into_owned();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let seen = Seen { method, path, headers, body, raw };
            let reply = answer(&seen, n);
            log.lock().unwrap().push(seen);
            let kind = reply.content_type.unwrap_or(if reply.status == 200 { "text/event-stream" } else { "application/json" });
            let extra: String = reply.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
            let head = format!("HTTP/1.1 {} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n", reply.status, reply.body.len());
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(reply.body.as_bytes());
            let _ = conn.flush();
        }
    });
    Mock { url, seen }
}

/// Follows a sign-in link as a browser would: the authorization server's
/// redirect, then the loopback callback it points at. Returns the page.
pub fn browse(url: &str) -> String {
    let get = |u: &str| -> (u16, Option<String>, String) {
        let rest = u.strip_prefix("http://").expect("a loopback http URL");
        let (addr, target) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let mut conn = std::net::TcpStream::connect(addr).unwrap();
        write!(conn, "GET {target} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = conn.read_to_string(&mut out);
        let status = out.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let location = out.lines().find_map(|l| l.strip_prefix("location: ").map(String::from));
        (status, location, out)
    };
    let (status, location, _) = get(url);
    assert_eq!(status, 302, "the authorization server redirects back");
    get(&location.unwrap()).2
}
