//! The HTTP/1.1 the stand-in speaks: a thread a connection, for as long as
//! the client keeps it open, and a request read off it, answered, and the
//! next read after.
//!
//! Not tiny_http any more. Its pool handed each connection to a worker for
//! the connection's life and woke one waiting worker per connection, but
//! counted a worker as waiting until it had taken the lock again: two
//! connections arriving together while one worker waited both went to that
//! one, and the second sat queued, unread, until some other connection
//! closed — a client's idle keep-alive, 15 seconds and more later. That was
//! a chunk GET answered 20 seconds late, under load, now and then. A thread
//! here is started for each connection as it is accepted, so none waits on
//! another.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// A request as read off the wire, its body still on the connection.
pub struct Head {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub remote: SocketAddr,
}

/// What a handler answers with; `undo` runs if it never reached the client.
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    pub undo: Option<Box<dyn FnOnce() + Send>>,
}

pub type Handler = dyn Fn(&Head, &mut dyn Read) -> Answer + Send + Sync;

/// The most a request's head may be: past it the request is refused.
const HEAD_MAX: usize = 1 << 20;

/// The most of a body a handler left unread that is read off to keep the
/// connection; past it the connection is closed instead, as Go's server does.
const DRAIN_MAX: u64 = 256 << 10;

/// A body nested as deep as Go's 10,000 levels needs the stack.
const STACK: usize = 64 << 20;

/// The connections open now, so a stop can close them: a stopped registry
/// answers nothing more, on a kept-alive connection either.
#[derive(Default)]
pub struct Open {
    stopping: AtomicBool,
    next: AtomicU64,
    conns: Mutex<HashMap<u64, TcpStream>>,
}

impl Open {
    /// Stops accepting and closes every open connection. `addr` is the
    /// listener's, dialled once so a blocked accept returns.
    pub fn stop(&self, addr: SocketAddr) {
        self.stopping.store(true, Ordering::SeqCst);
        for c in self.conns.lock().unwrap_or_else(|e| e.into_inner()).values() {
            let _ = c.shutdown(Shutdown::Both);
        }
        let ip = match addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        // Dialled until it connects: a dial refused for want of a
        // descriptor would leave the accept blocked, and a stop waiting on
        // it for ever. Ten seconds, then the stop goes on without.
        for _ in 0..500 {
            if TcpStream::connect(SocketAddr::new(ip, addr.port())).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// Accepts until accepting fails for good or `open` is stopped, and says
/// why. A machine out of descriptors, memory or threads for a moment is
/// waited out, as Go's server waits out a temporary accept error: that
/// connection is let go, and the registry goes on serving the next, rather
/// than stopping for good under a test run's load.
pub fn accept(listener: &TcpListener, open: &Arc<Open>, handler: &Arc<Handler>) -> io::Error {
    let mut pause = std::time::Duration::ZERO;
    loop {
        let (stream, remote) = match listener.accept() {
            Ok(c) => c,
            Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted) => continue,
            Err(e) if temporary(&e) && !open.stopping() => {
                wait_out(&mut pause, "accepting failed", &e);
                continue;
            }
            Err(e) => return e,
        };
        if open.stopping() {
            return io::Error::other("the registry was stopped");
        }
        let (o, h) = (Arc::clone(open), Arc::clone(handler));
        match thread::Builder::new().stack_size(STACK).spawn(move || connection(&o, &*h, stream, remote)) {
            Ok(_) => pause = std::time::Duration::ZERO,
            Err(e) => wait_out(&mut pause, "a connection's thread did not start, so it was closed", &e),
        }
    }
}

/// Says why, and sleeps the next step of a backoff from 5 ms to a second.
fn wait_out(pause: &mut std::time::Duration, what: &str, e: &io::Error) {
    *pause = (*pause * 2).clamp(std::time::Duration::from_millis(5), std::time::Duration::from_secs(1));
    eprintln!("devregistry: {what} ({e}); trying again in {pause:?}");
    thread::sleep(*pause);
}

/// An accept error gone in a moment: out of descriptors, memory or
/// buffers, EAGAIN, and the pending network errors accept(2) on Linux
/// passes on and asks be retried like EAGAIN — everything but a listener
/// that is no listener (EBADF, EFAULT, EINVAL, ENOTSOCK).
fn temporary(e: &io::Error) -> bool {
    let enotsock = if cfg!(target_os = "macos") { 38 } else { 88 };
    !e.raw_os_error().is_some_and(|n| [9, 14, 22, enotsock].contains(&n))
}

fn connection(open: &Open, handler: &Handler, stream: TcpStream, remote: SocketAddr) {
    let id = open.next.fetch_add(1, Ordering::SeqCst);
    match stream.try_clone() {
        Ok(c) => open.conns.lock().unwrap_or_else(|e| e.into_inner()).insert(id, c),
        Err(_) => return,
    };
    // A stop between the accept and the insert above closes this one too.
    if !open.stopping() {
        let _ = serve(open, handler, stream, remote);
    }
    open.conns.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}

/// Request after request on one connection, until either side closes it.
fn serve(open: &Open, handler: &Handler, stream: TcpStream, remote: SocketAddr) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let (head, version) = match read_head(&mut reader, remote)? {
            Some(Ok(h)) => h,
            Some(Err(())) => return writer.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
            None => return Ok(()),
        };
        if open.stopping() {
            return Ok(());
        }
        let header = |name: &str| head.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_ascii_lowercase());
        let mut close = version == "HTTP/1.0" || header("Connection").is_some_and(|c| c.contains("close"));
        match header("Expect").as_deref() {
            None => {}
            Some("100-continue") => writer.write_all(format!("{version} 100 Continue\r\n\r\n").as_bytes())?,
            Some(_) => return writer.write_all(format!("{version} 417 Expectation Failed\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()),
        }
        let mut body: Box<dyn Read + '_> = if header("Transfer-Encoding").is_some() {
            Box::new(Chunked { inner: &mut reader, left: 0, done: false })
        } else {
            match header("Content-Length").map(|n| n.parse::<u64>()) {
                None => Box::new(io::empty()),
                Some(Ok(n)) => Box::new((&mut reader).take(n)),
                Some(Err(_)) => return writer.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
            }
        };
        // A handler that panics is a registry bug: answered 500 and closed,
        // as tiny_http did, rather than left with the client waiting.
        let answer = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&head, &mut body))) {
            Ok(a) => a,
            Err(_) => {
                close = true;
                Answer { status: 500, headers: vec![], body: vec![], undo: None }
            }
        };
        // What the handler left unread comes off before the next request
        // can be; too much of it, and the connection goes instead.
        let mut unread = false;
        match io::copy(&mut (&mut body).take(DRAIN_MAX + 1), &mut io::sink()) {
            Ok(n) if n <= DRAIN_MAX => {}
            _ => (close, unread) = (true, true),
        }
        drop(body);
        close |= answer.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("Connection") && v.eq_ignore_ascii_case("close"));
        let wrote = writer.write_all(&response(&version, &head.method, &answer, close)).and_then(|()| writer.flush());
        if wrote.is_err() {
            if let Some(undo) = answer.undo {
                undo();
            }
            return wrote;
        }
        if close {
            if unread {
                linger(&writer, &mut reader);
            }
            return Ok(());
        }
    }
}

/// Closing on a body still arriving: a close with bytes unread sends a
/// reset, which can reach the client before the answer does. Go's server
/// shuts its side and reads on a moment first (`closeWriteAndWait`); so
/// does this, for up to a second.
fn linger(writer: &TcpStream, reader: &mut impl Read) {
    let _ = writer.shutdown(Shutdown::Write);
    let _ = writer.set_read_timeout(Some(std::time::Duration::from_millis(100)));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut buf = [0u8; 16384];
    while std::time::Instant::now() < until {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
            Err(_) => return,
        }
    }
}

/// The request line and headers, or None at a clean end of the connection.
/// `Some(Err(()))` is a head that does not parse: answered 400 and closed.
#[allow(clippy::type_complexity)]
fn read_head(reader: &mut impl BufRead, remote: SocketAddr) -> io::Result<Option<Result<(Head, String), ()>>> {
    let mut lines: Vec<String> = Vec::new();
    let mut size = 0;
    loop {
        let mut line = Vec::new();
        let n = reader.take((HEAD_MAX - size + 1) as u64).read_until(b'\n', &mut line)?;
        size += n;
        if n == 0 {
            // EOF: clean between requests, a cut-off request otherwise.
            return Ok(if lines.is_empty() { None } else { Some(Err(())) });
        }
        if size > HEAD_MAX || !line.ends_with(b"\n") {
            return Ok(Some(Err(())));
        }
        let Ok(line) = String::from_utf8(line) else { return Ok(Some(Err(()))) };
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        // Blank lines before a request line are skipped, as RFC 9112 allows.
        if line.is_empty() && lines.is_empty() {
            continue;
        }
        if line.is_empty() {
            break;
        }
        lines.push(line.to_owned());
    }
    let mut parts = lines[0].split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else { return Ok(Some(Err(()))) };
    if method.is_empty() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Ok(Some(Err(())));
    }
    let mut headers = Vec::new();
    for l in &lines[1..] {
        let Some((k, v)) = l.split_once(':') else { return Ok(Some(Err(()))) };
        if k.is_empty() || k.contains([' ', '\t']) {
            return Ok(Some(Err(())));
        }
        headers.push((k.to_owned(), v.trim().to_owned()));
    }
    Ok(Some(Ok((Head { method: method.to_owned(), target: target.to_owned(), headers, remote }, version.to_owned()))))
}

/// The answer as bytes: its length always given, its body left off for HEAD.
fn response(version: &str, method: &str, a: &Answer, close: bool) -> Vec<u8> {
    let mut out = format!("{version} {} {}\r\n", a.status, reason(a.status));
    let date = jiff::fmt::rfc2822::DateTimePrinter::new().timestamp_to_rfc9110_string(&jiff::Timestamp::now()).unwrap_or_default();
    out.push_str(&format!("Date: {date}\r\n"));
    for (k, v) in &a.headers {
        if !k.eq_ignore_ascii_case("Connection") && !k.eq_ignore_ascii_case("Content-Length") && !k.eq_ignore_ascii_case("Transfer-Encoding") {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    if close {
        out.push_str("Connection: close\r\n");
    }
    let bodiless = matches!(a.status, 100..=199 | 204 | 304);
    if !bodiless {
        out.push_str(&format!("Content-Length: {}\r\n", a.body.len()));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    if method != "HEAD" && !bodiless {
        out.extend_from_slice(&a.body);
    }
    out
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        422 => "Unprocessable Entity",
        423 => "Locked",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// A chunked request body, de-chunked; trailers are read and dropped.
struct Chunked<'a, R: BufRead> {
    inner: &'a mut R,
    left: u64,
    done: bool,
}

impl<R: BufRead> Chunked<'_, R> {
    fn line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        self.inner.take(4096).read_until(b'\n', &mut line)?;
        if !line.ends_with(b"\n") {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a chunk line that never ends"));
        }
        Ok(String::from_utf8_lossy(&line).trim_end().to_owned())
    }
}

impl<R: BufRead> Read for Chunked<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or_default().trim();
            self.left = u64::from_str_radix(size, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a chunk size that is not hex"))?;
            if self.left == 0 {
                while !self.line()?.is_empty() {}
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the body ended inside a chunk"));
        }
        self.left -= n as u64;
        if self.left == 0 && !self.line()?.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a chunk not followed by its line end"));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunked_body_reads_as_its_bytes() {
        let mut wire: &[u8] = b"4;ext=1\r\nWiki\r\n5\r\npedia\r\n0\r\nTrailer: x\r\n\r\nNEXT";
        let mut out = String::new();
        Chunked { inner: &mut wire, left: 0, done: false }.read_to_string(&mut out).unwrap();
        assert_eq!(out, "Wikipedia");
        assert_eq!(wire, b"NEXT", "the trailers are read off and the next request left");
    }

    /// A handler that panics answers 500 and closes, the client not left
    /// waiting; one that refuses a large body unread still gets its answer
    /// to the client, not a reset.
    #[test]
    fn a_panic_is_a_500_and_an_unread_body_still_gets_its_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let open = Arc::new(Open::default());
        let handler: Arc<Handler> = Arc::new(|h: &Head, _: &mut dyn Read| {
            assert!(h.target != "/panic", "a registry bug");
            Answer { status: 403, headers: vec![], body: b"no".to_vec(), undo: None }
        });
        let o = Arc::clone(&open);
        let t = thread::spawn(move || accept(&listener, &o, &handler));
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        s.write_all(b"GET /panic HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 500 ") && out.contains("Connection: close"), "{out}");

        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let big = 4 * DRAIN_MAX as usize;
        s.write_all(format!("PUT /x HTTP/1.1\r\nHost: x\r\nContent-Length: {big}\r\n\r\n").as_bytes()).unwrap();
        let writer = s.try_clone().unwrap();
        let sending = thread::spawn(move || {
            let mut w = writer;
            let _ = w.write_all(&vec![b'a'; big]);
        });
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let out = String::from_utf8_lossy(&out);
        assert!(out.starts_with("HTTP/1.1 403 ") && out.ends_with("no"), "{out}");
        let _ = sending.join();
        open.stop(addr);
        t.join().unwrap();
    }

    #[test]
    fn a_head_that_does_not_parse_is_refused() {
        let at: SocketAddr = "127.0.0.1:1".parse().unwrap();
        for bad in ["GET /\r\n\r\n", "GET / HTTP/2.0\r\n\r\n", "GET / HTTP/1.1\r\nno colon\r\n\r\n", "GET / HTTP/1.1\r\nHost: x"] {
            assert!(matches!(read_head(&mut bad.as_bytes(), at).unwrap(), Some(Err(()))), "{bad:?}");
        }
        assert!(read_head(&mut &b""[..], at).unwrap().is_none(), "a clean end");
        let (h, v) = read_head(&mut &b"\r\nPUT /x?y HTTP/1.0\r\nA: b \r\n\r\n"[..], at).unwrap().unwrap().unwrap();
        assert_eq!((h.method.as_str(), h.target.as_str(), v.as_str(), h.headers), ("PUT", "/x?y", "HTTP/1.0", vec![("A".into(), "b".into())]));
    }
}
