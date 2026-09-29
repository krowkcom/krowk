//! `krowk relay serve`: the reference relay (R-RELAY-1) — Canon's
//! engineering/relay.md in Rust, `krowk_harness::relay`. The hermetic
//! stand-in the tests and ticket 18's conformance runs use, and a way to
//! run a relay of one's own. Loopback unless `--addr` names another
//! address, and then it says so, as the stand-in registry does: anyone who
//! reaches it can open connections, though only the keyring's devices can
//! join, and it only ever holds ciphertext.

use super::Ctx;
use krowk_api::{fail, Error};
use krowk_harness::relay::{self, Config, Keyring, Limits};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};

pub const DEFAULT_ADDR: &str = "127.0.0.1:7790";

pub(super) fn serve(ctx: &mut Ctx) -> Result<(), Error> {
    if ctx.f.keyring.is_empty() {
        return Err(fail("bad_flags", "krowk relay serve needs --keyring FILE: the devices it trusts and the sessions' leases, as engineering/relay.md lays out"));
    }
    let text = std::fs::read_to_string(&ctx.f.keyring).map_err(|e| fail("bad_config", format!("{} cannot be read: {e}", ctx.f.keyring)))?;
    let keyring = Keyring::parse(&text).map_err(|e| fail("bad_config", format!("{}: {e}", ctx.f.keyring)))?;
    let asked = if ctx.f.addr.is_empty() { DEFAULT_ADDR.to_string() } else { ctx.f.addr.clone() };
    let addr: SocketAddr = asked.to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| fail("bad_flags", format!("--addr {asked:?} needs a host and a numeric port, like {DEFAULT_ADDR}")))?;
    let origin = Some(ctx.f.origin.clone()).filter(|o| !o.is_empty());
    if let Some(o) = &origin
        && !(o.starts_with("ws://") || o.starts_with("wss://"))
    {
        return Err(fail("bad_flags", format!("--origin {o:?} is ws://host:port or wss://host, what devices dial")));
    }
    let listener = TcpListener::bind(addr).map_err(|e| fail("relay_unavailable", format!("{addr} cannot be listened on: {e}")))?;
    let bound = listener.local_addr().map_err(|e| fail("relay_unavailable", e.to_string()))?;
    // Bound before it is announced, so a script keying off the banner
    // finds it listening.
    let _ = ctx.io.stdout.write_all(banner(&bound, origin.as_deref()).as_bytes()).and_then(|_| ctx.io.stdout.flush());
    relay::run(listener, Config { keyring, origin, limits: Limits::default() }).map_err(|e| fail("relay_unavailable", e))
}

/// Where the relay is, where a session's channel is, and — bound wider
/// than this machine — that it is reachable from the network.
pub fn banner(bound: &SocketAddr, origin: Option<&str>) -> String {
    let base = origin.map(str::to_string).unwrap_or_else(|| format!("ws://{bound}"));
    let mut lines = vec![format!("krowk relay listening on {base}"), format!("  a session's channel: {base}{}<session id>", relay::PATH)];
    if !bound.ip().to_canonical().is_loopback() {
        let what = if bound.ip().is_unspecified() { "every interface".to_string() } else { bound.ip().to_string() };
        lines.push(format!("  ! reachable from the network on {what} — anyone can connect; only the keyring's devices can join, and it carries ciphertext only"));
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
}
