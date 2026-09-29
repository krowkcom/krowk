//! What the local `tailscaled` says of this machine and its tailnet, read
//! through its LocalAPI on the unix socket (R-NET-1). Two `GET`s and their
//! JSON are all krowk asks of it: `status`, for this node's addresses, its
//! MagicDNS name and its peers, and `whois`, for who is on the far end of a
//! connection. The daemon answers HTTP/1.0 and closes, so a request is one
//! write and one read to the end, with no client library.
//!
//! The tailnet is a transport and nothing more (R-NET-3): nothing read here
//! lets a device in. The relay's ticket and challenge do that on a direct
//! path as on the relay; `whois` can only turn away more.

use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where `tailscaled` listens on Linux; macOS's app and a custom
/// `--socket` are named by `KROWK_TAILSCALE_SOCKET`.
pub const SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// The tag a machine carries to be listed by `krowk hosts` (R-NET-4).
pub const HOST_TAG: &str = "tag:krowk-host";

/// The socket to ask: `KROWK_TAILSCALE_SOCKET`, else the default.
pub fn socket(env: &dyn Fn(&str) -> String) -> PathBuf {
    match env("KROWK_TAILSCALE_SOCKET") {
        s if s.is_empty() => PathBuf::from(SOCKET),
        s => PathBuf::from(s),
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

    /// Peers tagged `tag:krowk-host`, by name.
    pub fn hosts(&self) -> Vec<Node> {
        let mut out: Vec<Node> = self.peer.iter().flatten().map(|(_, n)| n).filter(|n| n.tags.iter().flatten().any(|t| t == HOST_TAG)).cloned().collect();
        out.sort_by(|a, b| a.host_name.cmp(&b.host_name));
        out
    }
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

fn get(socket: &Path, path: &str) -> Result<Vec<u8>, String> {
    let mut s = std::os::unix::net::UnixStream::connect(socket).map_err(|e| format!("tailscaled is not answering at {}: {e}", socket.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    // The Host tailscaled's own CLI sends: the LocalAPI refuses others.
    write!(s, "GET {path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\r\n").map_err(|e| e.to_string())?;
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
pub fn status(socket: &Path) -> Result<Status, String> {
    serde_json::from_slice(&get(socket, "/localapi/v0/status")?).map_err(|e| format!("tailscaled's status is not what krowk reads: {e}"))
}

/// Who holds the tailnet node at `peer`.
pub fn whois(socket: &Path, peer: SocketAddr) -> Result<WhoIs, String> {
    serde_json::from_slice(&get(socket, &format!("/localapi/v0/whois?addr={peer}"))?).map_err(|e| format!("tailscaled's whois is not what krowk reads: {e}"))
}

/// The optional hardening (R-NET-3): a direct connection is let in only
/// when tailscaled names its far end as this tailnet user. Any error —
/// tailscaled gone, a peer it does not know — turns it away.
#[derive(Debug, Clone)]
pub struct SameUser {
    pub socket: PathBuf,
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
        "Peer":{"k1":{"HostName":"b","DNSName":"b.tail1.ts.net.","TailscaleIPs":["100.64.0.2"],"Online":false,"Tags":["tag:krowk-host"]},"k2":{"HostName":"phone","Tags":null,"Addrs":null,"TailscaleIPs":null}}}"#;

    /// R-NET-1, R-NET-4: the status as tailscaled writes it gives this
    /// node's tailnet address, its MagicDNS name and its LAN address, and
    /// the peers tagged `tag:krowk-host`.
    #[test]
    fn r_net_1_status_reads_the_addresses_and_r_net_4_the_tagged_hosts() {
        let s: Status = serde_json::from_str(STATUS).unwrap();
        assert!(s.running());
        assert_eq!(s.me.tailscale_ips[0], "100.64.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(s.magic_dns().as_deref(), Some("a.tail1.ts.net"));
        assert_eq!(s.lan(), vec!["192.168.1.147".parse::<IpAddr>().unwrap()]);
        let hosts = s.hosts();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host_name, "b");
    }
}
