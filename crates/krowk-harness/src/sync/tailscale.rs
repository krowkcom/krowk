//! What the local `tailscaled` says of this machine and its tailnet, read
//! through its LocalAPI (R-NET-1): a unix socket on Linux and for the
//! open-source daemon on macOS, 127.0.0.1 behind a password for the macOS
//! app. Two `GET`s and their JSON are all krowk asks of it: `status`, for
//! this node's addresses, its MagicDNS name and its peers, and `whois`, for
//! who is on the far end of a connection. The daemon answers HTTP/1.0 and
//! closes, so a request is one write and one read to the end, with no client
//! library.
//!
//! The tailnet is a transport and nothing more (R-NET-3): nothing read here
//! lets a device in. The relay's ticket and challenge do that on a direct
//! path as on the relay; `whois` can only turn away more.

use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where `tailscaled` listens on Linux.
pub const SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// Where an open-source `tailscaled` on macOS listens: the Homebrew and
/// hand-installed daemons take either path, by version.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_SOCKETS: [&str; 2] = ["/var/run/tailscaled.socket", SOCKET];

/// The three ways to name a LocalAPI krowk found none of, for every error
/// that ends a search.
const OPTIONS: &str = "start Tailscale, name tailscaled's socket in KROWK_TAILSCALE_SOCKET, or name the macOS app's LocalAPI in KROWK_TAILSCALE_LOCALAPI (http://127.0.0.1:<port> with KROWK_TAILSCALE_LOCALAPI_TOKEN, or the path of its sameuserproof file)";

/// Where tailscaled's LocalAPI answers.
#[derive(Clone, PartialEq, Eq)]
pub enum LocalApi {
    /// A unix socket: tailscaled on Linux, or the open-source tailscaled on
    /// macOS.
    Unix(PathBuf),
    /// 127.0.0.1 on a port, behind a password: the macOS app, whose network
    /// extension serves no socket outside its sandbox. The token goes as
    /// HTTP Basic auth with an empty user, as tailscale's own CLI sends it.
    Tcp { port: u16, token: String },
    /// None was found, and why.
    NotFound(String),
}

/// The token is a password: it stays out of logs and panics.
impl std::fmt::Debug for LocalApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalApi::Unix(p) => write!(f, "Unix({})", p.display()),
            LocalApi::Tcp { port, .. } => write!(f, "Tcp(127.0.0.1:{port})"),
            LocalApi::NotFound(why) => write!(f, "NotFound({why})"),
        }
    }
}

impl From<PathBuf> for LocalApi {
    fn from(p: PathBuf) -> Self {
        LocalApi::Unix(p)
    }
}

/// The LocalAPI to ask. `KROWK_TAILSCALE_SOCKET` wins, then
/// `KROWK_TAILSCALE_LOCALAPI`; with neither, Linux asks the default socket
/// and macOS looks for one (see [`discover`]).
pub fn socket(env: &dyn Fn(&str) -> String) -> LocalApi {
    match env("KROWK_TAILSCALE_SOCKET") {
        s if !s.is_empty() => return LocalApi::Unix(PathBuf::from(s)),
        _ => {}
    }
    let api = env("KROWK_TAILSCALE_LOCALAPI");
    if !api.is_empty() {
        return localapi_override(&api, &env("KROWK_TAILSCALE_LOCALAPI_TOKEN")).unwrap_or_else(LocalApi::NotFound);
    }
    #[cfg(target_os = "macos")]
    {
        let home = env("HOME");
        let containers = (!home.is_empty()).then(|| Path::new(&home).join("Library/Group Containers"));
        discover(&MACOS_SOCKETS.map(PathBuf::from), containers.as_deref(), Path::new("/Library/Tailscale"))
    }
    #[cfg(not(target_os = "macos"))]
    LocalApi::Unix(PathBuf::from(SOCKET))
}

/// `KROWK_TAILSCALE_LOCALAPI`: `http://127.0.0.1:<port>` with the token in
/// `KROWK_TAILSCALE_LOCALAPI_TOKEN`, or the path of a sameuserproof file.
/// Only loopback is taken: the token is the Tailscale app's password, and
/// krowk sends it nowhere else.
fn localapi_override(v: &str, token: &str) -> Result<LocalApi, String> {
    let Some((scheme, rest)) = v.split_once("://") else {
        return proof_file(Path::new(v));
    };
    // The value is not echoed: a URL can carry the token in its userinfo.
    let bad = || "KROWK_TAILSCALE_LOCALAPI must be http://127.0.0.1:<port> or the path of a sameuserproof file".to_string();
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':').ok_or_else(bad)?;
    let port: u16 = port.parse().ok().filter(|p| *p != 0).ok_or_else(bad)?;
    if scheme != "http" || !matches!(host, "127.0.0.1" | "localhost") {
        return Err(bad());
    }
    if !valid_token(token) {
        return Err("KROWK_TAILSCALE_LOCALAPI names a port, so KROWK_TAILSCALE_LOCALAPI_TOKEN must hold the app's token".into());
    }
    Ok(LocalApi::Tcp { port, token: token.into() })
}

/// The app's token is hex; anything else would break the request line.
fn valid_token(t: &str) -> bool {
    !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A sameuserproof file's name, as tailscale's `safesocket` writes it: the
/// App Store app's `sameuserproof-<port>-<token>`, or the standalone app's
/// `sameuserproof-<port>`, whose token is the file's contents.
fn parse_proof_name(name: &str) -> Option<(u16, Option<&str>)> {
    let rest = name.strip_prefix("sameuserproof-")?;
    let (port, token) = match rest.split_once('-') {
        Some((p, t)) if valid_token(t) => (p, Some(t)),
        Some(_) => return None,
        None => (rest, None),
    };
    let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
    Some((port, token))
}

/// The port and token a sameuserproof file names. Errors name its
/// directory, never the file: the App Store app's file name is the token.
fn proof_file(path: &Path) -> Result<LocalApi, String> {
    let dir = path.parent().unwrap_or(Path::new("")).display();
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match parse_proof_name(name) {
        Some((port, Some(token))) => Ok(LocalApi::Tcp { port, token: token.into() }),
        Some((port, None)) => {
            let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => format!("the Tailscale app's token in {dir} is readable by the admin group only: {e}"),
                _ => format!("the Tailscale app's token in {dir} could not be read: {e}"),
            })?;
            let token = raw.trim();
            if !valid_token(token) {
                return Err(format!("the Tailscale app's sameuserproof file in {dir} holds no token"));
            }
            Ok(LocalApi::Tcp { port, token: token.into() })
        }
        None => Err(format!("KROWK_TAILSCALE_LOCALAPI names a file in {dir} that is not a sameuserproof file (sameuserproof-<port>-<token>, or sameuserproof-<port> holding the token)")),
    }
}

/// macOS has three Tailscales, and krowk tries them in turn:
/// - the open-source `tailscaled` (Homebrew, or built by hand), on a unix
///   socket at one of `sockets` that answers a connect;
/// - the App Store app, whose network extension writes
///   `sameuserproof-<port>-<token>` into its group container, a directory
///   of `containers` whose name ends `io.tailscale.ipn.macos` (the team id
///   before it is left unmatched);
/// - the standalone app, whose system extension runs as root and writes
///   `ipnport`, a symlink to the port, and `sameuserproof-<port>`, holding
///   the token, into `/Library/Tailscale` (`macsys`), readable by admins.
///
/// These are the files tailscale's own CLI reads (`safesocket_darwin.go`);
/// it finds the App Store one through `lsof` on IPNExtension, which krowk
/// skips for reading the directory it names.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn discover(sockets: &[PathBuf], containers: Option<&Path>, macsys: &Path) -> LocalApi {
    // A socket file outlives a stopped daemon; a refused connect says so
    // at once, so a stale one does not hide a running app.
    if let Some(s) = sockets.iter().find(|s| std::os::unix::net::UnixStream::connect(s).is_ok()) {
        return LocalApi::Unix(s.clone());
    }
    let containers_shown = containers.map(|c| c.display().to_string()).unwrap_or_else(|| "~/Library/Group Containers".into());
    let mut found = Vec::new();
    let mut notes = Vec::new();
    match containers.map(app_store) {
        Some(Ok(apis)) => found.extend(apis),
        Some(Err(e)) => notes.push(e),
        None => {}
    }
    match std::fs::read_link(macsys.join("ipnport")) {
        Ok(port) => match proof_file(&macsys.join(format!("sameuserproof-{}", port.display()))) {
            Ok(api) => found.push(api),
            Err(e) => notes.push(e),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => notes.push(format!("{}: {e}", macsys.join("ipnport").display())),
    }
    // The apps' files outlive them as a socket does its daemon, and the App
    // Store app deletes its old ones only when it next starts: a port is
    // taken only if something answers on it, so a stale file does not hide
    // the other app running.
    let mut dead = Vec::new();
    for api in found {
        if let LocalApi::Tcp { port, .. } = &api {
            if answers(*port) {
                return api;
            }
            dead.push(port.to_string());
        }
    }
    let sockets_shown = sockets.iter().map(|s| s.display().to_string()).collect::<Vec<_>>().join(" or ");
    let mut why = if dead.is_empty() {
        format!("no tailscaled answers at {sockets_shown}, and no Tailscale app's LocalAPI is in {containers_shown} or {}", macsys.display())
    } else {
        format!("no tailscaled answers at {sockets_shown}, and nothing answers on 127.0.0.1:{}, the port the Tailscale app's files in {containers_shown} or {} name: is the app running?", dead.join(" or "), macsys.display())
    };
    for n in notes {
        why = format!("{why} ({n})");
    }
    LocalApi::NotFound(why)
}

/// Whether anything listens on 127.0.0.1 at `port`: refused at once when
/// nothing does.
fn answers(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)).is_ok()
}

/// The App Store app's ports and tokens, from the sameuserproof files in
/// its group container, newest first. A container krowk may not read is an
/// error, so the reason reaches the person rather than "none found".
fn app_store(containers: &Path) -> Result<Vec<LocalApi>, String> {
    let mut found: Vec<(std::time::SystemTime, u16, String)> = Vec::new();
    let entries = match std::fs::read_dir(containers) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{} could not be read: {e}", containers.display())),
    };
    for c in entries.flatten() {
        if !c.file_name().to_str().is_some_and(|n| n.ends_with("io.tailscale.ipn.macos")) {
            continue;
        }
        let files = std::fs::read_dir(c.path()).map_err(|e| format!("the Tailscale app's group container in {} could not be read: {e}", containers.display()))?;
        for f in files.flatten() {
            let Some(name) = f.file_name().to_str().map(str::to_string) else { continue };
            if let Some((port, Some(token))) = parse_proof_name(&name) {
                let at = f.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((at, port, token.to_string()));
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    Ok(found.into_iter().map(|(_, port, token)| LocalApi::Tcp { port, token }).collect())
}

/// An address tailscale hands a node — `100.64.0.0/10` or
/// `fd7a:115c:a1e0::/48` — or loopback, which reaches no one else.
pub fn is_tailnet(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64),
        IpAddr::V6(v6) => v6.is_loopback() || v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// tailscaled writes `null` for a list it has nothing in.
fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Node {
    #[serde(default)]
    pub host_name: String,
    #[serde(default, rename = "DNSName")]
    pub dns_name: String,
    #[serde(default, rename = "TailscaleIPs", deserialize_with = "nullable")]
    pub tailscale_ips: Vec<IpAddr>,
    /// Where tailscaled reaches this node from outside the tailnet: its
    /// LAN address among them.
    #[serde(default, deserialize_with = "nullable")]
    pub addrs: Vec<String>,
    #[serde(default, rename = "UserID")]
    pub user_id: u64,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Status {
    #[serde(default)]
    pub backend_state: String,
    #[serde(default, rename = "Self")]
    pub me: Node,
    #[serde(default)]
    pub peer: Option<std::collections::BTreeMap<String, Node>>,
}

impl Status {
    pub fn running(&self) -> bool {
        self.backend_state == "Running"
    }

    /// This node's MagicDNS name, without the root's dot.
    pub fn magic_dns(&self) -> Option<String> {
        let n = self.me.dns_name.trim_end_matches('.');
        (!n.is_empty()).then(|| n.to_string())
    }

    /// This node's own LAN addresses: the private ones tailscaled reaches
    /// it at, the endpoints on the public internet left out.
    pub fn lan(&self) -> Vec<IpAddr> {
        let mut out: Vec<IpAddr> = self.me.addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()).map(|a| a.ip()).filter(|ip| matches!(ip, IpAddr::V4(v4) if v4.is_private())).collect();
        out.dedup();
        out
    }

    /// This node, as a host keeps it in a session's sealed index: none
    /// unless Tailscale is up and gives it a tailnet address.
    pub fn tailnet(&self) -> Option<Tailnet> {
        let ips: Vec<IpAddr> = self.me.tailscale_ips.iter().copied().filter(is_tailnet).collect();
        (self.running() && !ips.is_empty()).then(|| Tailnet { name: self.me.host_name.clone(), dns_name: self.magic_dns().unwrap_or_default(), ips })
    }

    /// The peer `node` names: by its MagicDNS name, which names its tailnet
    /// too; else, for a machine renamed since, by a tailnet address among
    /// the peers of that same tailnet (the name's domain), since another
    /// tailnet may give the address out as well. Without MagicDNS on either
    /// side, by address alone.
    pub fn peer_for(&self, node: &Tailnet) -> Option<&Node> {
        let dns = |p: &Node| p.dns_name.trim_end_matches('.').to_ascii_lowercase();
        let domain = |name: &str| name.split_once('.').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
        let at = |p: &&Node| p.tailscale_ips.iter().any(|ip| node.ips.contains(ip));
        let mut peers = self.peer.iter().flatten().map(|(_, n)| n);
        if node.dns_name.is_empty() {
            return peers.find(|p| p.dns_name.is_empty() && at(p));
        }
        let name = node.dns_name.to_ascii_lowercase();
        let mut by_address = None;
        for p in peers {
            if dns(p) == name {
                return Some(p);
            }
            if by_address.is_none() && domain(&dns(p)) == domain(&name) && at(&p) {
                by_address = Some(p);
            }
        }
        by_address
    }
}

/// A machine's tailnet node, as its host keeps it in a session's sealed
/// index for `krowk hosts` to find on the tailnet.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tailnet {
    /// Tailscale's name for it.
    pub name: String,
    /// Its MagicDNS name, without the root's dot; empty without MagicDNS.
    #[serde(default)]
    pub dns_name: String,
    pub ips: Vec<IpAddr>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Profile {
    #[serde(default, rename = "ID")]
    pub id: u64,
    #[serde(default)]
    pub login_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct WhoIs {
    #[serde(default)]
    pub user_profile: Profile,
}

fn get(api: &LocalApi, path: &str) -> Result<Vec<u8>, String> {
    let timeout = Some(Duration::from_secs(2));
    // The Host tailscaled's own CLI sends: the LocalAPI refuses others.
    let head = format!("GET {path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\n");
    match api {
        LocalApi::Unix(socket) => {
            let s = std::os::unix::net::UnixStream::connect(socket).map_err(|e| format!("tailscaled is not answering at {}: {e} — {OPTIONS}", socket.display()))?;
            s.set_read_timeout(timeout).map_err(|e| e.to_string())?;
            s.set_write_timeout(timeout).map_err(|e| e.to_string())?;
            exchange(s, path, &format!("{head}\r\n"))
        }
        LocalApi::Tcp { port, token } => {
            use base64::Engine;
            // Loopback and nowhere else, whatever named the port.
            let at = SocketAddr::from(([127, 0, 0, 1], *port));
            let s = std::net::TcpStream::connect_timeout(&at, Duration::from_secs(2)).map_err(|e| format!("the Tailscale app is not answering at {at}: {e} — {OPTIONS}"))?;
            s.set_read_timeout(timeout).map_err(|e| e.to_string())?;
            s.set_write_timeout(timeout).map_err(|e| e.to_string())?;
            // Padded, as Go's SetBasicAuth writes it and its BasicAuth reads.
            let auth = base64::engine::general_purpose::STANDARD.encode(format!(":{token}"));
            exchange(s, path, &format!("{head}Authorization: Basic {auth}\r\nSec-Tailscale: localapi\r\n\r\n"))
        }
        LocalApi::NotFound(why) => Err(format!("{why} — {OPTIONS}")),
    }
}

/// One request written, the answer read to its end: tailscaled answers
/// HTTP/1.0 and closes, on a socket as on TCP.
fn exchange(mut s: impl Read + Write, path: &str, request: &str) -> Result<Vec<u8>, String> {
    s.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.take(8 << 20).read_to_end(&mut raw).map_err(|e| format!("tailscaled's answer could not be read: {e}"))?;
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("tailscaled answered with no HTTP header")?;
    let head = String::from_utf8_lossy(&raw[..at]);
    let code = head.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(format!("tailscaled answered {path} with {code}: {}", String::from_utf8_lossy(&raw[at + 4..]).trim()));
    }
    Ok(raw[at + 4..].to_vec())
}

/// This node, its addresses and its peers.
pub fn status(api: &LocalApi) -> Result<Status, String> {
    serde_json::from_slice(&get(api, "/localapi/v0/status")?).map_err(|e| format!("tailscaled's status is not what krowk reads: {e}"))
}

/// Who holds the tailnet node at `peer`.
pub fn whois(api: &LocalApi, peer: SocketAddr) -> Result<WhoIs, String> {
    serde_json::from_slice(&get(api, &format!("/localapi/v0/whois?addr={peer}"))?).map_err(|e| format!("tailscaled's whois is not what krowk reads: {e}"))
}

/// The optional hardening (R-NET-3): a direct connection is let in only
/// when tailscaled names its far end as this tailnet user. Any error —
/// tailscaled gone, a peer it does not know — turns it away.
#[derive(Debug, Clone)]
pub struct SameUser {
    pub socket: LocalApi,
    pub user: u64,
}

impl SameUser {
    pub fn admits(&self, peer: SocketAddr) -> bool {
        whois(&self.socket, peer).is_ok_and(|w| w.user_profile.id != 0 && w.user_profile.id == self.user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{"BackendState":"Running","Self":{"HostName":"a","DNSName":"a.tail1.ts.net.","TailscaleIPs":["100.64.0.1","fd7a::1"],"Addrs":["84.15.112.34:1257","192.168.1.147:41641"],"UserID":7,"Online":true},
        "Peer":{"k1":{"HostName":"b","DNSName":"b.tail1.ts.net.","TailscaleIPs":["100.64.0.2"],"Online":false,"Tags":["tag:ops"]},"k2":{"HostName":"phone","Tags":null,"Addrs":null,"TailscaleIPs":null}}}"#;

    /// R-NET-1, R-NET-4: the status as tailscaled writes it gives this
    /// node's tailnet address, its MagicDNS name and its LAN address; this
    /// node as a host keeps it; and a peer found again from what a host
    /// kept, whatever tags it carries.
    #[test]
    fn r_net_1_status_reads_the_addresses_and_r_net_4_finds_a_kept_node_among_the_peers() {
        let s: Status = serde_json::from_str(STATUS).unwrap();
        assert!(s.running());
        assert_eq!(s.me.tailscale_ips[0], "100.64.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(s.magic_dns().as_deref(), Some("a.tail1.ts.net"));
        assert_eq!(s.lan(), vec!["192.168.1.147".parse::<IpAddr>().unwrap()]);
        let me = s.tailnet().unwrap();
        assert_eq!((me.name.as_str(), me.dns_name.as_str()), ("a", "a.tail1.ts.net"));
        assert_eq!(me.ips, ["100.64.0.1".parse::<IpAddr>().unwrap()], "fd7a::1 is no tailnet address");

        let b = |dns: &str, ip: &str| Tailnet { name: "b".into(), dns_name: dns.into(), ips: vec![ip.parse().unwrap()] };
        assert_eq!(s.peer_for(&b("B.tail1.ts.net", "100.64.9.9")).map(|p| p.host_name.as_str()), Some("b"), "by MagicDNS name, which outlives an address");
        assert!(s.peer_for(&b("b.tail2.ts.net", "100.64.0.2")).is_none(), "the same address on another tailnet is another machine");
        assert_eq!(s.peer_for(&b("old-name.tail1.ts.net", "100.64.0.2")).map(|p| p.host_name.as_str()), Some("b"), "renamed since, at the same address on the same tailnet");
        assert!(s.peer_for(&b("", "100.64.0.2")).is_none(), "a peer with a MagicDNS name is not matched by address alone");
        let down: Status = serde_json::from_str(&STATUS.replace("Running", "Stopped")).unwrap();
        assert!(down.tailnet().is_none(), "nothing kept while Tailscale is down");
    }

    /// A stand-in for the macOS app's LocalAPI: 127.0.0.1 on a port, and
    /// the password checked as tailscaled checks it — none is 401, a wrong
    /// one 403.
    fn fake_app(token: &'static str) -> u16 {
        use base64::Engine;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&raw).to_string();
                let auth = req.lines().find_map(|l| l.strip_prefix("Authorization: Basic ")).map(str::trim);
                let want = base64::engine::general_purpose::STANDARD.encode(format!(":{token}"));
                let answer = match auth {
                    None => "HTTP/1.0 401 Unauthorized\r\n\r\nauth required".to_string(),
                    Some(a) if a != want => "HTTP/1.0 403 Forbidden\r\n\r\nbad password".to_string(),
                    Some(_) if !req.contains("\r\nHost: local-tailscaled.sock\r\n") => "HTTP/1.0 403 Forbidden\r\n\r\ninvalid localapi request".to_string(),
                    Some(_) => format!("HTTP/1.0 200 OK\r\n\r\n{STATUS}"),
                };
                let _ = c.write_all(answer.as_bytes());
            }
        });
        port
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> String + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()).unwrap_or_default()
    }

    /// R-NET-1: the macOS app's LocalAPI is read over TCP with its token as
    /// the Basic-auth password, and a wrong token is turned away.
    #[test]
    fn r_net_1_the_macos_app_answers_over_tcp_with_its_token_and_refuses_another() {
        let port = fake_app("2ae2ec9e0aa2005784f1");
        let s = status(&LocalApi::Tcp { port, token: "2ae2ec9e0aa2005784f1".into() }).unwrap();
        assert_eq!(s.magic_dns().as_deref(), Some("a.tail1.ts.net"));
        let e = status(&LocalApi::Tcp { port, token: "0000000000".into() }).unwrap_err();
        assert!(e.contains("403"), "{e}");
    }

    /// Only an address tailscale hands out, or loopback, is a tailnet
    /// address: a LocalAPI anyone could stand in for cannot widen the
    /// listener to every network.
    #[test]
    fn only_tailnet_and_loopback_addresses_are_listened_on() {
        for ok in ["100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::1", "127.0.0.1", "::1"] {
            assert!(is_tailnet(&ok.parse().unwrap()), "{ok}");
        }
        for bad in ["0.0.0.0", "::", "100.63.255.255", "100.128.0.1", "192.168.1.2", "fd7a:115c:a1e1::1"] {
            assert!(!is_tailnet(&bad.parse().unwrap()), "{bad}");
        }
    }

    /// The token is a password: debug output leaves it out.
    #[test]
    fn the_token_stays_out_of_debug_output() {
        assert_eq!(format!("{:?}", LocalApi::Tcp { port: 5, token: "secret".into() }), "Tcp(127.0.0.1:5)");
    }

    /// sameuserproof names as tailscale's safesocket writes them: the App
    /// Store app's carries the port and token, the standalone app's the
    /// port alone.
    #[test]
    fn sameuserproof_names_parse_to_a_port_and_a_token() {
        assert_eq!(parse_proof_name("sameuserproof-61577-2ae2ec9e0aa2005784f1"), Some((61577, Some("2ae2ec9e0aa2005784f1"))));
        assert_eq!(parse_proof_name("sameuserproof-61577"), Some((61577, None)));
        for bad in ["sameuserproof-abc", "sameuserproof-0-aa", "sameuserproof-61577-", "sameuserproof-61577-a b", "ipnport", "sameuserproof-99999-aa"] {
            assert_eq!(parse_proof_name(bad), None, "{bad}");
        }
    }

    /// The overrides: KROWK_TAILSCALE_SOCKET before KROWK_TAILSCALE_LOCALAPI
    /// before discovery, and a LocalAPI anywhere but loopback refused.
    #[test]
    fn overrides_take_precedence_and_only_loopback_is_taken() {
        let both = [("KROWK_TAILSCALE_SOCKET", "/s.sock"), ("KROWK_TAILSCALE_LOCALAPI", "http://127.0.0.1:41112"), ("KROWK_TAILSCALE_LOCALAPI_TOKEN", "abc")];
        assert_eq!(socket(&env(&both)), LocalApi::Unix("/s.sock".into()));
        assert_eq!(socket(&env(&both[1..])), LocalApi::Tcp { port: 41112, token: "abc".into() });
        assert_eq!(socket(&env(&[("KROWK_TAILSCALE_LOCALAPI", "http://localhost:41112/"), ("KROWK_TAILSCALE_LOCALAPI_TOKEN", "abc")])), LocalApi::Tcp { port: 41112, token: "abc".into() });
        assert_eq!(socket(&env(&[("KROWK_TAILSCALE_LOCALAPI", "/x/Group Containers/W.io.tailscale.ipn.macos/sameuserproof-41112-abc")])), LocalApi::Tcp { port: 41112, token: "abc".into() });
        for (url, token) in [("http://10.0.0.1:41112", "abc"), ("https://127.0.0.1:41112", "abc"), ("http://127.0.0.1", "abc"), ("http://127.0.0.1:41112", ""), ("http://127.0.0.1:41112", "a\r\nb")] {
            let api = socket(&env(&[("KROWK_TAILSCALE_LOCALAPI", url), ("KROWK_TAILSCALE_LOCALAPI_TOKEN", token)]));
            assert!(matches!(api, LocalApi::NotFound(_)), "{url} {token:?}: {api:?}");
            assert!(!status(&api).unwrap_err().contains("a\r\nb"));
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(socket(&env(&[])), LocalApi::Unix(SOCKET.into()));
    }

    /// macOS discovery, pointed at a fake home and /Library: a socket that
    /// answers first, then the App Store app's group container (its newest
    /// file), then the standalone app's ipnport and token file.
    #[test]
    fn macos_discovery_finds_the_daemon_then_the_app_store_app_then_the_standalone_app() {
        let root = std::env::temp_dir().join(format!("krowk-ts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (containers, macsys) = (root.join("home/Library/Group Containers"), root.join("Library/Tailscale"));
        std::fs::create_dir_all(&macsys).unwrap();
        let stale = root.join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        let sockets = [root.join("none.sock"), stale.clone()];

        let nothing = discover(&sockets, Some(&containers), &macsys);
        let LocalApi::NotFound(why) = &nothing else { panic!("{nothing:?}") };
        assert!(why.contains("none.sock") && why.contains("Group Containers") && why.contains("Library/Tailscale"), "{why}");
        let e = status(&nothing).unwrap_err();
        for option in ["start Tailscale", "KROWK_TAILSCALE_SOCKET", "KROWK_TAILSCALE_LOCALAPI"] {
            assert!(e.contains(option), "{option}: {e}");
        }

        // Each app's port is a live listener: a port nothing answers on is
        // a stale file, and is passed over.
        let live = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (standalone, store, older) = (live(), live(), live());
        let port = |l: &std::net::TcpListener| l.local_addr().unwrap().port();
        let gone = {
            let l = live();
            port(&l)
        };

        std::os::unix::fs::symlink(port(&standalone).to_string(), macsys.join("ipnport")).unwrap();
        std::fs::write(macsys.join(format!("sameuserproof-{}", port(&standalone))), "feedface\n").unwrap();
        assert_eq!(discover(&sockets, Some(&containers), &macsys), LocalApi::Tcp { port: port(&standalone), token: "feedface".into() });

        // The App Store app's leftover file, from before it quit, does not
        // hide the standalone app running.
        let app = containers.join("W5364U7YZB.group.io.tailscale.ipn.macos");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(containers.join("other.app")).unwrap();
        std::fs::write(containers.join("other.app/sameuserproof-1-dead"), "").unwrap();
        std::fs::write(app.join(format!("sameuserproof-{gone}-0ld")), "").unwrap();
        assert_eq!(discover(&sockets, Some(&containers), &macsys), LocalApi::Tcp { port: port(&standalone), token: "feedface".into() });

        // The App Store app running wins, by its newest file.
        // Timestamps set, not slept for: a filesystem with one-second ones
        // would tie them.
        let at = |secs| std::time::UNIX_EPOCH + Duration::from_secs(secs);
        std::fs::File::create(app.join(format!("sameuserproof-{}-0lder", port(&older)))).unwrap().set_modified(at(1_000)).unwrap();
        std::fs::File::create(app.join(format!("sameuserproof-{}-2ae2ec9e0aa2005784f1", port(&store)))).unwrap().set_modified(at(2_000)).unwrap();
        assert_eq!(discover(&sockets, Some(&containers), &macsys), LocalApi::Tcp { port: port(&store), token: "2ae2ec9e0aa2005784f1".into() });

        // Every app quit: the error says which port went unanswered.
        drop((standalone, store, older));
        let LocalApi::NotFound(why) = discover(&sockets, Some(&containers), &macsys) else { panic!("an app's port answered with every app quit") };
        assert!(why.contains(&gone.to_string()) && why.contains("is the app running?"), "{why}");

        let live = root.join("live.sock");
        let _l = std::os::unix::net::UnixListener::bind(&live).unwrap();
        assert_eq!(discover(&[stale, live.clone()], Some(&containers), &macsys), LocalApi::Unix(live));
        let _ = std::fs::remove_dir_all(&root);
    }
}
