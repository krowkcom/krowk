//! Direct paths (R-NET-1, R-NET-2, R-NET-3; engineering/harness.md →
//! Direct paths): the host's bridge runs a relay of its own on its tailnet
//! address — and its LAN address — joins it as host beside the relay it is
//! already on, and seals every batch once for both. Its viewers learn the
//! addresses from the welcome, sealed to them under the session key, and
//! each moves to the first that completes the same join and the same
//! handshake the relay path did.
//!
//! It is the reference relay, whole: a direct connection is admitted only by
//! a registry ticket and the challenge signed with the device's own signing
//! key, over the address it dialed. Being on the tailnet admits nobody.
//! Its links are numbered from `FIRST_LINK`, past any the relay hands out,
//! so the host's one set of chains never takes a direct viewer for a
//! relayed one.

use super::tailscale::{self, SameUser};
use crate::relay::{self, Limits, Roster};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, TcpListener};
use tokio::sync::watch;

/// Where a direct listener numbers its links from.
pub const FIRST_LINK: u64 = 1 << 40;

/// How the bridge offers direct paths.
#[derive(Clone)]
pub struct Config {
    /// tailscaled's LocalAPI.
    pub socket: tailscale::LocalApi,
    /// The registry's ticket-signing keys, as the relay has them.
    pub roster: Roster,
    /// Also require tailscaled to name the far end as this tailnet user.
    pub same_user: bool,
    /// Offer this machine's LAN address too (`KROWK_DIRECT_LAN=1`); never
    /// with `same_user`.
    pub lan: bool,
    /// Stops the listener when it turns true; without it the listener goes
    /// with the bridge.
    pub stop: Option<watch::Receiver<bool>>,
}

/// One address a viewer may dial the host at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub url: String,
    /// `tailscale` (the tailnet address), `magicdns` or `lan`.
    pub via: String,
}

impl Candidate {
    /// How a path over it is named on screen.
    pub fn path(&self) -> &'static str {
        if self.via == "lan" { "direct over LAN" } else { "direct over Tailscale" }
    }
}

/// A listener running: where the bridge dials it, and what it offers.
pub struct Listening {
    pub dial: String,
    pub candidates: Vec<Candidate>,
    stop: Option<watch::Sender<bool>>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        if let Some(s) = &self.stop {
            let _ = s.send(true);
        }
    }
}

/// Reads this node from tailscaled and listens on its tailnet address (and
/// LAN address), on one port. Errors say why there is no direct path; the
/// session then goes by the relay alone.
pub fn listen(c: &Config, session: [u8; 16], host: krowk_client::e2e::DeviceId) -> Result<Listening, String> {
    let status = tailscale::status(&c.socket)?;
    if !status.running() {
        return Err(if status.backend_state.is_empty() { "Tailscale isn't running".into() } else { format!("Tailscale isn't running: it is {}", status.backend_state) });
    }
    // Only a tailnet address is listened on, whatever the LocalAPI says: on
    // macOS it is a port on 127.0.0.1 that anyone may bind once the app
    // quits, and an answer of 0.0.0.0 would put plain ws:// on every
    // network, past KROWK_DIRECT_LAN.
    let ips: Vec<IpAddr> = status.me.tailscale_ips.iter().copied().filter(tailscale::is_tailnet).collect();
    let ip = ips.iter().find(|i| i.is_ipv4()).or(ips.first()).copied().ok_or("Tailscale gives this machine no tailnet address")?;
    let whois = if c.same_user {
        if status.me.user_id == 0 {
            return Err("tailscale names no user for this machine, so the same-user check cannot pass".into());
        }
        Some(SameUser { socket: c.socket.clone(), user: status.me.user_id })
    } else {
        None
    };
    let first = TcpListener::bind((ip, 0)).map_err(|e| format!("the tailnet address {ip} could not be listened on: {e}"))?;
    let port = first.local_addr().map_err(|e| e.to_string())?.port();
    let url = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => format!("ws://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("ws://{v4}:{port}"),
    };
    let dial = url(ip);
    let mut candidates = vec![Candidate { url: dial.clone(), via: "tailscale".into() }];
    if let Some(name) = status.magic_dns() {
        candidates.push(Candidate { url: format!("ws://{name}:{port}"), via: "magicdns".into() });
    }
    let mut listeners = vec![first];
    // Never with the same-user check: tailscaled knows no one on the LAN,
    // so every LAN connection would be turned away.
    if c.lan && !c.same_user {
        for lan in status.lan().into_iter().filter(|l| *l != ip) {
            // The same port, so a viewer's candidates name one listener; an
            // address taken there is simply not offered.
            if let Ok(l) = TcpListener::bind((lan, port)) {
                listeners.push(l);
                candidates.push(Candidate { url: url(lan), via: "lan".into() });
            }
        }
    }
    let config = relay::Config { roster: c.roster.clone(), origin: None, limits: Limits { first_link: FIRST_LINK, ..Limits::default() }, state: None, origins: candidates.iter().map(|c| c.url.clone()).collect(), whois, pin: Some((session, host)) };
    let (stop, rx) = match c.stop.clone() {
        Some(rx) => (None, rx),
        None => {
            let (tx, rx) = watch::channel(false);
            (Some(tx), rx)
        }
    };
    std::thread::spawn(move || {
        if let Err(e) = relay::run_until(listeners, config, rx) {
            eprintln!("krowk: the direct listener stopped: {e}");
        }
    });
    Ok(Listening { dial, candidates, stop })
}
