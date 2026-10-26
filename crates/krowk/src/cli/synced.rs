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
//!   a request, `/interrupt` or `/steer` the turn, `/model` to switch its
//!   model, `/switch` to take a rate limit's offer, or `/interrupt` one of
//!   its subagents by its session id (R-SUB-12). Stdin's end ends it once
//!   every command sent is answered.
//! - `new --host <name> --cwd <dir> "<prompt>"`: starts a session on another
//!   of the person's machines (R-HOST-4), checked first against what it
//!   announced, then follows it as `attach` does.
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
/// network. With it, a machine without Tailscale offers the LAN alone,
/// advertised over mDNS.
fn direct(ctx: &mut Ctx, api: &Client) -> Result<Option<krowk_harness::sync::direct::Config>, Error> {
    let socket = krowk_harness::sync::tailscale::socket(ctx.io.env);
    // A named file that does not read is a mistake said at once, Tailscale
    // up or not.
    let named = named_ticket_keys(ctx)?;
    let (same_user, lan) = (ctx.env("KROWK_TAILSCALE_SAME_USER") == "1", ctx.env("KROWK_DIRECT_LAN") == "1");
    // With the LAN offered, Tailscale not running leaves the LAN, found
    // over mDNS too (ticket 27); the listener says if there is none.
    let roster = match krowk_harness::sync::direct::running(&socket) {
        Err(why) if !lan || same_user => Err(why),
        _ => named.map_or_else(|| fetched_ticket_keys(ctx, api), Ok),
    };
    let roster = match roster {
        Ok(roster) => roster,
        Err(why) => {
            let _ = writeln!(ctx.io.stderr, "krowk: no direct path ({why}); the session goes by the relay");
            return Ok(None);
        }
    };
    Ok(Some(krowk_harness::sync::direct::Config { socket, roster, same_user, lan, lan_at: None, stop: None }))
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

/// `krowk hosts` (R-NET-4, R-HOST-3): the machines of yours that host
/// synced sessions, from the registry's sessions and your device list, each
/// marked hosting when it holds one's lease now and direct when the
/// tailnet reaches it — no tag, and no step in the Tailscale admin
/// console. Tailscale not answering here leaves the list whole, with no
/// machine marked direct. And every machine that announced itself, hosting
/// or not: online or when it was last seen, its version, its instances and
/// its directories. An announcement that does not open here is counted,
/// never shown.
pub(super) fn hosts(ctx: &mut Ctx) -> Result<(), Error> {
    use krowk_harness::sync::{hosts::Announced, tailscale};
    let k = keys(ctx)?;
    let api = signed(ctx, &k, "krowk hosts")?;
    // A removed device is refused here (`device_revoked`), in the
    // registry's own words. A registry that keeps no presences yet has
    // none to list.
    let presences = match api.list_presences() {
        Ok(p) => p,
        Err(e) if e.status == 404 => Vec::new(),
        Err(e) => return Err(e),
    };
    let (listed, _) = viewer::list(&api, &k.user, &k.chain)?;
    let mut unopened = 0;
    let announced: Vec<Announced> = presences
        .into_iter()
        .filter_map(|p| {
            let opened = DeviceId::parse(&p.device).ok_or_else(|| "not a device id".to_string()).and_then(|d| krowk_client::presence::open(&p.sealed, &d, &k.user, &k.chain));
            match opened {
                Ok(announcement) => Some(Announced { online: p.online, updated_at: p.updated_at, announcement }),
                Err(_) => {
                    unopened += 1;
                    None
                }
            }
        })
        .collect();
    let status = tailscale::status(&tailscale::socket(ctx.io.env)).ok().filter(|s| s.running());
    let hosts = krowk_harness::sync::hosts::hosts(&listed, &announced, &k.chain, &k.device.to_string(), status.as_ref());
    if ctx.format == crate::output::Format::Json {
        let rows: Vec<_> = hosts
            .iter()
            .map(|h| {
                let (dns, ips) = h.tailnet.as_ref().map_or((String::new(), Vec::new()), |t| (t.dns_name.clone(), t.ips.clone()));
                // On the tailnet — which this machine is whenever its
                // Tailscale is up — or announced and not lapsed.
                let tailnet = if h.this_machine { status.is_some() } else { h.direct() };
                let a = h.announced.as_ref();
                let instances: Vec<_> = a.map(|a| &a.announcement.instances[..]).unwrap_or_default().iter().map(|i| json!({"name": i.name, "kind": i.kind, "ready": i.ready, "state": i.state, "fix": i.fix, "models": i.models, "limit": i.limit})).collect();
                json!({"device": h.device, "name": h.name, "os": h.os, "thisMachine": h.this_machine, "hosting": h.hosting, "online": tailnet || a.is_some_and(|a| a.online), "direct": h.direct(), "sessions": h.sessions, "dnsName": dns, "addresses": ips,
                    "announced": a.is_some(), "lastSeen": a.map(|a| &a.updated_at), "version": a.map(|a| &a.announcement.version), "instances": instances, "directories": a.map(|a| &a.announcement.directories[..]).unwrap_or_default()})
            })
            .collect();
        return ctx.emit(&json!({"hosts": rows, "tailscale": status.is_some(), "unopened": unopened}).to_string());
    }
    let now = jiff::Timestamp::now();
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
        if let Some(a) = &h.announced {
            for line in announced_lines(a, now) {
                let _ = writeln!(ctx.io.stdout, "  {}", super::sync::printable(&line));
            }
        }
    }
    if unopened > 0 {
        let _ = writeln!(ctx.io.stdout, "{unopened} host{} announcement could not be opened on this machine", if unopened == 1 { "'s" } else { "s'" });
    }
    if hosts.is_empty() {
        let _ = writeln!(ctx.io.stdout, "no machine of yours hosts a synced session or announces itself yet — `krowk sync host <session>` on one hosts it");
    }
    Ok(())
}

/// The lines under an announced host: online or when it was last seen,
/// its ready instances with their first model, the others with their fix,
/// and its directories, home as `~`.
fn announced_lines(a: &krowk_harness::sync::hosts::Announced, now: jiff::Timestamp) -> Vec<String> {
    let r = &a.announcement;
    let mut out = vec![if a.online { format!("online · krowk {}", r.version) } else { format!("offline · last seen {}", ago(&a.updated_at, now)) }];
    let ready: Vec<String> = r.instances.iter().filter(|i| i.ready).map(|i| match &i.models[..] {
        [] => i.name.clone(),
        [one] => format!("{} ({one})", i.name),
        [first, ..] => format!("{} ({first}, …)", i.name),
    }).collect();
    if !ready.is_empty() {
        out.push(format!("ready: {}", ready.join(", ")));
    }
    for i in r.instances.iter().filter(|i| !i.ready) {
        out.push(match &i.fix {
            Some(fix) => format!("not ready: {} — {fix}", i.name),
            None => format!("not ready: {} ({})", i.name, i.state),
        });
    }
    if !r.directories.is_empty() {
        const SHOWN: usize = 3;
        let mut dirs: Vec<String> = r.directories.iter().take(SHOWN).map(|d| home_as_tilde(d)).collect();
        if r.directories.len() > SHOWN {
            dirs.push("…".into());
        }
        out.push(format!("directories: {}", dirs.join(", ")));
    }
    out
}

/// A path on another machine with its home shown as `~`: `/home/<name>`,
/// `/Users/<name>` or `/root`, the homes krowk's platforms make.
fn home_as_tilde(path: &str) -> String {
    let rest = |prefix: &str| path.strip_prefix(prefix).and_then(|r| match r.split_once('/') {
        Some((name, rest)) if !name.is_empty() => Some(format!("~/{rest}")),
        None if !r.is_empty() => Some("~".into()),
        _ => None,
    });
    rest("/home/").or_else(|| rest("/Users/")).or_else(|| match path.strip_prefix("/root") {
        Some("") => Some("~".into()),
        Some(r) if r.starts_with('/') => Some(format!("~{r}")),
        _ => None,
    }).unwrap_or_else(|| path.to_string())
}

/// How long ago `at` (RFC 3339) was: `12m ago`; "a while ago" when it
/// does not read.
fn ago(at: &str, now: jiff::Timestamp) -> String {
    let Ok(at) = at.parse::<jiff::Timestamp>() else { return "a while ago".into() };
    let s = now.as_second().saturating_sub(at.as_second()).max(0);
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
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
    let (listed, unreadable) = viewer::list(&api, &k.user, &k.chain)?;
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
    // Synced already by this machine's daemon (automatic sync): a second
    // bridge would only be refused the lease its first holds. Asked of a
    // daemon that runs; never one started for it.
    let bridged = krowk_harness::daemon::status(ctx.io.env, super::VERSION).ok().flatten().and_then(|s| s.bridged).unwrap_or_default();
    if bridged.contains(&session) {
        let summary = format!("session {session} is synced already by this machine's host daemon (automatic sync)");
        return super::chain::say(ctx, json!({ "session": session, "synced": true, "by": "daemon" }), summary);
    }
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
    let spawn = super::host::spawner(ctx)?;
    let o = options(ctx, session, cwd, handed)?;
    krowk_harness::sync::run_host(o, ctx.io.env, cwd, super::VERSION, &spawn).map_err(|(code, message)| fail(&code, message))
}

/// A bridge's options for `session`, whose own directory is `cwd`: the
/// device list brought up to date first, then the keys it leaves current,
/// the signed registry client, the relay and the direct paths. What `krowk
/// sync host` runs the bridge with, and what the host daemon's automatic
/// sync builds each of its bridges with (`host::serve`). May ask the
/// registry: never on the daemon's thread.
pub(super) fn options(ctx: &mut Ctx, session: &str, cwd: &std::path::Path, handed: Option<krowk_harness::sync::Handed>) -> Result<host::Options, Error> {
    // The list first: a newer key it takes up is the one `keys` reads.
    current(ctx)?;
    let k = keys(ctx)?;
    let api = Arc::new(signed(ctx, &k, "krowk sync host")?);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let direct = direct(ctx, &api)?;
    Ok(host::Options {
        relay: relay(ctx, &api.base_url),
        env,
        api,
        device: k.device,
        signing: k.signing,
        keys: k.user,
        chain: k.chain,
        session: session.to_string(),
        title: String::new(),
        cwd: cwd.display().to_string(),
        sessions: krowk_harness::log::sessions_dir(ctx.io.env)?,
        ttl: host::LEASE_TTL,
        keep: host::KEEP,
        direct,
        // Kept in the session's sealed index, for `krowk hosts`.
        tailnet: krowk_harness::sync::tailscale::status(&krowk_harness::sync::tailscale::socket(ctx.io.env)).ok().and_then(|s| s.tailnet()),
        handed,
        approval_wait: host::APPROVAL_WAIT,
        // Its own list reader: the host daemon's supervisor puts its one
        // reader's slot here for each bridge it starts.
        stale: None,
        supervised: None,
    })
}

/// What the host daemon announces itself with (R-HOST-3): the keys the
/// list leaves current — with `fresh`, the list brought up to date from the
/// registry first, so a new user key is taken up — and the signed client.
/// May ask the registry: never on the daemon's thread.
pub(super) fn announcer(ctx: &mut Ctx, fresh: bool) -> Result<krowk_harness::sync::announce::Announcer, Error> {
    if fresh {
        current(ctx)?;
    }
    let k = keys(ctx)?;
    let api = Arc::new(signed(ctx, &k, "automatic sync")?);
    Ok(krowk_harness::sync::announce::Announcer { api, keys: k.user, chain: k.chain, device: k.device })
}

/// Whether sync is set up here, as the host daemon's automatic sync asks
/// before each bridge: this machine's device key, the person's user key and
/// a verified device list kept, and a key to the workspace. File reads
/// only — never the registry — so a machine that does not sync makes no
/// network call at all (R-SYNC-1).
pub(super) fn ready(ctx: &Ctx) -> Result<(), Error> {
    let ks = keystore(ctx)?;
    let unreadable = |e: String| fail("keys_unreadable", e);
    if ks.device().map_err(unreadable)?.is_none() || ks.user_keys().map_err(unreadable)?.is_none() || ks.device_list().map_err(unreadable)?.is_none() {
        return Err(super::chain::not_set_up());
    }
    keyed_client(ctx, "automatic sync").map(|_| ())
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
    let sessions = krowk_harness::log::sessions_dir(ctx.io.env)?;
    handoff::check_local_log(&sessions, &session, &events).map_err(|e| fail("handoff_refused", e))?;

    // The host checkpoints, ships the bundle and hands the lease over.
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let v = keys(ctx)?;
    let o = viewer::Options { relay: relay(ctx, &api.base_url), env, api: api.clone(), device: v.device, signing: v.signing, keys: v.user, chain: v.chain, session: session.clone(), known: None };
    let _ = writeln!(ctx.io.stderr, "krowk: asking the session's host to hand it over…");
    let handed = match krowk_harness::sync::ask_handoff(o, std::time::Duration::from_secs(300)) {
        Ok(h) => h,
        // The host may have handed the lease over with an answer that never
        // came: said as it is, not as a refusal.
        Err(e) if api.show_sync_session(&session).ok().and_then(|s| s.lease).is_some_and(|l| l.device == me) => {
            return Err(fail("handoff_lost", format!("the host handed session {session} to this machine, but its answer did not arrive ({e}); the lease lapses in a minute — then `krowk sync host {session}` on the machine whose work it is")));
        }
        Err(e) => return Err(fail("handoff_refused", e)),
    };
    // Room to apply the work before the bridge renews it: the handed
    // lease's TTL is the bridge's minute.
    let _ = api.renew_lease(&session, &me, &handed.token, 600, krowk_api::relay_env(&api.base_url, ctx.io.env));

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
    // And past here the worktree goes too.
    let undo = |e: Error| {
        let _ = krowk_harness::worktree::discard(&applied.worktree);
        give_back(e)
    };
    if let Some(home) = &home {
        handoff::place_transcript(&bundle, home, &applied.cwd).map_err(|e| undo(fail("handoff_failed", e)))?;
        // The worktree is this repository's, which a backend may run in.
        let trusted = krowk_harness::trust::Store::new(Some(super::providers::krowk_dir().map_err(undo)?.join(krowk_harness::trust::FILE)), Some(ctx.env("HOME")).filter(|h| !h.is_empty()).map(Into::into));
        let _ = trusted.trust(&krowk_harness::trust::root(&applied.cwd));
    }
    // The log as the host left it, checkpoint and all.
    let events = read_log(&api).map_err(undo)?;
    handoff::restore_log(&sessions, &session, &events, &applied.cwd, &me, &handed.from_device).map_err(|e| undo(fail("handoff_failed", e)))?;
    // Its start, `moved_line`, is what the host daemon's watcher of a take
    // another device asked for reads as moved.
    let _ = writeln!(ctx.io.stderr, "{}, working in {} (branch {}); hosting it until interrupted", krowk_harness::sync::commands::moved_line(&session), applied.cwd.display(), applied.worktree.branch());
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

/// How many times `sync new` sends its start again after a transport
/// failure, the same id with the same bytes: the registry answers a repeat
/// 200, and the host runs an id once.
const SEND_RETRIES: u32 = 3;
/// How long `sync new` waits for the host's answer, asking every
/// `ANSWER_EVERY`: a host sees a command within one 10 s refresh.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const ANSWER_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
/// How long a started session has to reach the registry, which its host's
/// automatic sync takes it up to at its first turn.
const APPEAR_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// `krowk sync new --host <name> --cwd <dir> [--model <instance>/<id>]
/// "<prompt>"` (R-HOST-4): another machine of the person's asked to start a
/// session, then followed as `krowk sync attach` follows one. A host that
/// is offline, a directory it did not announce and a model it has no ready
/// instance for are refused here, from its announcement, before anything is
/// sent; the host checks again, and its own refusal is said the same way.
/// The permission mode is the host's (`--permission-mode` is refused).
pub(super) fn new_session(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    use krowk_client::host_command::{self, HostAnswer, HostCommand};
    let prompt = args.join(" ");
    let example = "e.g. krowk sync new --host desktop --cwd ~/code/app \"fix the tests\"";
    if prompt.trim().is_empty() {
        return Err(fail("bad_args", format!("`krowk sync new` takes the session's first prompt — {example}")));
    }
    let (asked, dir) = (ctx.f.host.trim().to_string(), ctx.f.cwd.trim().to_string());
    if asked.is_empty() {
        return Err(fail("bad_args", format!("`krowk sync new` needs --host, the machine to start the session on (`krowk hosts` lists yours) — {example}")));
    }
    if dir.is_empty() {
        return Err(fail("bad_args", format!("`krowk sync new` needs --cwd, the directory there to start it in (`krowk hosts` lists what each machine offers) — {example}")));
    }
    let here = std::env::current_dir().map_err(|e| fail("no_cwd", e.to_string()))?;
    let cwd = absolute(&dir, &ctx.env("HOME"), &here)?;
    current(ctx)?;
    let k = keys(ctx)?;
    let api = signed(ctx, &k, "krowk sync new")?;
    let host = find_host(&k.chain, &asked)?;
    let (to, name, now) = (host.id(), host.name.clone(), jiff::Timestamp::now());
    let presences = match api.list_presences() {
        Ok(p) => p,
        Err(e) if e.status == 404 => Vec::new(),
        Err(e) => return Err(e),
    };
    let announced = match presences.into_iter().find(|p| p.device.eq_ignore_ascii_case(&to.to_string())) {
        None => None,
        Some(p) => match krowk_client::presence::open(&p.sealed, &to, &k.user, &k.chain) {
            Ok(announcement) => Some(krowk_harness::sync::hosts::Announced { online: p.online, updated_at: p.updated_at, announcement }),
            // Offline is what matters then, whatever its last words were
            // sealed under.
            Err(_) if !p.online => return Err(offline(&name, &format!("last seen {}", ago(&p.updated_at, now)))),
            Err(e) => return Err(fail("host_unreadable", format!("{name}'s announcement does not open on this machine ({e}) — `krowk sync status` here, and on {name}, checks each one's keys"))),
        },
    };
    let model = startable(&name, announced.as_ref(), &cwd, &ctx.f.model, now)?;

    let id = command_id();
    let start = HostCommand::Start { cwd, model, prompt };
    let sealed = host_command::seal_command(&start, &id, &k.device, &to, &k.user, k.chain.generation()).map_err(|e| {
        // Only the two bounds say how long; anything else is the seal's own.
        let long = e.starts_with("the prompt is") || e.starts_with("the command seals to");
        fail(if long { "prompt_too_long" } else { "seal_failed" }, e)
    })?;
    let mut tries = 0;
    loop {
        match api.send_host_command(&to.to_string(), &id, &sealed) {
            Ok(_) => break,
            Err(e) if unreachable(&e) && tries < SEND_RETRIES => {
                tries += 1;
                std::thread::sleep(ANSWER_EVERY * tries);
            }
            // The last try may have landed with its answer lost.
            Err(e) if unreachable(&e) => return Err(fail(&e.code(), format!("the start could not be sent to {name} ({}) — it may still have reached it and start the session; krowk sync sessions will list it", e.code()))),
            Err(e) => return Err(e),
        }
    }
    let _ = writeln!(ctx.io.stderr, "krowk: asked {} to start the session — waiting for it (up to 10 s)…", super::sync::printable(&name));
    let deadline = std::time::Instant::now() + ANSWER_WAIT;
    let answer = loop {
        match api.host_command(&to.to_string(), &id) {
            Ok(s) if s.answered => {
                let Some(sealed) = s.answer else { return Err(fail("bad_answer", format!("{name} answered with nothing this krowk reads"))) };
                break host_command::open_answer(&sealed, &id, &to, &k.device, &k.user, &k.chain).map_err(|e| fail("bad_answer", e))?;
            }
            Ok(_) => {}
            Err(e) if unreachable(&e) => {}
            Err(e) => return Err(e),
        }
        if std::time::Instant::now() >= deadline {
            return Err(fail("host_silent", format!("{name} didn't answer within 30 s — it may still start the session; krowk sync sessions will list it")));
        }
        std::thread::sleep(ANSWER_EVERY);
    };
    let session = match answer {
        HostAnswer::Started { session_id } if krowk_harness::log::valid_id(&session_id) => session_id,
        HostAnswer::Started { session_id } => return Err(fail("bad_answer", format!("{name} started a session under {session_id:?}, which is not a session id"))),
        HostAnswer::Taken { .. } => return Err(fail("bad_answer", format!("{name} answered the start as a move"))),
        // The host's words, sealed under a key every device of the person's
        // holds: said as text, under a code only of its own shape.
        HostAnswer::Refused { code, message } => return Err(fail(refusal_code(&code), super::sync::printable(&message))),
    };
    // Its host's automatic sync takes it up to the registry at its first
    // turn: a second or two.
    let deadline = std::time::Instant::now() + APPEAR_WAIT;
    loop {
        match api.show_sync_session(&session) {
            Ok(_) => break,
            Err(e) if (e.status == 404 || unreachable(&e)) && std::time::Instant::now() < deadline => std::thread::sleep(ANSWER_EVERY),
            Err(e) if e.status == 404 => {
                return Err(fail("no_session", format!("{name} started session {session}, but it has not reached the registry within 10 s — `krowk sync attach {session}` follows it once it does")));
            }
            Err(e) => return Err(e),
        }
    }
    let _ = writeln!(ctx.io.stderr, "krowk: {} started session {session}; following it", super::sync::printable(&name));
    attach(ctx, &[session])
}

/// A host's refusal code as it is reported, which picks the exit code: one
/// of krowk's own shape, else `host_refused`.
fn refusal_code(code: &str) -> &str {
    let ours = !code.is_empty() && code.len() <= krowk_client::host_command::MAX_CODE && code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ours { code } else { "host_refused" }
}

/// The machine `asked` names on the device list: by its name, in any case,
/// or by 8 or more hex characters its device id starts with. The recovery
/// kit is no machine.
fn find_host<'a>(chain: &'a Chain, asked: &str) -> Result<&'a krowk_client::device_chain::Device, Error> {
    let lower = asked.to_lowercase();
    let by_id = lower.len() >= 8 && lower.chars().all(|c| c.is_ascii_hexdigit());
    let found: Vec<_> = chain.devices().iter().filter(|d| d.kind == krowk_client::device_chain::Kind::Device).filter(|d| d.name.to_lowercase() == lower || (by_id && d.id().to_string().starts_with(&lower))).collect();
    match found[..] {
        [one] => Ok(one),
        [] => Err(fail("no_host", format!("no host named '{}' — krowk hosts lists yours", super::sync::printable(asked)))),
        _ => {
            let named: Vec<String> = found.iter().map(|d| format!("{} ({})", super::sync::printable(&d.name), &d.id().to_string()[..12])).collect();
            Err(fail("ambiguous_host", format!("'{}' names more than one machine: {} — pass --host with 8 or more characters of its id", super::sync::printable(asked), named.join(", "))))
        }
    }
}

/// Whether `name`, as it `announced` itself, can start a session in `cwd`
/// on `model` (`<instance>/<id>`, or empty for its first ready instance's
/// first model): online, the directory one it offers, the instance there
/// and ready. The model it would run on, or the refusal.
fn startable(name: &str, announced: Option<&krowk_harness::sync::hosts::Announced>, cwd: &str, model: &str, now: jiff::Timestamp) -> Result<krowk_harness::protocol::ModelRef, Error> {
    use krowk_harness::protocol::ModelRef;
    let shown = super::sync::printable(name);
    let a = match announced {
        Some(a) if a.online => &a.announcement,
        _ => return Err(offline(name, &announced.map_or_else(|| "not announced".to_string(), |a| format!("last seen {}", ago(&a.updated_at, now))))),
    };
    if !a.directories.iter().any(|d| d == cwd) {
        let offers = if a.directories.is_empty() { "none yet — run krowk in a repository there once and trust it".to_string() } else { a.directories.join(", ") };
        return Err(fail("directory_not_offered", super::sync::printable(&format!("{shown} does not offer {cwd}; it offers: {offers}"))));
    }
    let model = model.trim();
    if model.is_empty() {
        let ready: Vec<_> = a.instances.iter().filter(|i| i.ready).collect();
        if let Some((i, m)) = ready.iter().find_map(|i| i.models.first().map(|m| (i, m))) {
            return Ok(ModelRef { instance: i.name.clone(), model: m.clone() });
        }
        if !ready.is_empty() {
            return Err(fail("no_ready_model", format!("{shown} announces no model for its ready instances — name one with --model <instance>/<id>")));
        }
        let fixes: Vec<String> = a.instances.iter().map(|i| format!("{} — {}", i.name, i.fix.clone().unwrap_or_else(|| i.state.clone()))).collect();
        let fixes = if fixes.is_empty() { "it has no instance — connect one there with krowk connect".to_string() } else { fixes.join("; ") };
        return Err(fail("no_ready_model", super::sync::printable(&format!("{shown} has no ready model: {fixes}"))));
    }
    let Some((instance, id)) = model.split_once('/').filter(|(i, m)| !i.is_empty() && !m.is_empty()) else {
        return Err(fail("bad_model", format!("--model takes <instance>/<id>, e.g. anthropic/claude-opus-5-5 — `krowk hosts` lists {shown}'s instances")));
    };
    let fix = match a.instances.iter().find(|i| i.name == instance) {
        Some(i) if i.ready => return Ok(ModelRef { instance: instance.into(), model: id.into() }),
        Some(i) => i.fix.clone().unwrap_or_else(|| format!("{instance} is not ready ({})", i.state)),
        None => {
            let has: Vec<&str> = a.instances.iter().map(|i| i.name.as_str()).collect();
            let has = if has.is_empty() { "none".to_string() } else { has.join(", ") };
            format!("it has no instance {instance} — its instances: {has}")
        }
    };
    Err(fail("model_not_ready", super::sync::printable(&format!("{shown} can't run {model}: {fix}"))))
}

/// The refusal of a host that is offline, `seen` saying since when.
fn offline(name: &str, seen: &str) -> Error {
    fail("host_offline", format!("{} is offline ({seen}): start krowk there, or krowk host enable", super::sync::printable(name)))
}

/// `dir` as the host is asked for it: `~` expanded to `home`, made absolute
/// from `here`, and `.` and trailing slashes dropped — never its links
/// resolved, since its paths are the host's own.
fn absolute(dir: &str, home: &str, here: &std::path::Path) -> Result<String, Error> {
    use std::path::{Path, PathBuf};
    let rest = if dir == "~" { Some("") } else { dir.strip_prefix("~/") };
    let path = match rest {
        Some(_) if home.is_empty() => return Err(fail("no_home", format!("--cwd {dir} starts with ~, and HOME is not set — name the directory in full"))),
        Some(rest) => Path::new(home).join(rest),
        None => PathBuf::from(dir),
    };
    let path = if path.is_absolute() { path } else { here.join(path) };
    Ok(path.components().collect::<PathBuf>().display().to_string())
}

/// A fresh command id: a random UUID, version 4.
fn command_id() -> String {
    let mut b = krowk_client::e2e::random::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = krowk_client::e2e::hex(&b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
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

    /// R-HOST-3: `krowk hosts` shows an announced host's state, its ready
    /// instances with their first model, the others with their fix, and its
    /// directories with the home as `~`.
    #[test]
    fn r_host_3_an_announced_host_is_shown_in_a_few_lines() {
        use krowk_client::presence::{AnnouncedInstance, Announcement};
        let inst = |name: &str, ready: bool, models: &[&str]| AnnouncedInstance { name: name.into(), kind: "k".into(), ready, state: if ready { "ready" } else { "key_not_set" }.into(), fix: (!ready).then(|| "set OPENAI_API_KEY".into()), models: models.iter().map(|m| m.to_string()).collect(), limit: None };
        let announcement = Announcement { device: "d".into(), name: "a".into(), os: "linux".into(), version: "0.31.0".into(), online_since_ms: 0, instances: vec![inst("anthropic", true, &["claude-sonnet-4-5", "claude-opus-4-1"]), inst("codex", true, &[]), inst("openai", false, &[])], directories: vec!["/home/ann/code/krowk".into(), "/Users/ann".into(), "/srv/x".into(), "/root/y".into()], sessions: Vec::new() };
        let mut a = krowk_harness::sync::hosts::Announced { online: true, updated_at: "2026-10-25T10:00:00Z".into(), announcement };
        let now: jiff::Timestamp = "2026-10-25T10:12:30Z".parse().unwrap();
        assert_eq!(super::announced_lines(&a, now), ["online · krowk 0.31.0", "ready: anthropic (claude-sonnet-4-5, …), codex", "not ready: openai — set OPENAI_API_KEY", "directories: ~/code/krowk, ~, /srv/x, …"]);
        a.online = false;
        assert_eq!(super::announced_lines(&a, now)[0], "offline · last seen 12m ago");
        assert_eq!(super::home_as_tilde("/root/y"), "~/y");
        assert_eq!(super::home_as_tilde("/home/"), "/home/");
        assert_eq!(super::ago("not a time", now), "a while ago");
    }

    /// R-HOST-4, R-SWITCH-4: `krowk sync new` refuses, from the host's
    /// announcement, a host that is offline, a directory it did not offer
    /// and a model it has no ready instance for, each with its fix; with no
    /// `--model` it takes the first ready instance's first model.
    #[test]
    fn r_host_4_sync_new_checks_the_announcement_before_it_sends() {
        use krowk_client::presence::{AnnouncedInstance, Announcement};
        let inst = |name: &str, ready: bool, models: &[&str]| AnnouncedInstance { name: name.into(), kind: "k".into(), ready, state: if ready { "ready" } else { "keyNotSet" }.into(), fix: (!ready).then(|| format!("set the key of {name}")), models: models.iter().map(|m| m.to_string()).collect(), limit: None };
        let announcement = Announcement { device: "d".into(), name: "desk".into(), os: "linux".into(), version: "0.31.0".into(), online_since_ms: 0, instances: vec![inst("openai", false, &["gpt-5"]), inst("codex", true, &[]), inst("anthropic", true, &["claude-opus-5-5", "claude-sonnet-5-5"])], directories: vec!["/home/ann/app".into(), "/srv/x".into()], sessions: Vec::new() };
        let mut a = krowk_harness::sync::hosts::Announced { online: true, updated_at: "2026-10-25T10:00:00Z".into(), announcement };
        let now: jiff::Timestamp = "2026-10-25T10:12:30Z".parse().unwrap();
        let refused = |a: Option<&krowk_harness::sync::hosts::Announced>, cwd: &str, model: &str| {
            let e = super::startable("desk", a, cwd, model, now).unwrap_err();
            (e.code(), e.fix())
        };
        let ok = super::startable("desk", Some(&a), "/home/ann/app", "", now).unwrap();
        assert_eq!(ok.to_string(), "anthropic/claude-opus-5-5", "the first ready instance with a model");
        assert_eq!(super::startable("desk", Some(&a), "/srv/x", "anthropic/claude-sonnet-5-5", now).unwrap().to_string(), "anthropic/claude-sonnet-5-5");
        assert_eq!(refused(Some(&a), "/home/ann/other", ""), ("directory_not_offered".into(), "desk does not offer /home/ann/other; it offers: /home/ann/app, /srv/x".into()));
        assert_eq!(refused(Some(&a), "/srv/x", "openai/gpt-5"), ("model_not_ready".into(), "desk can't run openai/gpt-5: set the key of openai".into()));
        assert_eq!(refused(Some(&a), "/srv/x", "xai/grok-4").1, "desk can't run xai/grok-4: it has no instance xai — its instances: openai, codex, anthropic");
        assert_eq!(refused(Some(&a), "/srv/x", "claude-opus-5-5").0, "bad_model");
        a.announcement.instances.retain(|i| !i.ready);
        assert_eq!(refused(Some(&a), "/srv/x", ""), ("no_ready_model".into(), "desk has no ready model: openai — set the key of openai".into()));
        a.online = false;
        assert_eq!(refused(Some(&a), "/srv/x", ""), ("host_offline".into(), "desk is offline (last seen 12m ago): start krowk there, or krowk host enable".into()));
        assert_eq!(refused(None, "/srv/x", "").1, "desk is offline (not announced): start krowk there, or krowk host enable");
    }

    /// R-HOST-4: `--host` names a machine on the list by its name in any
    /// case, or by 8+ hex characters of its id; none is said. (The list
    /// itself refuses two names alike.)
    #[test]
    fn r_host_4_sync_new_finds_its_host_by_name_or_id() {
        use krowk_client::device_chain::{Chain, Change, Kind, Subject};
        use krowk_client::e2e::{DeviceKey, SigningKey};
        let subject = |name: &str, kind: Kind| {
            let (d, s) = (DeviceKey::generate(), SigningKey::generate());
            (Subject { kind, name: name.into(), os: "linux".into(), device: d.public(), signing: s.public() }, s)
        };
        let ((desk, signing), (kit, kit_key)) = (subject("Desk", Kind::Device), subject("recovery", Kind::Recovery));
        let (twin, laptop) = (subject("twin", Kind::Device).0, subject("laptop", Kind::Device).0);
        let (chain, start) = Chain::start(desk.clone(), &signing, Some((kit.clone(), &kit_key)), 1).unwrap();
        let (chain, _) = chain.batch(&start.newest, vec![Change::Add(twin.clone()), Change::Add(laptop)], desk.id(), &signing, 2).unwrap();
        let found = |asked: &str| super::find_host(&chain, asked).map(|d| d.id()).map_err(|e| e.code());
        assert_eq!(found("desk"), Ok(desk.id()));
        assert_eq!(found(&desk.id().to_string()[..8].to_uppercase()), Ok(desk.id()));
        assert_eq!(found(&twin.id().to_string()[..10]), Ok(twin.id()));
        assert_eq!(found(&desk.id().to_string()[..7]), Err("no_host".into()), "fewer than 8 characters is a name");
        assert_eq!(found("recovery"), Err("no_host".into()), "the kit is no machine");
        assert_eq!(found(&kit.id().to_string()[..8]), Err("no_host".into()));
        assert_eq!(found("TWIN"), Ok(twin.id()));
        assert_eq!(super::find_host(&chain, "nope").unwrap_err().fix(), "no host named 'nope' — krowk hosts lists yours");
    }

    /// `--cwd` is the host's path: `~` expanded and made absolute, never
    /// resolved. A command id is a version 4 UUID, and a host's refusal code
    /// is reported only in krowk's own shape.
    #[test]
    fn r_host_4_sync_new_asks_for_an_absolute_path_under_a_fresh_uuid() {
        let here = std::path::Path::new("/work/here");
        assert_eq!(super::absolute("~/app/", "/home/ann", here).unwrap(), "/home/ann/app");
        assert_eq!(super::absolute("~", "/home/ann", here).unwrap(), "/home/ann");
        assert_eq!(super::absolute("app/./x", "/home/ann", here).unwrap(), "/work/here/app/x");
        assert_eq!(super::absolute("/srv/../x", "", here).unwrap(), "/srv/../x");
        assert_eq!(super::absolute("~/x", "", here).unwrap_err().code(), "no_home");
        let id = super::command_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12], "{id}");
        assert!(parts[2].starts_with('4') && "89ab".contains(&parts[3][..1]) && id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()), "{id}");
        assert_ne!(id, super::command_id());
        assert_eq!(super::refusal_code("directory_not_offered"), "directory_not_offered");
        for odd in ["", "Bad", "x\u{1b}[2J", &"x".repeat(65)] {
            assert_eq!(super::refusal_code(odd), "host_refused", "{odd:?}");
        }
    }

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
