//! `krowk relay serve`: the reference relay (R-RELAY-1) — Canon's
//! engineering/relay.md in Rust, `krowk_harness::relay`. The hermetic
//! stand-in the tests and ticket 18's conformance runs use, and a way to
//! run a relay of one's own. Loopback unless `--addr` names another
//! address, and then it says so, as the stand-in registry does: anyone who
//! reaches it can open connections, though only a device with a registry ticket can
//! join, and it only ever holds ciphertext.

use super::Ctx;
use krowk_api::{fail, Error};
use krowk_harness::relay::{self, Config, Roster, Limits};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};

pub const DEFAULT_ADDR: &str = "127.0.0.1:7790";

pub(super) fn serve(ctx: &mut Ctx) -> Result<(), Error> {
    if ctx.f.roster.is_empty() {
        return Err(fail("bad_flags", "krowk relay serve needs --ticket-keys FILE: the registry's ticket-signing public keys, {\"ticketKeys\": {\"<kid>\": \"<key>\"}}, as engineering/relay.md lays out"));
    }
    let text = std::fs::read_to_string(&ctx.f.roster).map_err(|e| fail("bad_config", format!("{} cannot be read: {e}", ctx.f.roster)))?;
    let roster = Roster::parse(&text).map_err(|e| fail("bad_config", format!("{}: {e}", ctx.f.roster)))?;
    let asked = if ctx.f.addr.is_empty() { DEFAULT_ADDR.to_string() } else { ctx.f.addr.clone() };
    let addr: SocketAddr = asked.to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| fail("bad_flags", format!("--addr {asked:?} needs a host and a numeric port, like {DEFAULT_ADDR}")))?;
    let origin = match Some(ctx.f.origin.clone()).filter(|o| !o.is_empty()) {
        None => None,
        Some(o) => Some(krowk_client_origin(&o).ok_or_else(|| fail("bad_flags", format!("--origin {o:?} is ws://host[:port] or wss://host[:port], what devices dial")))?),
    };
    // Off loopback, the relay must be told what devices dial: taking it
    // from the Host a client sends would let a relay in the middle pass
    // this one's challenge through under its own name (relay.md → Joining).
    if origin.is_none() && !addr.ip().to_canonical().is_loopback() {
        return Err(fail("bad_flags", format!("--addr {asked} is reachable from the network, so the relay needs --origin: the URL devices dial it by, like wss://relay.example.com")));
    }
    // And its channels' fences must outlive a restart there, or a restart
    // lets a displaced lease holder host again (relay.md → Tickets).
    let state = Some(ctx.f.relay_state.clone()).filter(|d| !d.is_empty()).map(std::path::PathBuf::from);
    if state.is_none() && !addr.ip().to_canonical().is_loopback() {
        return Err(fail("bad_flags", format!("--addr {asked} is reachable from the network, so the relay needs --state DIR: where it keeps each channel's fence across restarts")));
    }
    let config = Config { roster, origin: origin.clone(), limits: Limits::default(), state, origins: Vec::new(), whois: None, pin: None };
    // The state first: a relay that will not start says so before it says
    // it is listening.
    let opened = relay::open(&config).map_err(|e| fail("bad_state", e))?;
    let listener = TcpListener::bind(addr).map_err(|e| fail("relay_unavailable", format!("{addr} cannot be listened on: {e}")))?;
    let bound = listener.local_addr().map_err(|e| fail("relay_unavailable", e.to_string()))?;
    // Bound before it is announced, so a script keying off the banner
    // finds it listening.
    let _ = ctx.io.stdout.write_all(banner(&bound, origin.as_deref()).as_bytes()).and_then(|_| ctx.io.stdout.flush());
    relay::run_opened(listener, config, opened).map_err(|e| fail("relay_unavailable", e))
}

/// `--origin` in the form devices sign it.
fn krowk_client_origin(o: &str) -> Option<String> {
    let lower = o.to_ascii_lowercase();
    krowk_harness::relay::canonical_origin(o).filter(|_| lower.starts_with("ws://") || lower.starts_with("wss://"))
}

/// Where the relay is, where a session's channel is, and — bound wider
/// than this machine — that it is reachable from the network.
pub fn banner(bound: &SocketAddr, origin: Option<&str>) -> String {
    let base = origin.map(str::to_string).unwrap_or_else(|| format!("ws://{bound}"));
    let mut lines = vec![format!("krowk relay listening on {base}"), format!("  a session's channel: {base}{}<session id>", relay::PATH)];
    if !bound.ip().to_canonical().is_loopback() {
        let what = if bound.ip().is_unspecified() { "every interface".to_string() } else { bound.ip().to_string() };
        lines.push(format!("  ! reachable from the network on {what} — anyone can connect; only a device with a ticket its keys verify can join, and it carries ciphertext only"));
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-RELAY-1: loopback says nothing more; any other address says it is
    /// open to the network, as the stand-in registry's banner does.
    #[test]
    fn r_relay_1_the_banner_warns_only_when_the_relay_is_reachable_from_the_network() {
        let local = banner(&"127.0.0.1:7790".parse().unwrap(), None);
        assert_eq!(local, "krowk relay listening on ws://127.0.0.1:7790\n  a session's channel: ws://127.0.0.1:7790/v1/relay/<session id>\n");
        assert!(banner(&"0.0.0.0:7790".parse().unwrap(), None).contains("! reachable from the network on every interface"));
        assert!(banner(&"192.168.1.4:7790".parse().unwrap(), Some("wss://relay.example")).contains("on 192.168.1.4"));
        assert!(!banner(&"[::1]:7790".parse().unwrap(), None).contains('!'));
    }

    /// R-RELAY-1: `--origin` is taken as a device signs it, and only as a
    /// WebSocket URL.
    #[test]
    fn r_relay_1_origin_is_canonical_and_a_websocket_url() {
        assert_eq!(krowk_client_origin("WSS://Relay.Example:443/v1").as_deref(), Some("wss://relay.example"));
        assert_eq!(krowk_client_origin("ws://10.0.0.2:7790").as_deref(), Some("ws://10.0.0.2:7790"));
        assert_eq!(krowk_client_origin("https://relay.example"), None);
        assert_eq!(krowk_client_origin("relay.example"), None);
    }
}
