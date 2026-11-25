//! The open way out for an attended Claude Code session in its sandbox.
//!
//! Claude Code's sandbox sends every sandboxed command's network through a
//! proxy that asks before each new domain, and its settings have no way to
//! allow them all. An attended session is sandboxed for one reason, to keep
//! it out of the gruntas home (`launch::claude_sandbox`), not to fence its
//! network, which the person's own `claude` reaches freely. So `gruntas run
//! claude` starts this pass-through proxy beside the harness and names its
//! ports in `sandbox.network.httpProxyPort` and `socksProxyPort`, which
//! Claude Code then uses instead of its own: an HTTP proxy (`CONNECT`, and
//! plain `http://` requests) and a SOCKS5 one (`CONNECT`, no
//! authentication), each on `127.0.0.1` at a port the system picks, each
//! connection on a thread of its own, for as long as gruntas runs.
//!
//! It opens nothing a sandboxed command lacks without the sandbox: the
//! gruntas daemon listens on no TCP port, and a destination on this
//! machine is one any of the person's processes can already reach.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

/// The two ports, as `launch` puts them into Claude Code's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Egress {
    pub http: u16,
    pub socks: u16,
}

/// How long a destination may take to answer a connect.
const CONNECT: Duration = Duration::from_secs(30);
/// The most a request's head may hold before its first blank line.
const HEAD: usize = 64 * 1024;

/// Both proxies, listening, each served by a thread that ends with the
/// process.
pub fn start() -> io::Result<Egress> {
    let http = TcpListener::bind("127.0.0.1:0")?;
    let socks = TcpListener::bind("127.0.0.1:0")?;
    let ports = Egress { http: http.local_addr()?.port(), socks: socks.local_addr()?.port() };
    serve(http, "gruntas-egress-http", http_conn)?;
    serve(socks, "gruntas-egress-socks", socks_conn)?;
    Ok(ports)
}

fn serve(listener: TcpListener, name: &str, conn: fn(TcpStream) -> io::Result<()>) -> io::Result<()> {
    thread::Builder::new().name(name.into()).spawn(move || {
        for client in listener.incoming().flatten() {
            let _ = thread::Builder::new().spawn(move || conn(client));
        }
    })?;
    Ok(())
}

fn dial(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Bytes both ways until either side ends.
fn splice(a: TcpStream, b: TcpStream) -> io::Result<()> {
    let (mut a2, mut b2) = (a.try_clone()?, b.try_clone()?);
    let up = thread::Builder::new().spawn(move || {
        let _ = io::copy(&mut a2, &mut b2);
        let _ = b2.shutdown(Shutdown::Write);
    })?;
    let (mut a, mut b) = (a, b);
    let _ = io::copy(&mut b, &mut a);
    let _ = a.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// The request's head up to its blank line, and whatever came after it.
fn head(c: &mut TcpStream) -> io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            return Ok((String::from_utf8_lossy(&buf).into_owned(), rest));
        }
        if buf.len() > HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too long"));
        }
        let n = c.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// `host:port`, `[v6]:port`, or a host alone on `default`.
fn host_port(s: &str, default: u16) -> Option<(String, u16)> {
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') || h.ends_with(']') => (h, p.parse().ok()?),
        _ => (s, default),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| (host.to_string(), port))
}

fn http_conn(mut c: TcpStream) -> io::Result<()> {
    let (head, rest) = head(&mut c)?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut words = first.split(' ');
    let (method, target, version) = (words.next().unwrap_or_default(), words.next().unwrap_or_default(), words.next().unwrap_or("HTTP/1.1"));
    let refuse = |c: &mut TcpStream, code: &str| c.write_all(format!("HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = host_port(target, 443) else { return refuse(&mut c, "400 Bad Request") };
        let Ok(mut up) = dial(&host, port) else { return refuse(&mut c, "502 Bad Gateway") };
        c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        up.write_all(&rest)?;
        return splice(c, up);
    }
    // A plain request names its whole URL: sent on in origin form, one
    // request to the connection, without the proxy's own headers.
    let Some(url) = target.strip_prefix("http://") else { return refuse(&mut c, "400 Bad Request") };
    let (authority, path) = url.find('/').map_or((url, "/"), |i| (&url[..i], &url[i..]));
    let Some((host, port)) = host_port(authority, 80) else { return refuse(&mut c, "400 Bad Request") };
    let Ok(mut up) = dial(&host, port) else { return refuse(&mut c, "502 Bad Gateway") };
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines.filter(|l| !l.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().trim().to_ascii_lowercase();
        if !matches!(name.as_str(), "connection" | "proxy-connection" | "proxy-authorization" | "keep-alive") {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    up.write_all(out.as_bytes())?;
    up.write_all(&rest)?;
    splice(c, up)
}

fn socks_conn(mut c: TcpStream) -> io::Result<()> {
    let mut b = [0u8; 2];
    c.read_exact(&mut b)?;
    if b[0] != 5 {
        return Ok(());
    }
    let mut methods = vec![0u8; b[1] as usize];
    c.read_exact(&mut methods)?;
    if !methods.contains(&0) {
        return c.write_all(&[5, 0xff]);
    }
    c.write_all(&[5, 0])?;
    let mut req = [0u8; 4];
    c.read_exact(&mut req)?;
    let reply = |c: &mut TcpStream, code: u8| c.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]);
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            c.read_exact(&mut a)?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut n = [0u8; 1];
            c.read_exact(&mut n)?;
            let mut name = vec![0u8; n[0] as usize];
            c.read_exact(&mut name)?;
            String::from_utf8_lossy(&name).into_owned()
        }
        4 => {
            let mut a = [0u8; 16];
            c.read_exact(&mut a)?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => return reply(&mut c, 8),
    };
    let mut p = [0u8; 2];
    c.read_exact(&mut p)?;
    if req[1] != 1 {
        return reply(&mut c, 7);
    }
    let Ok(up) = dial(&host, u16::from_be_bytes(p)) else { return reply(&mut c, 5) };
    reply(&mut c, 0)?;
    splice(c, up)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One echo server on this machine, its port.
    fn echo() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                thread::spawn(move || {
                    let mut r = s.try_clone().unwrap();
                    let _ = io::copy(&mut r, &mut s);
                });
            }
        });
        port
    }

    fn read_to(c: &mut TcpStream, end: &[u8]) -> Vec<u8> {
        let mut got = Vec::new();
        let mut b = [0u8; 1];
        while !got.ends_with(end) {
            c.read_exact(&mut b).unwrap();
            got.push(b[0]);
        }
        got
    }

    #[test]
    fn host_ports_read_as_written() {
        assert_eq!(host_port("example.com:8443", 443), Some(("example.com".into(), 8443)));
        assert_eq!(host_port("example.com", 80), Some(("example.com".into(), 80)));
        assert_eq!(host_port("[::1]:22", 443), Some(("::1".into(), 22)));
        assert_eq!(host_port("[::1]", 443), Some(("::1".into(), 443)));
        assert_eq!(host_port(":443", 443), None);
        assert_eq!(host_port("example.com:x", 443), None);
    }

    #[test]
    fn http_connect_reaches_any_destination() {
        let e = start().unwrap();
        let dest = echo();
        let mut c = TcpStream::connect(("127.0.0.1", e.http)).unwrap();
        c.write_all(format!("CONNECT 127.0.0.1:{dest} HTTP/1.1\r\nHost: x\r\n\r\nearly").as_bytes()).unwrap();
        assert!(String::from_utf8(read_to(&mut c, b"\r\n\r\n")).unwrap().starts_with("HTTP/1.1 200"));
        assert_eq!(read_to(&mut c, b"early"), b"early", "what came with the head goes on");
        c.write_all(b"ping").unwrap();
        assert_eq!(read_to(&mut c, b"ping"), b"ping");
    }

    #[test]
    fn a_plain_request_goes_on_in_origin_form_without_proxy_headers() {
        let e = start().unwrap();
        let dest = echo();
        let mut c = TcpStream::connect(("127.0.0.1", e.http)).unwrap();
        c.write_all(format!("GET http://127.0.0.1:{dest}/a?b HTTP/1.1\r\nHost: 127.0.0.1\r\nProxy-Connection: keep-alive\r\n\r\n").as_bytes()).unwrap();
        let got = String::from_utf8(read_to(&mut c, b"\r\n\r\n")).unwrap();
        assert_eq!(got, "GET /a?b HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    }

    #[test]
    fn socks5_connects_by_name_and_refuses_what_it_does_not_speak() {
        let e = start().unwrap();
        let dest = echo();
        let mut c = TcpStream::connect(("127.0.0.1", e.socks)).unwrap();
        c.write_all(&[5, 1, 0]).unwrap();
        let mut m = [0u8; 2];
        c.read_exact(&mut m).unwrap();
        assert_eq!(m, [5, 0]);
        let mut req = vec![5, 1, 0, 3, 9];
        req.extend_from_slice(b"localhost");
        req.extend_from_slice(&dest.to_be_bytes());
        c.write_all(&req).unwrap();
        let mut r = [0u8; 10];
        c.read_exact(&mut r).unwrap();
        assert_eq!(r[1], 0);
        c.write_all(b"ping").unwrap();
        assert_eq!(read_to(&mut c, b"ping"), b"ping");

        let mut c = TcpStream::connect(("127.0.0.1", e.socks)).unwrap();
        c.write_all(&[5, 1, 2]).unwrap();
        c.read_exact(&mut m).unwrap();
        assert_eq!(m, [5, 0xff], "only no authentication");
        let mut c = TcpStream::connect(("127.0.0.1", e.socks)).unwrap();
        c.write_all(&[5, 1, 0, 5, 2, 0, 1, 127, 0, 0, 1, 0, 80]).unwrap();
        let mut r = [0u8; 12];
        c.read_exact(&mut r[..2]).unwrap();
        c.read_exact(&mut r[2..]).unwrap();
        assert_eq!(r[3], 7, "BIND is not supported");
    }
}
