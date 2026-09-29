//! devregistry runs the local stand-in for api.krowk.com, so developing against
//! a registry needs neither the network nor a checkout of the registry itself.
//! Built by no release: `cargo run -p krowk-devregistry`, then
//! `krowk push screenshot.png --dev`.

use krowk_devregistry::Config;
use std::fmt;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs};
use std::process::exit;

/// Where the stand-in listens, matching the CLI's DEV_BASE_URL so --dev finds
/// it with no configuration. Loopback, not ":8787": this registry takes uploads
/// without a key and serves their bytes to anyone who can reach it, so a wider
/// bind has to be asked for.
const DEFAULT_ADDR: &str = "127.0.0.1:8787";

/// The port of DEV_BASE_URL, `http://localhost:8787/v1`.
const DEV_PORT: &str = "8787";

const USAGE: &str = "  -addr string
    \tlisten address (loopback only by default) (default \"127.0.0.1:8787\")
  -limit-bytes int
    \treject uploads above this size
  -site string
    \torigin for the links returned (default: the request host)
";

fn main() {
    let (addr, site, limit_bytes) = flags(std::env::args().collect());
    // The library treats <= 0 as the default, so a negative limit would
    // silently mean 100 MiB. Refused instead of guessed at.
    if limit_bytes < 0 {
        eprintln!("--limit-bytes must not be negative — omit it or pass 0 for the default");
        exit(1);
    }
    if let Err(e) = usable_addr(&addr) {
        eprintln!("{e}");
        exit(1);
    }
    // Bound before anything is announced, so a script keying off the banner
    // never proceeds against a port that failed to open.
    let e = run(&mut io::stdout(), &addr, &site, limit_bytes);
    eprintln!("{e}");
    exit(1);
}

/// Go's `flag` package, for the three flags: `-name value`, `--name=value`,
/// stopping at the first argument that is not a flag.
fn flags(args: Vec<String>) -> (String, String, i64) {
    let prog = args.first().cloned().unwrap_or_else(|| "devregistry".into());
    let fail = |msg: String| -> ! {
        eprint!("{msg}\nUsage of {prog}:\n{USAGE}");
        exit(2)
    };
    let (mut addr, mut site, mut limit) = (DEFAULT_ADDR.to_owned(), String::new(), 0);
    let mut rest = args.into_iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--" || !arg.starts_with('-') || arg == "-" {
            break;
        }
        let name = arg.strip_prefix("--").unwrap_or(&arg[1..]);
        if name.is_empty() || name.starts_with(['-', '=']) {
            fail(format!("bad flag syntax: {arg}"));
        }
        let (name, inline) = name.split_once('=').map_or((name, None), |(n, v)| (n, Some(v.to_owned())));
        if name == "h" || name == "help" {
            eprint!("Usage of {prog}:\n{USAGE}");
            exit(0);
        }
        if !matches!(name, "addr" | "site" | "limit-bytes") {
            fail(format!("flag provided but not defined: -{name}"));
        }
        let Some(value) = inline.or_else(|| rest.next()) else { fail(format!("flag needs an argument: -{name}")) };
        match name {
            "addr" => addr = value,
            "site" => site = value,
            _ => match parse_int(&value) {
                Some(n) => limit = n,
                None => fail(format!("invalid value {value:?} for flag -{name}: parse error")),
            },
        }
    }
    (addr, site, limit)
}

/// `strconv.ParseInt(s, 0, 64)`: a sign, then decimal, or 0x, 0o, 0b or a
/// leading 0 for octal. ponytail: no digit separators.
fn parse_int(s: &str) -> Option<i64> {
    let (neg, digits) = s.strip_prefix('-').map_or((false, s.strip_prefix('+').unwrap_or(s)), |d| (true, d));
    let lower = digits.to_ascii_lowercase();
    let (radix, body) = match lower.as_bytes() {
        [b'0', b'x', ..] => (16, &lower[2..]),
        [b'0', b'o', ..] => (8, &lower[2..]),
        [b'0', b'b', ..] => (2, &lower[2..]),
        [b'0', _, ..] => (8, &lower[1..]),
        _ => (10, lower.as_str()),
    };
    if body.is_empty() || body.starts_with(['+', '-']) {
        return None;
    }
    let n = i128::from_str_radix(body, radix).ok()?;
    i64::try_from(if neg { -n } else { n }).ok()
}

/// A listen that never happened, or a registry that stopped serving.
#[derive(Debug)]
enum RunError {
    Bind(String, io::Error),
    Stopped(String, io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RunError::Bind(addr, e) => write!(f, "could not listen on {addr}: {e}"),
            RunError::Stopped(addr, e) => write!(f, "registry on {addr} stopped: {e}"),
        }
    }
}

/// Binds `addr`, announces the registry on `w`, and serves until it stops. An
/// empty address is a bind failure rather than "every interface", which is
/// exactly what this must never do by accident.
fn run(w: &mut dyn Write, addr: &str, site: &str, limit_bytes: i64) -> RunError {
    if addr.is_empty() {
        return RunError::Bind(addr.to_owned(), io::Error::other("no address given"));
    }
    match bind(addr) {
        Ok(ln) => serve(w, ln, addr, site, limit_bytes),
        Err(e) => RunError::Bind(addr.to_owned(), e),
    }
}

/// net.Listen's reading of an address: an empty host is every interface on
/// both stacks, and a name binds its first address, IPv4 preferred.
fn bind(addr: &str) -> io::Result<TcpListener> {
    let (host, port) = split_host_port(addr).ok_or_else(|| io::Error::other("missing port in address"))?;
    let port: u16 = if port.is_empty() { 0 } else { port.parse().map_err(|_| io::Error::other("invalid port"))? };
    if host.is_empty() {
        return TcpListener::bind(("::", port)).or_else(|_| TcpListener::bind(("0.0.0.0", port)));
    }
    let mut addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    addrs.sort_by_key(|a| !a.is_ipv4());
    TcpListener::bind(*addrs.first().ok_or_else(|| io::Error::other("no such host"))?)
}

fn serve(w: &mut dyn Write, ln: TcpListener, asked: &str, site: &str, limit_bytes: i64) -> RunError {
    let bound = ln.local_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = w.write_all(banner(&bound, asked).as_bytes()).and_then(|_| w.flush());
    let config = Config { limit_bytes, site: site.to_owned(), clock: None, max_sessions: 0 };
    RunError::Stopped(asked.to_owned(), krowk_devregistry::serve(ln, config))
}

/// Where the registry is, how to point a push at it, and — bound wider than
/// this machine — that it is open to anyone who can reach it.
fn banner(bound: &str, asked: &str) -> String {
    let base = local_base(bound, asked);
    let mut lines = vec![format!("krowk registry listening on {base}")];
    if reachable_by_dev(asked) {
        lines.push("  krowk push screenshot.png --dev".into());
    } else {
        // --dev only knows the default address, so say what to use instead.
        lines.push(format!("  KROWK_API_URL={base}/v1 krowk push screenshot.png"));
    }
    let host = listen_host(asked);
    if !is_loopback_host(&host) {
        let what = if matches!(host.as_str(), "" | "0.0.0.0" | "::") { "every interface" } else { &host };
        lines.push(format!("  ! reachable from the network on {what} — it needs no key to accept uploads"));
    }
    lines.join("\n") + "\n"
}

/// A listen address as a URL a client can call: the port from the bind when
/// ":0" asked for any, the name the user typed otherwise, and localhost for a
/// wildcard or loopback IP — the name that reaches either stack.
fn local_base(bound: &str, asked: &str) -> String {
    let Some((host, mut port)) = split_host_port(asked) else { return format!("http://{asked}") };
    if port == "0"
        && let Some((_, p)) = split_host_port(bound)
    {
        port = p;
    }
    let host = if matches!(host.as_str(), "" | "0.0.0.0" | "::" | "127.0.0.1" | "::1") { "localhost".into() } else { host };
    format!("http://{}", join_host_port(&host, &port))
}

/// Whether --dev, which dials localhost on one port, finds a registry here —
/// so the rest of 127.0.0.0/8 does not qualify, loopback though it is.
fn reachable_by_dev(addr: &str) -> bool {
    listen_port(addr) == DEV_PORT && matches!(listen_host(addr).as_str(), "" | "0.0.0.0" | "::" | "localhost" | "127.0.0.1" | "::1")
}

fn listen_host(addr: &str) -> String {
    split_host_port(addr).map(|(h, _)| h).unwrap_or_default()
}

fn listen_port(addr: &str) -> String {
    split_host_port(addr).map(|(_, p)| p).unwrap_or_default()
}

/// Rejects what the bind would reject anyway, so the banner never announces an
/// address that does not bind. `--addr 8787` is the easy mistake.
fn usable_addr(addr: &str) -> Result<(), String> {
    match split_host_port(addr) {
        Some((_, port)) if bindable_port(&port) => Ok(()),
        _ => Err(format!("--addr {addr:?} needs a host and a numeric port, like 127.0.0.1:8787 or :8787")),
    }
}

/// All digits, within range, and spelled the way it binds: ":08787" binds 8787,
/// and "http" would be resolved and then printed verbatim. 0 asks the kernel.
fn bindable_port(port: &str) -> bool {
    port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u32>().is_ok_and(|n| n <= 65535 && n.to_string() == port)
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// Go's `net.SplitHostPort`.
fn split_host_port(hp: &str) -> Option<(String, String)> {
    let i = hp.rfind(':')?;
    let (host, k) = if let Some(inner) = hp.strip_prefix('[') {
        let end = inner.find(']')? + 1;
        if end + 1 != i {
            return None;
        }
        (&hp[1..end], end + 1)
    } else {
        let host = &hp[..i];
        if host.contains(':') {
            return None;
        }
        (host, 0)
    };
    let j = usize::from(hp.starts_with('['));
    if hp[j..].contains('[') || hp[k..].contains(']') {
        return None;
    }
    Some((host.to_owned(), hp[i + 1..].to_owned()))
}

fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
