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

/// Direct paths beside the relay (R-NET-1): offered whenever the local
/// tailscaled answers, a viewer's ticket checked as the relay checks it,
/// with the registry's ticket public keys (`ticket_keys`). Without them
/// there is no direct listener, never a weaker one: nothing but a ticket
/// may let a device in, so the host says why and goes by the relay.
/// `KROWK_TAILSCALE_SAME_USER=1` also requires tailscaled to name the far
/// end as this tailnet user (R-NET-3). The LAN address is offered only with
/// `KROWK_DIRECT_LAN=1`: it is off the tailnet, so plain `ws://` there is
/// reachable by the whole network.
fn direct(ctx: &mut Ctx, api: &Client) -> Result<Option<krowk_harness::sync::direct::Config>, Error> {
    let roster = match ticket_keys(ctx, api)? {
        Ok(roster) => roster,
        Err(why) => {
            let _ = writeln!(ctx.io.stderr, "krowk: no direct path ({why}); the session goes by the relay");
            return Ok(None);
        }
    };
    Ok(Some(krowk_harness::sync::direct::Config { socket: krowk_harness::sync::tailscale::socket(ctx.io.env), roster, same_user: ctx.env("KROWK_TAILSCALE_SAME_USER") == "1", lan: ctx.env("KROWK_DIRECT_LAN") == "1", stop: None }))
}

/// Where the registry's ticket keys are kept between hostings, in krowk's
/// cache: `{"registry": "<base url>", "ticketKeys": {...}}`.
const TICKET_KEYS_CACHE: &str = "relay-ticket-keys.json";

/// The keys a direct listener trusts, or why there are none.
/// `KROWK_RELAY_TICKET_KEYS` names a file of them (the one `krowk relay
/// serve --ticket-keys` reads), for a stand-in registry, and a file that
/// does not read is an error. Otherwise they are the registry's own, from
/// `GET /v1/relay/ticket_keys`, fetched at every hosting — so a rotation's
/// next key, published before the registry signs with it, reaches the next
/// session with nothing reinstalled — and kept, so a registry that does not
/// answer that once leaves the last keys in place. Only the same registry's
/// keys are ever read back.
fn ticket_keys(ctx: &Ctx, api: &Client) -> Result<Result<krowk_harness::relay::Roster, String>, Error> {
    use krowk_harness::relay::Roster;
    let named = ctx.env("KROWK_RELAY_TICKET_KEYS");
    let named = named.trim();
    if !named.is_empty() {
        let text = std::fs::read_to_string(named).map_err(|e| fail("bad_ticket_keys", format!("KROWK_RELAY_TICKET_KEYS names {named}, which could not be read: {e}")))?;
        return Roster::parse(&text).map(Ok).map_err(|e| fail("bad_ticket_keys", e));
    }
    let cache = krowk_api::home::dir(ctx.io.env).ok().map(|h| h.join(krowk_api::home::CACHE).join(TICKET_KEYS_CACHE));
    let fetched = api.relay_ticket_keys().map_err(|e| format!("the registry's relay ticket keys could not be fetched: {}", e.code())).and_then(|v| {
        let keys = json!({"registry": api.base_url, "ticketKeys": v["ticketKeys"]});
        Roster::parse(&keys.to_string()).map(|roster| (roster, keys)).map_err(|e| format!("the registry's relay ticket keys do not read: {e}"))
    });
    match fetched {
        Ok((roster, keys)) => {
            // By rename, so a host starting beside another never reads half a file.
            if let Some(path) = cache.filter(|p| p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())) {
                let tmp = path.with_file_name(format!(".{TICKET_KEYS_CACHE}.{}", std::process::id()));
                if std::fs::write(&tmp, format!("{keys}\n")).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            Ok(Ok(roster))
        }
        Err(why) => {
            let kept = cache.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok()).filter(|v| v["registry"] == api.base_url.as_str());
            Ok(kept.and_then(|v| Roster::parse(&v.to_string()).ok()).ok_or(why))
        }
    }
}

/// `krowk hosts`: the tailnet's machines tagged `tag:krowk-host`, from the
/// local tailscaled, with no pairing step (R-NET-4).
pub(super) fn hosts(ctx: &mut Ctx) -> Result<(), Error> {
    use krowk_harness::sync::tailscale;
    let s = tailscale::status(&tailscale::socket(ctx.io.env)).map_err(|e| fail("tailscale_unavailable", e))?;
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
    loop {
        match host_once(ctx, &session) {
            // A device was removed while it ran: hosted again from the top,
            // so the new list is verified and its user key taken up.
            Err(e) if e.code() == "device_list_moved" => {
                let _ = writeln!(ctx.io.stderr, "{}", e.fix());
            }
            other => return other,
        }
    }
}

/// One run of the bridge, from the list's check to the bridge's end.
fn host_once(ctx: &mut Ctx, session: &str) -> Result<(), Error> {
    let session = session.to_string();
    // The list first: a newer key it takes up is the one `keys` reads.
    current(ctx)?;
    let k = keys(ctx)?;
    let api = Arc::new(signed(ctx, &k, "krowk sync host")?);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let cwd = std::env::current_dir().map_err(|e| fail("no_cwd", e.to_string()))?;
    let spawn = super::host::spawner(ctx)?;
    let direct = direct(ctx, &api)?;
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
        direct,
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
