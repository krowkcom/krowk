//! Direct paths on the LAN without Tailscale (R-NET-2; engineering/harness.md
//! → Direct paths): the host advertises its direct listener over mDNS, and
//! a viewer racing for a direct path looks for it there beside the
//! addresses the welcome named.
//!
//! The service is `_krowk._tcp.local.`, and an instance is named by the
//! session's tag — a hash of its id — so the network learns that some krowk
//! session is hosted at an address, never which. mDNS is a way to find an
//! address and nothing more (R-NET-3): anyone on the LAN can answer for a
//! tag, so an address found this way is joined exactly as a welcome's is,
//! with a registry ticket and the challenge signed over the address dialed,
//! and the session moves onto it only once the host's sealed welcome comes
//! back over it. Only a private IPv4 answer is dialed — loopback only for a
//! viewer whose host is this machine — so an answer cannot send a viewer
//! anywhere off the LAN, nor to another local service.

use super::direct::Candidate;
use mdns_sd::{IfKind, RecvTimeoutError, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

/// The service a host's direct listener is advertised under.
pub const SERVICE: &str = "_krowk._tcp.local.";

/// A session's instance name: the first 16 bytes of
/// `SHA-256("krowk mdns v1" || id)`, in hex.
pub fn tag(session: [u8; 16]) -> String {
    let mut h = Sha256::new();
    h.update(b"krowk mdns v1");
    h.update(session);
    h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// An address mDNS may hand a viewer: a private IPv4 one, which reaches no
/// further than the LAN, or loopback when `loopback` (the host is this
/// machine, as the welcome said).
pub fn on_lan(ip: &IpAddr, loopback: bool) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.is_private() || (loopback && v4.is_loopback()))
}

/// The most addresses one browse hands on: a LAN answering with more is not
/// a host, and each would be a connection and a ticket sent.
pub const MOST: usize = 4;

/// This machine's own LAN addresses, from its interfaces: private IPv4 ones,
/// a tailnet's `100.64.0.0/10` falling outside them, and none on a bridge
/// this machine runs for its containers or virtual machines, which are no
/// LAN. A VPN's private address is taken like any other.
pub fn lan_addresses() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !virtual_bridge(&i.name))
        .map(|i| i.ip())
        .filter(|ip| matches!(ip, IpAddr::V4(v4) if v4.is_private()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// An interface Docker, Podman, libvirt, LXD, Kubernetes' CNIs or a VM
/// host's tools make for what runs on this machine.
fn virtual_bridge(name: &str) -> bool {
    ["docker", "br-", "veth", "virbr", "lxdbr", "lxcbr", "podman", "cni", "flannel", "cali", "vmnet", "vboxnet", "bridge"].iter().any(|p| name.starts_with(p))
}

/// A responder that answers for one session's listener while it lives.
pub struct Advertised {
    daemon: ServiceDaemon,
}

impl Drop for Advertised {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// A responder on every interface, loopback included: that reaches only
/// this machine, and is what lets two processes on it find each other.
fn daemon() -> Result<ServiceDaemon, String> {
    let d = ServiceDaemon::new().map_err(|e| format!("mDNS could not start: {e}"))?;
    let _ = d.enable_interface(IfKind::LoopbackV4);
    Ok(d)
}

/// Advertises `session`'s listener at `addrs`, on `port`.
pub fn advertise(session: [u8; 16], addrs: &[Ipv4Addr], port: u16) -> Result<Advertised, String> {
    let name = tag(session);
    let addrs: Vec<IpAddr> = addrs.iter().copied().map(IpAddr::V4).collect();
    let info = ServiceInfo::new(SERVICE, &name, &format!("{name}.local."), &addrs[..], port, None::<HashMap<String, String>>).map_err(|e| format!("mDNS: {e}"))?;
    let daemon = daemon()?;
    daemon.register(info).map_err(|e| format!("mDNS could not advertise: {e}"))?;
    Ok(Advertised { daemon })
}

/// The candidates one answer for `name` names: each LAN address it gives,
/// on its port.
fn candidates(name: &str, s: &ResolvedService, loopback: bool) -> Vec<Candidate> {
    if s.fullname != format!("{name}.{SERVICE}") || s.port == 0 {
        return Vec::new();
    }
    let mut ips: Vec<IpAddr> = s.get_addresses_v4().into_iter().map(IpAddr::V4).filter(|ip| on_lan(ip, loopback)).collect();
    ips.sort();
    ips.into_iter().map(|ip| Candidate { url: format!("ws://{ip}:{}", s.port), via: "mdns".into() }).collect()
}

/// Looks for `session`'s host for up to `wait`, handing on each address
/// found as it is found, `MOST` at most, loopback ones only with
/// `loopback`; blocking, so run off the runtime. Ends early when `found` is
/// closed: the race it feeds is over.
pub fn browse(session: [u8; 16], wait: Duration, loopback: bool, found: tokio::sync::mpsc::UnboundedSender<Candidate>) {
    let Ok(daemon) = daemon() else { return };
    let name = tag(session);
    if let Ok(rx) = daemon.browse(SERVICE) {
        let mut sent = std::collections::HashSet::new();
        let until = Instant::now() + wait;
        while let Some(left) = until.checked_duration_since(Instant::now()).filter(|l| !l.is_zero()) {
            if found.is_closed() {
                break;
            }
            // Short waits, so a race that is over ends the browse soon.
            match rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(ServiceEvent::ServiceResolved(s)) => {
                    for c in candidates(&name, &s, loopback) {
                        if sent.len() >= MOST {
                            break;
                        }
                        if sent.insert(c.url.clone()) && found.send(c).is_err() {
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    let _ = daemon.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-NET-2: a host's listener advertised on this machine is found by
    /// its session's tag, at its address and port, as an `mdns` candidate;
    /// another session's tag finds nothing.
    #[tokio::test]
    async fn r_net_2_an_advertised_listener_is_found_by_its_sessions_tag_alone() {
        let (session, other) = ([7u8; 16], [8u8; 16]);
        let _a = advertise(session, &[Ipv4Addr::LOCALHOST], 45_123).unwrap();
        let look = |s: [u8; 16], wait| async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::task::spawn_blocking(move || browse(s, wait, true, tx)).await.unwrap();
            let mut got = Vec::new();
            while let Ok(c) = rx.try_recv() {
                got.push(c);
            }
            got
        };
        let got = look(session, Duration::from_secs(3)).await;
        assert!(got.contains(&Candidate { url: "ws://127.0.0.1:45123".into(), via: "mdns".into() }), "{got:?}");
        assert!(look(other, Duration::from_secs(1)).await.is_empty(), "another session's tag was answered");
    }

    /// Container and VM bridges are no LAN; Wi-Fi and Ethernet are.
    #[test]
    fn container_and_vm_bridges_are_not_the_lan() {
        for v in ["docker0", "br-3f2a", "veth12ab", "virbr0", "lxdbr0", "podman0", "cni0", "vboxnet0", "vmnet8", "bridge100"] {
            assert!(virtual_bridge(v), "{v}");
        }
        for lan in ["eth0", "enp3s0", "wlan0", "wlp2s0", "en0", "en1"] {
            assert!(!virtual_bridge(lan), "{lan}");
        }
    }

    /// The tag names the session to nobody: a fixed-length hash, stable
    /// for one id and different for another.
    #[test]
    fn the_tag_is_a_hash_of_the_session_id() {
        let t = tag([1; 16]);
        assert_eq!(t.len(), 32);
        assert_eq!(t, tag([1; 16]));
        assert_ne!(t, tag([2; 16]));
        assert!(!t.contains("01010101"));
    }

    /// R-NET-3: an answer is dialed only at a private IPv4 address, loopback
    /// only for a host on this machine, and only for the tag asked; a
    /// public, link-local or IPv6 address, another instance and port 0
    /// give nothing.
    #[test]
    fn r_net_3_only_lan_answers_for_the_tag_are_candidates() {
        for ok in ["192.168.1.2", "10.0.0.1", "172.16.5.5"] {
            assert!(on_lan(&ok.parse().unwrap(), false), "{ok}");
        }
        assert!(on_lan(&"127.0.0.1".parse().unwrap(), true));
        for bad in ["127.0.0.1", "169.254.1.1", "8.8.8.8", "100.64.0.1", "fe80::1", "::1"] {
            assert!(!on_lan(&bad.parse().unwrap(), false), "{bad}");
        }
        let name = tag([3; 16]);
        let info = |instance: &str, port| ServiceInfo::new(SERVICE, instance, "h.local.", &["192.168.1.9".parse::<IpAddr>().unwrap(), "8.8.8.8".parse().unwrap()][..], port, None::<HashMap<String, String>>).unwrap().as_resolved_service();
        assert_eq!(candidates(&name, &info(&name, 4000), false), [Candidate { url: "ws://192.168.1.9:4000".into(), via: "mdns".into() }]);
        assert!(candidates(&name, &info(&tag([4; 16]), 4000), false).is_empty());
        assert!(candidates(&name, &info(&name, 0), false).is_empty());
    }
}
