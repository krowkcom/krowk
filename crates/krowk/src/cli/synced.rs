//! `krowk sync sessions`, `krowk sync host` and `krowk sync attach`: a
//! session running on one machine, watched and steered from another
//! (engineering/harness.md → Sync). The bridge and the viewer are
//! `krowk_harness::sync`; this is the command line around them.
//!
//! - `sessions`: the synced sessions this machine can open, from their
//!   sealed indexes.
//! - `host <session>`: this machine's daemon runs the session, and the
//!   bridge holds its lease and syncs it until interrupted.
//! - `attach <session>`: follows it from another machine as stream-json on
//!   stdout; each line typed on stdin is a prompt to it, queued while no
//!   host is online.
//!
//! The relay is `KROWK_RELAY_URL`, else the reference relay on this
//! machine (`krowk relay serve`), until the hosted relay has a published
//! address.

use super::sync::{keyed_client, keystore};
use super::Ctx;
use krowk_api::{fail, Error};
use krowk_client::e2e::{AccountKey, DeviceId, SigningKey};
use krowk_harness::sync::{host, viewer};
use serde_json::json;
use std::sync::Arc;

struct Keys {
    device: DeviceId,
    signing: SigningKey,
    account: AccountKey,
}

fn keys(ctx: &Ctx) -> Result<Keys, Error> {
    let ks = keystore(ctx)?;
    let device = ks.device().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(|| fail("not_set_up", "this machine has no sync keys — run `krowk sync init`, `recover` or `join` first"))?.id();
    let account = ks.account().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(|| fail("not_set_up", "this machine holds no account key — run `krowk sync join` or `recover` first"))?;
    let signing = ks.signing_key().map_err(|e| fail("keys_unreadable", e))?;
    Ok(Keys { device, signing, account })
}

fn relay(ctx: &Ctx) -> String {
    let r = ctx.env("KROWK_RELAY_URL");
    if r.trim().is_empty() { format!("ws://{}", super::relay::DEFAULT_ADDR) } else { r.trim().to_string() }
}

fn one(args: &[String], what: &str) -> Result<String, Error> {
    match args {
        [s] => Ok(s.clone()),
        _ => Err(fail("bad_args", format!("`krowk sync {what}` takes one session id"))),
    }
}

pub(super) fn sessions(ctx: &mut Ctx) -> Result<(), Error> {
    let k = keys(ctx)?;
    let api = keyed_client(ctx, "krowk sync sessions")?;
    let (listed, unreadable) = viewer::list(&api, &k.account).map_err(|e| fail("sync_failed", e))?;
    let rows: Vec<_> = listed.iter().map(|s| json!({"id": s.id, "title": s.index.title, "cwd": s.index.cwd, "updatedMs": s.index.updated_ms, "host": s.holder})).collect();
    if ctx.format == crate::output::Format::Json {
        return ctx.emit(&json!({"sessions": rows, "unreadable": unreadable}).to_string());
    }
    for s in &listed {
        let title = super::sync::printable(&s.index.title);
        let _ = writeln!(ctx.io.stdout, "{}  {}  {}", s.id, if s.holder.is_some() { "hosted" } else { "offline" }, title);
    }
    if unreadable > 0 {
        let _ = writeln!(ctx.io.stdout, "{unreadable} synced session(s) do not open with this machine's account key");
    }
    Ok(())
}

pub(super) fn host_session(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let session = one(args, "host")?;
    let k = keys(ctx)?;
    let api = Arc::new(keyed_client(ctx, "krowk sync host")?);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let cwd = std::env::current_dir().map_err(|e| fail("no_cwd", e.to_string()))?;
    let spawn = super::host::spawner(ctx)?;
    let o = host::Options { relay: relay(ctx), env, api, device: k.device, signing: k.signing, account: k.account, session, title: String::new(), cwd: cwd.display().to_string(), ttl: host::LEASE_TTL };
    krowk_harness::sync::run_host(o, ctx.io.env, &cwd, super::VERSION, &spawn).map_err(|(code, message)| fail(&code, message))
}

pub(super) fn attach(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let session = one(args, "attach")?;
    let k = keys(ctx)?;
    let api = Arc::new(keyed_client(ctx, "krowk sync attach")?);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env).to_string();
    let o = viewer::Options { relay: relay(ctx), env, api, device: k.device, signing: k.signing, account: k.account, session: session.clone(), known: None };
    krowk_harness::sync::run_attach(o, &mut *ctx.io.stdout).map_err(|e| fail("sync_failed", e))
}
