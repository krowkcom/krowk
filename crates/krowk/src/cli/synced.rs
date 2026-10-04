//! `krowk sync sessions`, `krowk sync host` and `krowk sync attach`: a
//! session running on one machine, watched and steered from another
//! (engineering/harness.md → Sync). The bridge and the viewer are
//! `krowk_harness::sync`; this is the command line around them.
//!
//! - `sessions`: the synced sessions this machine can open, from their
//!   sealed indexes.
//! - `host <session>`: this machine's daemon runs the session, and the
//!   bridge holds its lease and syncs it until interrupted. The session is
//!   one this machine already has; any other id is refused up front.
//! - `attach <session>`: follows it from another machine. On a terminal it
//!   opens the TUI on it (D11); otherwise, or with `--json`, as stream-json
//!   on stdout, each line typed on stdin a prompt to it, queued while no
//!   host is online, or a command: `/approve`, `/allow-session` and `/deny`
//!   a request, `/interrupt` or `/steer` the turn. Stdin's end ends it once
//!   every command sent is answered.
//!
//! The relay is `KROWK_RELAY_URL`, else the reference relay on this
//! machine (`krowk relay serve`), until the hosted relay has a published
//! address.

use super::sync::{keyed_client, keystore};
use super::Ctx;
use krowk_api::{fail, Client, Error};
use krowk_client::e2e::{DeviceId, SigningKey};
use krowk_client::device_chain::Chain;
use krowk_client::user_key::UserKeys;
use krowk_harness::sync::{host, viewer};
use serde_json::json;
use std::sync::Arc;

struct Keys {
    device: DeviceId,
    signing: SigningKey,
    user: UserKeys,
    /// The device list as this machine last verified it.
    chain: Chain,
}

fn keys(ctx: &Ctx) -> Result<Keys, Error> {
    let ks = keystore(ctx)?;
    let device = ks.device().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(|| fail("not_set_up", "this machine has no sync keys — run `krowk sync init`, `recover` or `join` first"))?.id();
    // Every session key is sealed under the person's user key, so a
    // machine without it has nothing to open or seal with.
    let user = ks.user_keys().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(|| fail("not_set_up", "this machine holds no user key yet — add it to your devices from one that does, or recover with your kit"))?;
    // Whose signatures a session record is checked against, and the
    // generation a new session is sealed under: the list as this machine
    // keeps it, verified again from entry 0 — brought up to date first by
    // `current` wherever anything is sealed. Without it nothing opens or
    // seals.
    let chain = ks.device_list().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(|| fail("not_set_up", "this machine has not verified your device list yet — add it to your devices from one that has, or recover with your kit"))?;
    let user = user.verified_by(&chain).map_err(|e| fail("keys_unreadable", e.0))?;
    let signing = ks.signing_key().map_err(|e| fail("keys_unreadable", e))?;
    Ok(Keys { device, signing, user, chain })
}

/// Before a session is hosted or attached to — and so before anything new
/// is sealed — the list kept here extended from the registry and verified,
/// and a newer user key taken up (`chain::before_sealing`), so the keys
/// `keys` reads next are the ones the list leaves current. A machine with
/// no list kept, or cut off from the registry, seals nothing: never under
/// a generation a removed device may hold.
fn current(ctx: &Ctx) -> Result<(), Error> {
    let me = super::chain::Me::load(ctx)?;
    let client = keyed_client(ctx, "sync")?;
    super::chain::before_sealing(ctx, &client, &me).map(|_| ())
}

/// The registry client for this machine's sync calls, signed by its own
/// key: leases, chunks, the index, relay tickets and every session read act
/// as this device.
fn signed(ctx: &Ctx, k: &Keys, what: &str) -> Result<Client, Error> {
    let key = SigningKey::from_secret(&*k.signing.secret_bytes()).map_err(|e| fail("keys_unreadable", e.to_string()))?;
    Ok(keyed_client(ctx, what)?.signed_by(krowk_client::e2e::DeviceSigner::new(k.device, key).shared()))
}

/// The relay hosted beside the production registry.
const HOSTED_RELAY: &str = "wss://relay.krowk.com";

/// The relay a sync link dials: `KROWK_RELAY_URL` when set, else the hosted
/// relay for the production registry, else the local stand-in (`krowk relay
/// serve`) that a stand-in or custom registry is run beside.
fn relay(ctx: &Ctx, base_url: &str) -> String {
    relay_for(&ctx.env("KROWK_RELAY_URL"), base_url)
}

fn relay_for(asked: &str, base_url: &str) -> String {
    if !asked.trim().is_empty() {
        return asked.trim().to_string();
    }
    if base_url.trim_end_matches('/') == krowk_api::DEFAULT_BASE_URL { HOSTED_RELAY.to_string() } else { format!("ws://{}", super::relay::DEFAULT_ADDR) }
}

/// Direct paths beside the relay (R-NET-1): on when this machine can check
/// a viewer's ticket as the relay does, with the registry's ticket-signing
/// keys in `KROWK_RELAY_TICKET_KEYS` (the file `krowk relay serve
/// --ticket-keys` reads). Without them there is no direct listener: nothing
/// but a ticket may let a device in. `KROWK_TAILSCALE_SAME_USER=1` also
/// requires tailscaled to name the far end as this tailnet user (R-NET-3).
/// The LAN address is offered only with `KROWK_DIRECT_LAN=1`: it is off the
/// tailnet, so plain `ws://` there is reachable by the whole network.
fn direct(ctx: &Ctx) -> Result<Option<krowk_harness::sync::direct::Config>, Error> {
    let keys = ctx.env("KROWK_RELAY_TICKET_KEYS");
    if keys.trim().is_empty() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(keys.trim()).map_err(|e| fail("bad_ticket_keys", format!("KROWK_RELAY_TICKET_KEYS names {}, which could not be read: {e}", keys.trim())))?;
    let roster = krowk_harness::relay::Roster::parse(&text).map_err(|e| fail("bad_ticket_keys", e))?;
    Ok(Some(krowk_harness::sync::direct::Config { socket: krowk_harness::sync::tailscale::socket(ctx.io.env), roster, same_user: ctx.env("KROWK_TAILSCALE_SAME_USER") == "1", lan: ctx.env("KROWK_DIRECT_LAN") == "1", stop: None }))
}

/// `krowk hosts`: the tailnet's machines tagged `tag:krowk-host`, from the
/// local tailscaled, with no pairing step (R-NET-4).
pub(super) fn hosts(ctx: &mut Ctx) -> Result<(), Error> {
    use krowk_harness::sync::tailscale;
    let s = tailscale::status(&tailscale::socket(ctx.io.env)).map_err(|e| fail("tailscale_unavailable", format!("{e} — start Tailscale, or name its socket in KROWK_TAILSCALE_SOCKET")))?;
    let hosts = s.hosts();
    if ctx.format == crate::output::Format::Json {
        let rows: Vec<_> = hosts.iter().map(|h| json!({"name": h.host_name, "dnsName": h.dns_name.trim_end_matches('.'), "addresses": h.tailscale_ips, "online": h.online})).collect();
        return ctx.emit(&json!({"hosts": rows}).to_string());
    }
    for h in &hosts {
        let ip = h.tailscale_ips.first().map(|i| i.to_string()).unwrap_or_default();
        let _ = writeln!(ctx.io.stdout, "{}  {}  {}  {}", super::sync::printable(&h.host_name), super::sync::printable(h.dns_name.trim_end_matches('.')), ip, if h.online { "online" } else { "offline" });
    }
    if hosts.is_empty() {
        let _ = writeln!(ctx.io.stdout, "no machine on this tailnet is tagged {}", tailscale::HOST_TAG);
    }
    Ok(())
}

fn one(args: &[String], what: &str) -> Result<String, Error> {
    match args {
        [s] => Ok(s.clone()),
        _ => Err(fail("bad_args", format!("`krowk sync {what}` takes one session id"))),
    }
}

pub(super) fn sessions(ctx: &mut Ctx) -> Result<(), Error> {
    let k = keys(ctx)?;
    let api = signed(ctx, &k, "krowk sync sessions")?;
    let (listed, unreadable) = viewer::list(&api, &k.user, &k.chain).map_err(|e| fail("sync_failed", e))?;
    let rows: Vec<_> = listed.iter().map(|s| json!({"id": s.id, "title": s.index.title, "cwd": s.index.cwd, "updatedMs": s.index.updated_ms, "host": s.holder})).collect();
    if ctx.format == crate::output::Format::Json {
        return ctx.emit(&json!({"sessions": rows, "unreadable": unreadable}).to_string());
    }
    for s in &listed {
        let title = super::sync::printable(&s.index.title);
        let _ = writeln!(ctx.io.stdout, "{}  {}  {}", s.id, if s.holder.is_some() { "hosted" } else { "offline" }, title);
    }
    if unreadable > 0 {
        let _ = writeln!(ctx.io.stdout, "{unreadable} synced session(s) do not open with the user keys this machine holds");
    }
    Ok(())
}

pub(super) fn host_session(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let session = one(args, "host")?;
    // The daemon runs only sessions it has a log of. Asked for any other,
    // the bridge would take the session's lease in the registry and then
    // have nothing to follow: said here, before anything is written there.
    if !krowk_harness::log::valid_id(&session) {
        return Err(fail("bad_session", format!("{session:?} is not a session id — `krowk sessions` lists this machine's")));
    }
    let session = local_log(ctx, &session)?;
    // The list first: a newer key it takes up is the one `keys` reads.
    current(ctx)?;
    let k = keys(ctx)?;
    let api = Arc::new(signed(ctx, &k, "krowk sync host")?);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let cwd = std::env::current_dir().map_err(|e| fail("no_cwd", e.to_string()))?;
    let spawn = super::host::spawner(ctx)?;
    let o = host::Options {
        relay: relay(ctx, &api.base_url),
        env,
        api,
        device: k.device,
        signing: k.signing,
        keys: k.user,
        chain: k.chain,
        session,
        title: String::new(),
        cwd: cwd.display().to_string(),
        ttl: host::LEASE_TTL,
        keep: host::KEEP,
        direct: direct(ctx)?,
    };
    krowk_harness::sync::run_host(o, ctx.io.env, &cwd, super::VERSION, &spawn).map_err(|(code, message)| fail(&code, message))
}

/// The id of the log this machine keeps for `session`: the id itself, or —
/// for the id `krowk sessions` listed a session under before its row took
/// its log's id — the log id krowk.db binds it to.
fn local_log(ctx: &Ctx, session: &str) -> Result<String, Error> {
    let dir = krowk_harness::log::sessions_dir(ctx.io.env)?;
    let has_log = |id: &str| dir.join(id).join(krowk_harness::log::EVENTS_FILE).is_file();
    if has_log(session) {
        return Ok(session.to_string());
    }
    log_id_for(ctx, session).filter(|id| has_log(id)).ok_or_else(|| {
        fail("no_session", format!("this machine has no session {session} to host — start one here (`krowk`, or `krowk -p \"...\"`), then `krowk sync host <its id>`; `krowk sessions` lists this machine's"))
    })
}

/// The log id of the native session krowk.db stores under `id`: a session
/// stored before its row took its log's id is listed under the other one.
/// None for anything else, a store that cannot be read included — never a
/// new krowk.db made just to look.
fn log_id_for(ctx: &Ctx, id: &str) -> Option<String> {
    if !krowk_store::db_path(ctx.io.env).ok()?.is_file() {
        return None;
    }
    let conn = super::sessions::open_store(ctx).ok()?;
    krowk_store::foreign_session_id(&conn, id, krowk_harness::project::HARNESS).ok().flatten()
}

pub(super) fn attach(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let session = one(args, "attach")?;
    current(ctx)?;
    let k = keys(ctx)?;
    let api = Arc::new(signed(ctx, &k, "krowk sync attach")?);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let o = viewer::Options { relay: relay(ctx, &api.base_url), env, api, device: k.device, signing: k.signing, keys: k.user, chain: k.chain, session: session.clone(), known: None };
    if on_a_terminal(ctx) {
        return super::tui::run_synced(ctx, o);
    }
    krowk_harness::sync::run_attach(o, &mut *ctx.io.stdout).map_err(|e| fail("sync_failed", e))
}

/// Whether the session is drawn in the TUI: a person at both ends of a
/// terminal that can draw it, and no `--json`. Anything else — a pipe, a
/// script — gets stream-json, as `krowk sync attach` always printed.
fn on_a_terminal(ctx: &Ctx) -> bool {
    ctx.format == crate::output::Format::Human && ctx.io.tty && ctx.io.stdin_tty && ctx.env("TERM") != "dumb"
}

/// `krowk --resume <id>` for a session this machine does not have but can
/// open from sync: it follows it as `krowk sync attach` does, so the
/// command an artifact card copies works on any of the workspace's
/// machines, not only the one holding the log. None when the id is local,
/// or this machine does not sync, or the registry holds no session under it
/// that this machine's user keys open: `--resume` then goes on as before.
pub(super) fn resume(ctx: &mut Ctx) -> Option<Result<(), Error>> {
    let id = ctx.f.resume.trim().to_string();
    if !krowk_harness::log::valid_id(&id) {
        return None;
    }
    let dir = krowk_harness::log::sessions_dir(ctx.io.env).ok()?;
    if dir.join(&id).join(krowk_harness::log::EVENTS_FILE).is_file() {
        return None;
    }
    let k = keys(ctx).ok()?;
    let api = signed(ctx, &k, "krowk --resume").ok()?;
    let s = api.show_sync_session(&id).ok()?;
    krowk_harness::sync::store::open_session_key(&s, &id, &k.user, &k.chain, krowk_client::session_record::Signer::EverHeld).ok()?;
    // Said plainly, on stderr, where no TUI draws it: what follows is
    // stream-json, as `krowk sync attach` prints it.
    if !on_a_terminal(ctx) {
        let _ = writeln!(ctx.io.stderr, "krowk: session {id} runs on another machine — following it through sync as stream-json (`krowk sync attach`)");
    }
    Some(attach(ctx, &[id]))
}

#[cfg(test)]
mod tests {
    use super::relay_for;

    /// The production registry's devices dial the hosted relay with no
    /// `KROWK_RELAY_URL`; a stand-in or custom registry keeps the local
    /// relay, and `KROWK_RELAY_URL` always wins.
    #[test]
    fn r_relay_1_the_production_registry_defaults_to_the_hosted_relay() {
        assert_eq!(relay_for("", "https://api.krowk.com/v1"), "wss://relay.krowk.com");
        assert_eq!(relay_for(" ", "https://api.krowk.com/v1/"), "wss://relay.krowk.com");
        assert_eq!(relay_for("", "http://127.0.0.1:3000/v1"), "ws://127.0.0.1:7790");
        assert_eq!(relay_for("", "https://staging.example.com/v1"), "ws://127.0.0.1:7790");
        assert_eq!(relay_for("ws://10.0.0.2:7790", "https://api.krowk.com/v1"), "ws://10.0.0.2:7790");
    }
}
