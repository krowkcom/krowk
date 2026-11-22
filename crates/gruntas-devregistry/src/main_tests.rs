use super::*;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;

fn post(addr: SocketAddr, path: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn get_status(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    line
}

#[test]
fn local_base_turns_a_listen_address_into_a_url() {
    for (bound, asked, want) in [
        ("[::]:8787", ":8787", "http://localhost:8787"),
        // ":0" means any port, and the bound address says which.
        ("[::]:41234", ":0", "http://localhost:41234"),
        // A name the user typed survives the listener resolving it.
        ("192.0.2.1:9000", "files.internal:9000", "http://files.internal:9000"),
        ("127.0.0.1:41234", "localhost:0", "http://localhost:41234"),
        // Wildcards dial nowhere and loopback IPs fold to the --dev name.
        ("0.0.0.0:8787", "0.0.0.0:8787", "http://localhost:8787"),
        ("[::]:8787", "[::]:8787", "http://localhost:8787"),
        ("127.0.0.1:8787", DEFAULT_ADDR, "http://localhost:8787"),
        ("[::1]:8787", "[::1]:8787", "http://localhost:8787"),
    ] {
        assert_eq!(local_base(bound, asked), want, "local_base({bound:?}, {asked:?})");
    }
}

/// Uploads with no key, bytes back to anyone who can reach it: off-box by
/// default would hand that to whoever shares the network.
#[test]
fn the_local_registry_binds_loopback_by_default() {
    assert!(is_loopback_host(&listen_host(DEFAULT_ADDR)));
    assert!(reachable_by_dev(DEFAULT_ADDR), "--dev cannot reach the default address");
}

#[test]
fn reachable_by_dev_names_only_what_localhost_dials() {
    for (addr, want) in [
        ("127.0.0.1:8787", true),
        ("localhost:8787", true),
        (":8787", true),
        ("0.0.0.0:8787", true),
        ("127.0.0.1:9000", false),
        ("192.168.1.5:8787", false),
        // Loopback, but not where localhost lands.
        ("127.0.0.2:8787", false),
    ] {
        assert_eq!(reachable_by_dev(addr), want, "reachable_by_dev({addr:?})");
    }
}

/// An address that cannot bind must not first be announced as listening.
#[test]
fn an_address_without_a_usable_port_is_rejected() {
    for addr in [
        "8787", "localhost", "127.0.0.1", "127.0.0.1:", ":", "127.0.0.1:http", ":-1", "127.0.0.1:99999", ":65536",
        "127.0.0.1:08787",
    ] {
        assert!(usable_addr(addr).is_err(), "usable_addr({addr:?}) passed");
    }
    for addr in [":8787", "127.0.0.1:8787", "0.0.0.0:9000", ":65535", ":0", "127.0.0.1:0", "[::1]:8787", DEFAULT_ADDR] {
        assert!(usable_addr(addr).is_ok(), "usable_addr({addr:?}) failed");
    }
}

/// Bind-before-banner makes ":0" a feature: the banner names the port the
/// kernel picked, with advice that connects, and a registry answers on it.
#[test]
fn port_zero_serves_on_a_kernel_picked_port() {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap();
    let port = addr.port();
    let text = banner(&addr.to_string(), "127.0.0.1:0");
    assert!(text.contains(&format!("krowk registry listening on http://localhost:{port}\n")), "{text}");
    assert!(text.contains(&format!("KROWK_API_URL=http://localhost:{port}/v1")), "{text}");

    std::thread::spawn(move || serve(&mut io::sink(), ln, "127.0.0.1:0", "", 0));
    assert!(get_status(addr, "/").starts_with("HTTP/1.1 200"));
}

#[test]
fn the_banner_warns_only_when_bound_off_box() {
    let def = banner("127.0.0.1:8787", DEFAULT_ADDR);
    assert!(def.contains("krowk registry listening on http://localhost:8787\n"), "{def}");
    assert!(def.contains("--dev") && !def.contains("reachable from the network"), "{def}");

    for addr in ["0.0.0.0:8787", ":8787", "192.168.1.5:8787"] {
        let got = banner(addr, addr);
        assert!(got.contains("reachable from the network") && got.contains("needs no key"), "{got}");
    }
    assert!(banner(":8787", ":8787").contains("on every interface"));

    let other = banner("127.0.0.1:9000", "127.0.0.1:9000");
    assert!(other.contains("KROWK_API_URL=http://localhost:9000/v1"), "{other}");
}

/// The banner appears only after a successful bind.
#[test]
fn a_failed_bind_prints_no_banner() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let mut out = Vec::new();
    let e = run(&mut out, &addr, "", 0);
    assert!(matches!(e, RunError::Bind(..)), "{e:?}");
    assert!(e.to_string().contains(&addr), "{e}");
    assert!(out.is_empty(), "banner printed despite the failed bind");
}

/// An empty address never reaches run from main, but a caller that passes one
/// gets the bind failure rather than a fallback decided in two places.
#[test]
fn an_empty_addr_fails_to_bind_quietly() {
    let mut out = Vec::new();
    assert!(matches!(run(&mut out, "", "", 0), RunError::Bind(..)));
    assert!(out.is_empty());
}

/// --limit-bytes arrives as the byte ceiling.
#[test]
fn serve_wires_limit_bytes_into_the_registry() {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap();
    std::thread::spawn(move || serve(&mut io::sink(), ln, "x", "", 4));
    let got = post(addr, "/v1/artifacts", r#"{"artifact":{"filename":"shot.png","content_type":"image/png","byte_size":5}}"#);
    assert!(got.starts_with("HTTP/1.1 422"), "{got}");
    assert!(got.contains("\"invalid\"") && got.contains("at most 4 bytes"), "{got}");
}

/// --site rebrands the links only; the upload still lands here.
#[test]
fn serve_wires_site_into_the_registry() {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap();
    std::thread::spawn(move || serve(&mut io::sink(), ln, "x", "https://files.example", 0));
    let got = post(addr, "/v1/artifacts", r#"{"artifact":{"filename":"shot.png","content_type":"image/png","byte_size":5}}"#);
    assert!(got.contains("\"url\": \"https://files.example/a/art_"), "{got}");
}

#[test]
fn a_stopped_registry_says_so() {
    let running = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Config::default()).unwrap();
    let e = RunError::Stopped("127.0.0.1:0".into(), running.stop());
    assert!(e.to_string().contains("stopped"), "{e}");
}

#[test]
fn flags_parse_the_way_go_parses_them() {
    let args = |a: &[&str]| std::iter::once("devregistry").chain(a.iter().copied()).map(String::from).collect();
    assert_eq!(flags(args(&[])), (DEFAULT_ADDR.into(), String::new(), 0));
    assert_eq!(flags(args(&["--addr", ":0", "-limit-bytes=0x10", "--site=https://x"])), (":0".into(), "https://x".into(), 16));
    assert_eq!(parse_int("-5"), Some(-5));
    assert_eq!(parse_int("017"), Some(15));
    assert_eq!(parse_int("abc"), None);
}
