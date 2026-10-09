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
//! - `take <session>`: moves it here from the machine hosting it
//!   (R-HAND-1): checked first that it can run here, then the host
//!   checkpoints, ships its uncommitted work and hands its lease over; the
//!   work lands in a fresh worktree of this clone, and this machine hosts
//!   the session from there until interrupted.
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
/// tailscaled answers and is up, a viewer's ticket checked as the relay
/// checks it, with the registry's ticket public keys (`ticket_keys`).
/// Without them there is no direct listener, never a weaker one: nothing
/// but a ticket may let a device in. Either missing, the host says why in
/// one line and goes by the relay; Tailscale is asked first, so a machine
/// without it asks the registry nothing more. `KROWK_TAILSCALE_SAME_USER=1`
/// also requires tailscaled to name the far end as this tailnet user
/// (R-NET-3). The LAN address is offered only with `KROWK_DIRECT_LAN=1`: it
/// is off the tailnet, so plain `ws://` there is reachable by the whole
/// network.
fn direct(ctx: &mut Ctx, api: &Client) -> Result<Option<krowk_harness::sync::direct::Config>, Error> {
    let socket = krowk_harness::sync::tailscale::socket(ctx.io.env);
    // A named file that does not read is a mistake said at once, Tailscale
    // up or not.
    let named = named_ticket_keys(ctx)?;
    let roster = match krowk_harness::sync::direct::running(&socket) {
        Ok(_) => named.map_or_else(|| fetched_ticket_keys(ctx, api), Ok),
        Err(why) => Err(why),
    };
    let roster = match roster {
        Ok(roster) => roster,
        Err(why) => {
            let _ = writeln!(ctx.io.stderr, "krowk: no direct path ({why}); the session goes by the relay");
            return Ok(None);
        }
    };
    Ok(Some(krowk_harness::sync::direct::Config { socket, roster, same_user: ctx.env("KROWK_TAILSCALE_SAME_USER") == "1", lan: ctx.env("KROWK_DIRECT_LAN") == "1", stop: None }))
}

/// Where the registry's ticket keys are kept between hostings, in krowk's
/// cache: `{"registry": "<base url>", "fetchedMs": <ms>, "ticketKeys": {...}}`.
const TICKET_KEYS_CACHE: &str = "relay-ticket-keys.json";

/// How long kept keys stand in for a registry that does not answer. A key
/// the registry stops publishing — retired, or pulled — is trusted no
/// longer than this after the last fetch that listed it.
const TICKET_KEYS_KEPT_MS: i64 = 24 * 60 * 60 * 1000;

/// The keys `KROWK_RELAY_TICKET_KEYS` names a file of (the one `krowk relay
/// serve --ticket-keys` reads), for a stand-in registry: they win over the
/// registry's. None when it is unset; a file that does not read is an error.
fn named_ticket_keys(ctx: &Ctx) -> Result<Option<krowk_harness::relay::Roster>, Error> {
    let named = ctx.env("KROWK_RELAY_TICKET_KEYS");
    let named = named.trim();
    if named.is_empty() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(named).map_err(|e| fail("bad_ticket_keys", format!("KROWK_RELAY_TICKET_KEYS names {named}, which could not be read: {e}")))?;
    krowk_harness::relay::Roster::parse(&text).map(Some).map_err(|e| fail("bad_ticket_keys", e))
}

/// The registry's own ticket keys, from `GET /v1/relay/ticket_keys`, or
/// why there are none: fetched at every hosting — so a rotation's next
/// key, published before the registry signs with it, reaches the next
/// session with nothing reinstalled — and kept (`kept_keys`).
fn fetched_ticket_keys(ctx: &Ctx, api: &Client) -> Result<krowk_harness::relay::Roster, String> {
    use krowk_harness::relay::Roster;
    let cache = krowk_api::home::dir(ctx.io.env).ok().map(|h| h.join(krowk_api::home::CACHE).join(TICKET_KEYS_CACHE));
    let now = jiff::Timestamp::now().as_millisecond();
    let v = match api.relay_ticket_keys() {
        Ok(v) => v,
        Err(e) => {
            let why = format!("the registry's relay ticket keys could not be fetched: {}", e.code());
            let kept = if unreachable(&e) { cache.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|text| kept_keys(&text, &api.base_url, now)) } else { None };
            return kept.ok_or(why);
        }
    };
    let keys = json!({"registry": api.base_url, "fetchedMs": now, "ticketKeys": v["ticketKeys"]});
    let roster = Roster::parse(&keys.to_string()).map_err(|e| format!("the registry's relay ticket keys do not read: {e}"))?;
    // By rename, so a host starting beside another never reads half a file.
    if let Some(path) = cache.filter(|p| p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())) {
        let tmp = path.with_file_name(format!(".{TICKET_KEYS_CACHE}.{}", std::process::id()));
        if std::fs::write(&tmp, format!("{keys}\n")).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok(roster)
}

/// Whether a failed fetch leaves the kept keys standing: only when the
/// registry could not be reached or was briefly unable to answer (nothing
/// answered, 429, a 5xx), never when it answered what it meant — a 404
/// from a registry that publishes none, or `relay_tickets_unconfigured`.
fn unreachable(e: &Error) -> bool {
    (e.status == 0 || e.status == 429 || e.status >= 500) && e.code() != "relay_tickets_unconfigured"
}

/// The keys kept in `text`: read back only for the registry they were
/// fetched from, and only within `TICKET_KEYS_KEPT_MS` of that fetch.
fn kept_keys(text: &str, registry: &str, now_ms: i64) -> Option<krowk_harness::relay::Roster> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let fetched = v["fetchedMs"].as_i64()?;
    if v["registry"] != registry || now_ms.saturating_sub(fetched) > TICKET_KEYS_KEPT_MS || fetched > now_ms {
        return None;
    }
    krowk_harness::relay::Roster::parse(text).ok()
}

/// `krowk hosts` (R-NET-4): the machines of yours that host synced
/// sessions, from the registry's sessions and your device list, each
/// marked hosting when it holds one's lease now and direct when the
/// tailnet reaches it — no tag, and no step in the Tailscale admin
/// console. Tailscale not answering here leaves the list whole, with no
/// machine marked direct.
pub(super) fn hosts(ctx: &mut Ctx) -> Result<(), Error> {
    use krowk_harness::sync::tailscale;
    let k = keys(ctx)?;
    let api = signed(ctx, &k, "krowk hosts")?;
    let (listed, _) = viewer::list(&api, &k.user, &k.chain).map_err(|e| fail("sync_failed", e))?;
    let status = tailscale::status(&tailscale::socket(ctx.io.env)).ok().filter(|s| s.running());
    let hosts = krowk_harness::sync::hosts::hosts(&listed, &k.chain, &k.device.to_string(), status.as_ref());
    if ctx.format == crate::output::Format::Json {
        let rows: Vec<_> = hosts
            .iter()
            .map(|h| {
                let (dns, ips) = h.tailnet.as_ref().map_or((String::new(), Vec::new()), |t| (t.dns_name.clone(), t.ips.clone()));
                // `online` keeps its meaning from before: on the tailnet,
                // which this machine is whenever its Tailscale is up.
                let online = if h.this_machine { status.is_some() } else { h.direct() };
                json!({"device": h.device, "name": h.name, "os": h.os, "thisMachine": h.this_machine, "hosting": h.hosting, "online": online, "direct": h.direct(), "sessions": h.sessions, "dnsName": dns, "addresses": ips})
            })
            .collect();
        return ctx.emit(&json!({"hosts": rows, "tailscale": status.is_some()}).to_string());
    }
    for h in &hosts {
        let reach = match (&h.tailnet, &h.peer) {
            _ if h.this_machine => "this machine",
            _ if status.is_none() => "Tailscale is not running here",
            (_, Some(p)) if p.online => "reachable directly",
            (_, Some(_)) => "offline on the tailnet",
            (Some(_), None) => "not on this tailnet",
            (None, None) => "no tailnet address known",
        };
        let n = if h.sessions == 1 { "1 session".to_string() } else { format!("{} sessions", h.sessions) };
        let _ = writeln!(ctx.io.stdout, "{}  {}  {}  {n}", super::sync::printable(&h.name), if h.hosting { "hosting" } else { "not hosting" }, reach);
    }
    if hosts.is_empty() {
        let _ = writeln!(ctx.io.stdout, "no machine of yours hosts a synced session yet — `krowk sync host <session>` on one hosts it");
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
    let cwd = std::env::current_dir().map_err(|e| fail("no_cwd", e.to_string()))?;
    host_loop(ctx, &session, &cwd, None)
}

/// Hosts until the bridge ends, again from the top after a device's
/// removal; `handed` is the lease a handoff brought, for the first run.
fn host_loop(ctx: &mut Ctx, session: &str, cwd: &std::path::Path, mut handed: Option<krowk_harness::sync::Handed>) -> Result<(), Error> {
    loop {
        match host_once(ctx, session, cwd, handed.take()) {
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
fn host_once(ctx: &mut Ctx, session: &str, cwd: &std::path::Path, handed: Option<krowk_harness::sync::Handed>) -> Result<(), Error> {
    let session = session.to_string();
    // The list first: a newer key it takes up is the one `keys` reads.
    current(ctx)?;
    let k = keys(ctx)?;
    let api = Arc::new(signed(ctx, &k, "krowk sync host")?);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let cwd = cwd.to_path_buf();
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
        // Kept in the session's sealed index, for `krowk hosts`.
        tailnet: krowk_harness::sync::tailscale::status(&krowk_harness::sync::tailscale::socket(ctx.io.env)).ok().and_then(|s| s.tailnet()),
        handed,
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

/// `krowk sync take <session>` (R-HAND-1, R-HAND-5, R-INST-5, R-CRED-1):
/// the session moved here from the machine hosting it, run from a clone of
/// its repository. Nothing moves until this machine has shown it can run
/// the session — its instance here by name, signed in, its model reachable,
/// a backend's repository trusted — so a refusal leaves it on its host.
pub(super) fn take(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    use krowk_harness::sync::{handoff, store};
    let session = one(args, "take")?;
    if !krowk_harness::log::valid_id(&session) {
        return Err(fail("bad_session", format!("{session:?} is not a session id — `krowk sync sessions` lists the synced ones")));
    }
    let cwd = std::env::current_dir().map_err(|e| fail("no_cwd", e.to_string()))?;
    current(ctx)?;
    let k = keys(ctx)?;
    let api = Arc::new(signed(ctx, &k, "krowk sync take")?);
    let me = k.device.to_string();
    let read_log = |api: &Client| -> Result<Vec<serde_json::Value>, Error> {
        let s = api.show_sync_session(&session).map_err(|e| if e.status == 404 { fail("no_session", format!("the registry holds no synced session {session} — `krowk sync sessions` lists them")) } else { e })?;
        let key = store::open_session_key(&s, &session, &k.user, &k.chain, krowk_client::session_record::Signer::EverHeld).map_err(|e| fail("sync_failed", e))?;
        let index = store::open_index(&key, &session, &s.sealed_index).map_err(|e| fail("sync_failed", e))?;
        Ok(store::attach(api, &key, &session, index, None).map_err(|e| fail("sync_failed", e))?.events)
    };
    match api.show_sync_session(&session).ok().and_then(|s| s.lease) {
        Some(l) if l.device == me => return Err(fail("already_here", format!("this machine hosts session {session} already"))),
        Some(_) => {}
        None => return Err(fail("no_host", format!("session {session} has no host right now, so there is no live work to move; `krowk sync host {session}` on the machine that has it, then take it again"))),
    }
    let events = read_log(&api)?;
    let home = ready_here(ctx, &session, &events, &cwd)?;

    // The host checkpoints, ships the bundle and hands the lease over.
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let v = keys(ctx)?;
    let o = viewer::Options { relay: relay(ctx, &api.base_url), env, api: api.clone(), device: v.device, signing: v.signing, keys: v.user, chain: v.chain, session: session.clone(), known: None };
    let _ = writeln!(ctx.io.stderr, "krowk: asking the session's host to hand it over…");
    let handed = krowk_harness::sync::ask_handoff(o, std::time::Duration::from_secs(120)).map_err(|e| fail("handoff_refused", e))?;

    // The lease is this machine's now: anything failing from here gives it
    // back, so the session can be hosted again.
    let give_back = |e: Error| {
        let _ = api.release_lease(&session, &handed.token);
        e
    };
    let key = handoff::key_from_hex(&handed.key).ok_or_else(|| give_back(fail("handoff_failed", "the host sent no key this krowk reads")))?;
    let sealed = api.read_transport(&handed.transport).map_err(give_back)?;
    let plain = krowk_client::e2e::open_transport(&sealed, &krowk_harness::daemon::ws::uuid(&session), &key).map_err(|e| give_back(fail("handoff_failed", e.to_string())))?;
    let bundle = handoff::Bundle::decode(&plain, &session).map_err(|e| give_back(fail("handoff_failed", e)))?;
    let root = krowk_api::home::worktrees_root(ctx.io.env).ok_or_else(|| give_back(fail("no_home", "krowk found no home directory to make worktrees under — set HOME or XDG_DATA_HOME")))?;
    let applied = handoff::apply(&bundle, &cwd, &root).map_err(|e| give_back(fail("handoff_failed", format!("{e} — the session's lease was given back; host it again where it was"))))?;
    let _ = api.spend_transport(&handed.transport);
    if let Some(home) = &home {
        handoff::place_transcript(&bundle, home, &applied.cwd).map_err(|e| give_back(fail("handoff_failed", e)))?;
        // The worktree is this repository's, which a backend may run in.
        let trusted = krowk_harness::trust::Store::new(Some(super::providers::krowk_dir()?.join(krowk_harness::trust::FILE)), Some(ctx.env("HOME")).filter(|h| !h.is_empty()).map(Into::into));
        let _ = trusted.trust(&krowk_harness::trust::root(&applied.cwd));
    }
    // The log as the host left it, checkpoint and all.
    let events = read_log(&api).map_err(give_back)?;
    let sessions = krowk_harness::log::sessions_dir(ctx.io.env)?;
    handoff::restore_log(&sessions, &session, &events, &applied.cwd, &me, &handed.from_device).map_err(|e| give_back(fail("handoff_failed", e)))?;
    let _ = writeln!(ctx.io.stderr, "krowk: session {session} moved here, working in {} (branch {}); hosting it until interrupted", applied.cwd.display(), applied.worktree.branch());
    let _held = applied.held;
    host_loop(ctx, &session, &applied.cwd, Some(handed))
}

/// Whether the session can run here (R-INST-5, R-CRED-1): the instance its
/// next turn runs on, by name, ready — signed in, its key set, its vendor's
/// login good — and for a backend this repository trusted. The backend's
/// config directory, where its transcript goes; None for a native session.
fn ready_here(ctx: &Ctx, session: &str, events: &[serde_json::Value], cwd: &std::path::Path) -> Result<Option<std::path::PathBuf>, Error> {
    use krowk_harness::{readiness, trust};
    let Some(name) = krowk_harness::sync::handoff::last_instance(events) else { return Ok(None) };
    let stays = "the session stays where it is";
    let config = super::prompt::config_json()?;
    let registry = krowk_harness::instances::Registry::resolve(&super::prompt::instances_from(&config)?, &krowk_api::home::process_env);
    let inst = registry.get(&name).map_err(|_| fail("instance_missing", format!("session {session} runs on the {name} instance, which this machine does not have — {stays}; connect it here under the same name with `krowk connect`, then take it again")))?;
    let creds = super::providers::credentials_path()?;
    let probe = readiness::Probe { dir: cwd.to_path_buf(), within: std::time::Duration::from_secs(20) };
    if let Some(e) = readiness::check(inst, &creds, &probe).refusal(inst) {
        return Err(fail(&e.code, format!("{} — {stays}", e.message)));
    }
    let Some(b) = &inst.backend else { return Ok(None) };
    let store = trust::Store::new(Some(super::providers::krowk_dir()?.join(trust::FILE)), Some(ctx.env("HOME")).filter(|h| !h.is_empty()).map(Into::into));
    let root = trust::root(cwd);
    if !store.trusts(&root) {
        return Err(fail("untrusted_directory", format!("{} is not a repository you have trusted, and the session runs on {}, which runs what a repository configures — {stays}; run krowk there once on a terminal and trust it, then take it again", root.display(), inst.vendor)));
    }
    Ok(b.home.clone())
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

    /// Ticket 42: kept ticket keys stand in only for the registry they came
    /// from, and only for a day after the fetch that listed them.
    #[test]
    fn kept_ticket_keys_are_read_back_only_for_their_registry_and_within_a_day() {
        use super::{kept_keys, TICKET_KEYS_KEPT_MS};
        let (registry, fetched) = ("https://api.krowk.com/v1", 1_790_000_000_000_i64);
        let kept = |registry: &str| serde_json::json!({"registry": registry, "fetchedMs": fetched, "ticketKeys": {"00000000000000aa": "ab".repeat(32)}}).to_string();

        let roster = kept_keys(&kept(registry), registry, fetched + 1000).expect("the same registry's, a second later");
        assert_eq!(roster.keys, [([0, 0, 0, 0, 0, 0, 0, 0xaa], [0xab; 32])]);
        assert!(kept_keys(&kept(registry), registry, fetched + TICKET_KEYS_KEPT_MS).is_some(), "a day on");
        assert!(kept_keys(&kept(registry), registry, fetched + TICKET_KEYS_KEPT_MS + 1).is_none(), "past a day");
        assert!(kept_keys(&kept(registry), registry, fetched - 1).is_none(), "fetched in the future");
        assert!(kept_keys(&kept("http://127.0.0.1:3000/v1"), registry, fetched).is_none(), "another registry's");
        let undated = serde_json::json!({"registry": registry, "ticketKeys": {"00000000000000aa": "ab".repeat(32)}}).to_string();
        assert!(kept_keys(&undated, registry, fetched).is_none(), "no fetch time");
        assert!(kept_keys("not json", registry, fetched).is_none());
    }

    /// Ticket 42: a fetch that failed leaves the kept keys standing only
    /// when the registry could not answer, never when it answered no.
    #[test]
    fn kept_ticket_keys_stand_in_only_for_a_registry_that_could_not_answer() {
        use super::unreachable;
        let e = |status: u16, code: &str| krowk_api::Error { status, body: [("error".to_string(), serde_json::json!(code))].into_iter().collect() };
        assert!(unreachable(&e(0, "")), "nothing answered");
        assert!(unreachable(&e(502, "")));
        assert!(unreachable(&e(429, "rate_limited")));
        assert!(!unreachable(&e(503, "relay_tickets_unconfigured")), "the registry says it has none");
        assert!(!unreachable(&e(404, "no_such_endpoint")), "a registry that publishes none");
        let mut marked = e(409, "conflict");
        marked.body.insert("retryable".into(), serde_json::json!(true));
        assert!(!unreachable(&marked), "by the status, whatever the body says");
    }
}
