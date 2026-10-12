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
use krowk_client::e2e::DeviceId;
use crate::relay::{self, Limits, Roster};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, TcpListener};
use std::time::{Duration, Instant};
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
    /// Offer this machine's LAN address too (`KROWK_DIRECT_LAN=1`), and
    /// advertise it over mDNS — with Tailscale or without it; never with
    /// `same_user`.
    pub lan: bool,
    /// Where the LAN addresses to offer are read, at the start and every
    /// `LAN_CHECK` after; None for this machine's own: what tailscaled says
    /// when it is up, else its interfaces' private IPv4 addresses.
    pub lan_at: Option<LanAt>,
    /// Stops the listener when it turns true; without it the listener goes
    /// with the bridge.
    pub stop: Option<watch::Receiver<bool>>,
}

/// A source of LAN addresses, for a test to move them.
pub type LanAt = std::sync::Arc<dyn Fn() -> Vec<IpAddr> + Send + Sync>;

/// How often a host offering the LAN reads its LAN addresses again: one it
/// takes up is listened on and advertised over mDNS, one it lost is no
/// longer, so a viewer whose welcome named the old address finds the new
/// one over mDNS.
pub const LAN_CHECK: Duration = Duration::from_secs(5);

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
        if self.on_lan() { "direct over LAN" } else { "direct over Tailscale" }
    }

    /// An address on the LAN: the host's own, or one mDNS answered with.
    pub fn on_lan(&self) -> bool {
        matches!(self.via.as_str(), "lan" | "mdns")
    }
}

/// A listener running: where the bridge dials it, and what it offers.
pub struct Listening {
    pub dial: String,
    pub candidates: Vec<Candidate>,
    stop: Option<watch::Sender<bool>>,
    /// Whether the LAN is still followed (`follow_lan`), and so advertised.
    following: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// What it offers now, the LAN addresses as `follow_lan` last found them.
    now: std::sync::Arc<std::sync::Mutex<Vec<Candidate>>>,
}

impl Listening {
    /// The addresses to name in a welcome: those listened on now, so a
    /// viewer arriving after the machine moved races where it is.
    pub fn candidates_now(&self) -> Vec<Candidate> {
        self.now.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.following.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(s) = &self.stop {
            let _ = s.send(true);
        }
    }
}

/// This node as tailscaled reads it, when Tailscale answers and is up;
/// otherwise why there is no direct path: "Tailscale isn't running" for a
/// node that answered as down, else what asking it gave, as said.
pub fn running(socket: &tailscale::LocalApi) -> Result<tailscale::Status, String> {
    let status = tailscale::status(socket).map_err(|e| format!("Tailscale: {e}"))?;
    if !status.running() {
        return Err(if status.backend_state.is_empty() { "Tailscale isn't running".into() } else { format!("Tailscale isn't running: it is {}", status.backend_state) });
    }
    Ok(status)
}

/// The tailnet user the same-user check holds a connection's far end to:
/// this node's, when a person owns it. Tailscale owns a tagged node by its
/// tags, and `whois` names every tagged node of a tailnet as one shared
/// user, `tagged-devices` — so on a tagged host the check would admit any
/// tagged machine and turn away the person's own. It is refused there.
pub fn same_user(status: &tailscale::Status) -> Result<u64, String> {
    if let Some(tags) = status.me.tags.as_ref().filter(|t| !t.is_empty()) {
        return Err(format!("Tailscale owns this machine by its tags ({}), not by a person, so the same-user check cannot hold — remove them, or leave KROWK_TAILSCALE_SAME_USER unset", tags.join(", ")));
    }
    if status.me.user_id == 0 {
        return Err("Tailscale names no user for this machine, so the same-user check cannot pass".into());
    }
    Ok(status.me.user_id)
}

/// Reads this node from tailscaled and listens on its tailnet address (and
/// LAN address), on one port; with the LAN offered, Tailscale not running
/// leaves the LAN alone, advertised over mDNS too. Errors say why there is
/// no direct path; the session then goes by the relay alone.
pub fn listen(c: &Config, session: [u8; 16], host: krowk_client::e2e::DeviceId) -> Result<Listening, String> {
    // Never the LAN with the same-user check: tailscaled knows no one on
    // the LAN, so every LAN connection would be turned away.
    let lan = c.lan && !c.same_user;
    let (status, down) = match running(&c.socket) {
        Ok(s) => (Some(s), None),
        Err(why) if lan => (None, Some(why)),
        Err(why) => return Err(why),
    };
    let url = |ip: IpAddr, port: u16| match ip {
        IpAddr::V6(v6) => format!("ws://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("ws://{v4}:{port}"),
    };
    let (mut listeners, mut candidates, mut port, mut whois, mut tailnet) = (Vec::new(), Vec::new(), 0, None, None);
    if let Some(status) = &status {
        // Only a tailnet address is listened on, whatever the LocalAPI
        // says: on macOS it is a port on 127.0.0.1 that anyone may bind
        // once the app quits, and an answer of 0.0.0.0 would put plain
        // ws:// on every network, past KROWK_DIRECT_LAN.
        let ips: Vec<IpAddr> = status.me.tailscale_ips.iter().copied().filter(tailscale::is_tailnet).collect();
        let ip = ips.iter().find(|i| i.is_ipv4()).or(ips.first()).copied().ok_or("Tailscale gives this machine no tailnet address")?;
        whois = if c.same_user { Some(SameUser { socket: c.socket.clone(), user: same_user(status)? }) } else { None };
        let first = TcpListener::bind((ip, 0)).map_err(|e| format!("the tailnet address {ip} could not be listened on: {e}"))?;
        port = first.local_addr().map_err(|e| e.to_string())?.port();
        listeners.push(first);
        candidates.push(Candidate { url: url(ip, port), via: "tailscale".into() });
        if let Some(name) = status.magic_dns() {
            candidates.push(Candidate { url: format!("ws://{name}:{port}"), via: "magicdns".into() });
        }
        tailnet = Some(ip);
    }
    // Without Tailscale the bridge dials its own listener on loopback, so
    // its uplink outlives a LAN address it loses; never offered to a viewer.
    let mut origins = Vec::new();
    if lan && status.is_none() {
        let own = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("the direct listener could not be opened on loopback: {e}"))?;
        origins.push(url(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), own.local_addr().map_err(|e| e.to_string())?.port()));
        listeners.push(own);
    }
    let read_lan: LanAt = match (&c.lan_at, &status) {
        (Some(at), _) => at.clone(),
        (None, Some(_)) => {
            let socket = c.socket.clone();
            std::sync::Arc::new(move || tailscale::status(&socket).map(|s| s.lan()).unwrap_or_default())
        }
        (None, None) => std::sync::Arc::new(super::mdns::lan_addresses),
    };
    let mut bound = Vec::new();
    if lan {
        for ip in read_lan().into_iter().filter(|l| Some(*l) != tailnet) {
            // The same port, so a viewer's candidates name one listener; an
            // address taken there is simply not offered.
            let Ok(l) = TcpListener::bind((ip, port)) else { continue };
            if port == 0 {
                port = l.local_addr().map_err(|e| e.to_string())?.port();
            }
            listeners.push(l);
            candidates.push(Candidate { url: url(ip, port), via: "lan".into() });
            bound.push(ip);
        }
        if candidates.is_empty() {
            return Err(format!("{}, and no LAN address of this machine could be listened on", down.unwrap_or_default()));
        }
    }
    let dial = origins.first().unwrap_or(&candidates[0].url).clone();
    origins.extend(candidates.iter().map(|c| c.url.clone()));
    let config = relay::Config { roster: c.roster.clone(), origin: None, limits: Limits { first_link: FIRST_LINK, ..Limits::default() }, state: None, origins, whois, pin: Some((session, host)) };
    let (stop, rx) = match c.stop.clone() {
        Some(rx) => (None, rx),
        None => {
            let (tx, rx) = watch::channel(false);
            (Some(tx), rx)
        }
    };
    let following = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let now = std::sync::Arc::new(std::sync::Mutex::new(candidates.clone()));
    let changes = if lan {
        let (tx, changes) = tokio::sync::mpsc::unbounded_channel();
        let (following, stopped, now) = (following.clone(), rx.clone(), now.clone());
        std::thread::spawn(move || follow_lan(session, port, tailnet, bound, read_lan, tx, following, stopped, now));
        Some(changes)
    } else {
        None
    };
    std::thread::spawn(move || {
        if let Err(e) = relay::run_changing(listeners, config, rx, changes) {
            eprintln!("krowk: the direct listener stopped: {e}");
        }
    });
    Ok(Listening { dial, candidates, stop, following, now })
}

/// The LAN addresses as the machine moves (`LAN_CHECK`): each new one
/// listened on, at the listener's port, and each lost one let go, and the
/// mDNS advertisement kept to what is listened on. mDNS lets a viewer on
/// the LAN find the listener by its session alone, beside the welcome's
/// addresses, which name only those the session started with. Ends with
/// the listener.
#[allow(clippy::too_many_arguments)]
fn follow_lan(session: [u8; 16], port: u16, tailnet: Option<IpAddr>, mut bound: Vec<IpAddr>, read_lan: LanAt, changes: tokio::sync::mpsc::UnboundedSender<relay::Change>, following: std::sync::Arc<std::sync::atomic::AtomicBool>, stopped: watch::Receiver<bool>, offered: std::sync::Arc<std::sync::Mutex<Vec<Candidate>>>) {
    let url = |ip: &IpAddr| match ip {
        IpAddr::V6(v6) => format!("ws://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("ws://{v4}:{port}"),
    };
    let advertise = |bound: &[IpAddr]| {
        let v4: Vec<std::net::Ipv4Addr> = bound.iter().filter_map(|ip| if let IpAddr::V4(v4) = ip { Some(*v4) } else { None }).collect();
        if v4.is_empty() {
            return None;
        }
        super::mdns::advertise(session, &v4, port).map_err(|e| eprintln!("krowk: {e}; the LAN path is offered without it")).ok()
    };
    let mut advertised = advertise(&bound);
    let on = || following.load(std::sync::atomic::Ordering::SeqCst) && !*stopped.borrow() && !changes.is_closed();
    while on() {
        let until = Instant::now() + LAN_CHECK;
        while on() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !on() {
            break;
        }
        let now: Vec<IpAddr> = read_lan().into_iter().filter(|l| Some(*l) != tailnet).collect();
        // None read — tailscaled not answering this once, or no network at
        // all — lets nothing go: a listener on an address that is gone
        // costs nothing, and one let go too soon is a path lost.
        if now.is_empty() {
            continue;
        }
        let mut moved = false;
        for ip in bound.clone().into_iter().filter(|ip| !now.contains(ip)) {
            let _ = changes.send(relay::Change::Remove(url(&ip)));
            bound.retain(|b| *b != ip);
            moved = true;
        }
        let new: Vec<IpAddr> = now.into_iter().filter(|ip| !bound.contains(ip)).collect();
        for ip in new {
            // Made ready to accept here, so the relay takes it as it is:
            // an address is advertised only once something listens there.
            let Ok(l) = TcpListener::bind((ip, port)).and_then(|l| l.set_nonblocking(true).map(|()| l)) else { continue };
            if changes.send(relay::Change::Add(l, url(&ip))).is_err() {
                return;
            }
            bound.push(ip);
            moved = true;
        }
        if moved {
            let mut o = offered.lock().unwrap_or_else(|e| e.into_inner());
            o.retain(|c| c.via != "lan");
            o.extend(bound.iter().map(|ip| Candidate { url: url(ip), via: "lan".into() }));
            drop(o);
            // The old responder says goodbye as it goes; the new one
            // announces what is listened on now.
            drop(advertised.take());
            advertised = advertise(&bound);
        }
    }
}

/// How long a viewer the host offered direct addresses may stay on the
/// relay before the host says none of them reached it. The viewer races
/// them once welcomed, after reading the session's chunks and fetching a
/// ticket, each address given `viewer::PROBE`: past this its first race is
/// long over, a large session and a slow device included.
pub const UNREACHED_AFTER: Duration = Duration::from_secs(30);

/// The viewers the host offered direct addresses that have not reached
/// them. A firewall on the host, or a tailnet access policy that does not
/// allow the port, drops a viewer's connection before the listener sees
/// it, so the host has nothing to refuse and nothing to log; what it can
/// see is a viewer's device the relay announced (`relay.md` → Presence)
/// that never joins the direct listener. Kept by relay link, said by
/// device, once per device and never for one that has reached the direct
/// path in this run — a viewer that falls back to the relay retries on its
/// own clock — and only while the host's own link to its listener is up,
/// since viewers that reach the listener while it is down stay away too.
#[derive(Debug)]
pub struct Unreached {
    since: HashMap<u64, (DeviceId, Instant)>,
    seen: HashSet<String>,
    told: HashSet<String>,
    down: bool,
}

impl Default for Unreached {
    fn default() -> Self {
        Unreached { since: HashMap::new(), seen: HashSet::new(), told: HashSet::new(), down: true }
    }
}

impl Unreached {
    /// Viewer link `link`, of `device`, joined on the relay, offered the
    /// direct addresses.
    pub fn relay_joined(&mut self, link: u64, device: DeviceId, now: Instant) {
        let id = device.to_string();
        if !self.seen.contains(&id) && !self.told.contains(&id) {
            self.since.entry(link).or_insert((device, now));
        }
    }

    /// Viewer link `link` left the relay.
    pub fn relay_left(&mut self, link: u64) {
        self.since.remove(&link);
    }

    /// The host's link to the relay was lost: who is there is learned
    /// again from the relay's replay as it joins once more.
    pub fn relay_lost(&mut self) {
        self.since.clear();
    }

    /// `device` joined the direct listener: it reaches this machine.
    pub fn direct_joined(&mut self, device: DeviceId) {
        self.seen.insert(device.to_string());
        self.since.retain(|_, (d, _)| d.0 != device.0);
    }

    /// The host's link to its own listener was lost.
    pub fn direct_down(&mut self) {
        self.down = true;
    }

    /// The host's link to its own listener is back. A viewer that raced
    /// while it was down found no host there and backed off, up to
    /// `viewer::REPROBE_MAX`, so each waiting viewer's wait starts again
    /// past that.
    pub fn direct_up(&mut self, now: Instant) {
        if self.down {
            for (_, t) in self.since.values_mut() {
                *t = (*t).max(now + super::viewer::REPROBE_MAX);
            }
        }
        self.down = false;
    }

    /// The devices that have stayed on the relay past `UNREACHED_AFTER`,
    /// each answered once; none while the listener's link is down.
    pub fn due(&mut self, now: Instant) -> Vec<DeviceId> {
        if self.down {
            return Vec::new();
        }
        let mut due: Vec<DeviceId> = self.since.values().filter(|(_, t)| now.saturating_duration_since(*t) >= UNREACHED_AFTER).map(|(d, _)| *d).collect();
        due.sort_by_key(|d| d.to_string());
        due.dedup_by(|a, b| a.0 == b.0);
        for d in &due {
            self.told.insert(d.to_string());
            self.since.retain(|_, (x, _)| x.0 != d.0);
        }
        due
    }
}

/// What the host says of a viewer `device` that never reached
/// `candidates`; `same_user` when that check could have turned it away.
pub fn unreached_line(session: &str, device: &DeviceId, candidates: &[Candidate], same_user: bool) -> String {
    let at = candidates.iter().map(|c| c.url.trim_start_matches("ws://")).collect::<Vec<_>>().join(", ");
    let short: String = device.to_string().chars().take(8).collect();
    let why = if candidates.iter().any(|c| !c.on_lan()) { "if it is on this tailnet, a firewall here or the tailnet's access policy may be refusing the port" } else { "a firewall here may be refusing the port" };
    let check = if same_user { ", or the same-user check (KROWK_TAILSCALE_SAME_USER) turned it away" } else { "" };
    let mac = if cfg!(target_os = "macos") { " — on macOS, allow incoming connections for krowk under System Settings → Network → Firewall → Options" } else { "" };
    format!("krowk: session {session}: device {short} stays on the relay — nothing it sent reached this machine's direct addresses ({at}); {why}{check}{mac}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(n: u8) -> DeviceId {
        DeviceId([n; 16])
    }

    fn listening(now: Instant) -> Unreached {
        let mut u = Unreached::default();
        u.direct_up(now);
        u
    }

    /// R-NET-2: a viewer that stays on the relay past its race is named
    /// once; one that reaches the direct path, or leaves, is not; two
    /// viewers of one device are one name.
    #[test]
    fn r_net_2_a_viewer_that_never_reaches_the_direct_path_is_named_once() {
        let t0 = Instant::now();
        let mut u = listening(t0);
        u.relay_joined(1, device(1), t0);
        u.relay_joined(2, device(2), t0);
        u.relay_joined(3, device(3), t0);
        u.relay_joined(4, device(1), t0 + Duration::from_secs(5));
        u.direct_joined(device(2));
        u.relay_left(3);
        assert!(u.due(t0 + UNREACHED_AFTER - Duration::from_millis(1)).is_empty(), "not before the race is over");
        assert_eq!(u.due(t0 + UNREACHED_AFTER), [device(1)], "from its first link, once");
        u.relay_joined(5, device(1), t0 + UNREACHED_AFTER);
        assert!(u.due(t0 + UNREACHED_AFTER * 3).is_empty(), "each device once");
    }

    /// R-NET-2: a viewer that reached the direct path and fell back to the
    /// relay is never named: it retries on its own clock, and the path
    /// worked.
    #[test]
    fn r_net_2_a_viewer_that_fell_back_from_the_direct_path_is_not_named() {
        let t0 = Instant::now();
        let mut u = listening(t0);
        u.relay_joined(1, device(1), t0);
        u.direct_joined(device(1));
        u.relay_left(1);
        u.relay_joined(2, device(1), t0 + Duration::from_secs(60));
        assert!(u.due(t0 + Duration::from_secs(600)).is_empty());
    }

    /// R-NET-2: nothing is said while the host's own link to its listener
    /// is down — viewers that reach it then stay on the relay too — and
    /// once it is back the wait starts again past the viewer's longest
    /// backoff; a relay link lost forgets who was there, for the relay's
    /// replay to say again.
    #[test]
    fn r_net_2_nothing_is_said_while_the_listener_is_down_or_after_the_relay_is_lost() {
        let t0 = Instant::now();
        let mut u = Unreached::default();
        u.relay_joined(1, device(1), t0);
        assert!(u.due(t0 + UNREACHED_AFTER).is_empty(), "down from the start");
        let up = t0 + UNREACHED_AFTER;
        u.direct_up(up);
        let named = up + crate::sync::viewer::REPROBE_MAX + UNREACHED_AFTER;
        assert!(u.due(named - Duration::from_millis(1)).is_empty(), "past the viewer's backoff first");
        assert_eq!(u.due(named), [device(1)], "then named");

        let mut u = listening(t0);
        u.relay_joined(1, device(2), t0);
        u.relay_lost();
        assert!(u.due(t0 + UNREACHED_AFTER * 5).is_empty(), "a viewer that left while the relay was away is not named");
    }

    /// The line names the device by its id's first eight characters, every
    /// address, and the same-user check when it is on.
    #[test]
    fn r_net_2_the_unreached_line_names_the_addresses_and_the_checks() {
        let c = [Candidate { url: "ws://100.64.0.1:51915".into(), via: "tailscale".into() }, Candidate { url: "ws://a.tail1.ts.net:51915".into(), via: "magicdns".into() }];
        let line = unreached_line("s1", &device(0xab), &c, false);
        assert!(line.contains("device abababab ") && line.contains("100.64.0.1:51915, a.tail1.ts.net:51915") && line.contains("tailnet's access policy"), "{line}");
        assert!(!line.contains("same-user"));
        assert!(unreached_line("s1", &device(1), &c, true).contains("KROWK_TAILSCALE_SAME_USER"));
        let lan = [Candidate { url: "ws://192.168.1.2:51915".into(), via: "lan".into() }];
        assert!(!unreached_line("s1", &device(1), &lan, false).contains("tailnet"));
    }

    /// R-NET-3: the same-user check holds a far end to this node's person,
    /// and is refused on a tagged node, which `whois` names as the
    /// tailnet's shared `tagged-devices` user, never a person.
    #[test]
    fn r_net_3_the_same_user_check_is_refused_on_a_tagged_host() {
        let status = |me: serde_json::Value| -> tailscale::Status { serde_json::from_value(serde_json::json!({"BackendState": "Running", "Self": me})).unwrap() };
        assert_eq!(same_user(&status(serde_json::json!({"UserID": 7}))), Ok(7));
        assert_eq!(same_user(&status(serde_json::json!({"UserID": 7, "Tags": []}))), Ok(7));
        let tagged = same_user(&status(serde_json::json!({"UserID": 478457612062579_u64, "Tags": ["tag:ops"]}))).unwrap_err();
        assert!(tagged.contains("tag:ops") && tagged.contains("KROWK_TAILSCALE_SAME_USER"), "{tagged}");
        assert!(same_user(&status(serde_json::json!({}))).is_err());
    }
}
