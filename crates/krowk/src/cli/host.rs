//! `krowk host`: the per-user daemon sessions run in, so a session outlives
//! the terminal that started it (R-HOST-1). The daemon itself, its socket
//! and its protocol are `krowk_harness::daemon`; this is the command line
//! around them.
//!
//! - `serve` (not listed): the daemon. The first `krowk` that needs it
//!   (`krowk -p --daemon`) starts it detached; a service runs it too.
//! - `status`: whether it runs, and what it holds.
//! - `attach <session>`: follows a session in it as stream-json — the log,
//!   the turn so far, then live — until the running turn ends.
//! - `enable` / `disable`: the systemd user unit or launchd agent that
//!   keeps it running on an always-on machine (R-HOST-2).

use super::{prompt, Ctx};
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::daemon::{self, server, service};
use krowk_harness::engine::EngineError;
use krowk_harness::host::HostConfig;
use krowk_harness::instances::Registry;
use krowk_harness::protocol::HostStatus;
use krowk_harness::{log, trust};
use serde_json::json;
use std::sync::Arc;

/// Where the daemon's stdout and stderr go: `host.log` in krowk's home.
pub(super) fn log_path(ctx: &Ctx) -> Result<std::path::PathBuf, Error> {
    Ok(krowk_api::home::dir(ctx.io.env)?.join("host.log"))
}

/// Starts the daemon, detached, from this very binary.
pub(super) fn spawner(ctx: &Ctx) -> Result<impl Fn() -> Result<(), String> + use<>, Error> {
    let exe = std::env::current_exe().map_err(|e| fail("host_unavailable", format!("krowk cannot find its own binary to start the host daemon: {e}")))?;
    let log = log_path(ctx)?;
    Ok(move || daemon::spawn_detached(&exe, &["host", "serve"], &log))
}

fn engine(e: EngineError) -> Error {
    prompt::engine_error(&e.code, &e.message, e.status)
}

/// The daemon: every working directory's host made the way `krowk -p`
/// makes its own, save what only a person at a terminal could answer. A
/// repository is trusted for a backend only when it is on the trusted
/// list — a daemon asks nobody — and approval requests go to the clients
/// following the session, which is the TUI's to answer.
pub(super) fn serve(ctx: &mut Ctx) -> Result<(), Error> {
    let config = prompt::config_json()?;
    let socket = daemon::socket(ctx.io.env).map_err(|e| fail("host_unavailable", e))?;
    let idle = daemon::idle_window(ctx.io.env, Some(&config)).map_err(|e| fail("bad_config", e))?;
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let store = trust::Store::new(Some(super::providers::krowk_dir()?.join(trust::FILE)), home);
    let trusted_store = store.clone();
    let permissions = prompt::permissions_config(ctx, &config, Arc::new(move |root: &std::path::Path| trusted_store.trusts(root)), true);
    let gate: trust::Gate = Arc::new(move |root: &std::path::Path| {
        if store.trusts(root) {
            return Ok(());
        }
        if let Some(why) = store.refuses(root) {
            return Err(trust::untrusted(root, &format!("It cannot be trusted for good — {why}. Run `krowk -p --trust` there, in this process rather than the daemon, for one run.")));
        }
        Err(trust::untrusted(root, "The host daemon asks nobody: run krowk there once on a terminal and answer its trust prompt, then send the prompt again."))
    });
    let publisher = prompt::publisher(ctx);
    let sessions_dir = log::sessions_dir(ctx.io.env)?;
    let credentials = super::providers::credentials_path()?;
    let env = krowk_api::home::process_env;
    let factory: server::Factory = Box::new(move |cwd: &std::path::Path| {
        // Read again for each directory's host: a provider connected since
        // the daemon started is one its next directory has.
        let config = prompt::config_json().map_err(|e| EngineError::new("bad_config", e.fix()))?;
        let instances = prompt::instances_from(&config).map_err(|e| EngineError::new("bad_config", e.fix()))?;
        Ok(HostConfig {
            sessions_dir: sessions_dir.clone(),
            cwd: cwd.to_path_buf(),
            registry: Registry::resolve(&instances, &env),
            krowk_version: super::VERSION.into(),
            pricer: prompt::pricer(&env),
            catalog: prompt::catalog(&env),
            credentials: credentials.clone(),
            trust: gate.clone(),
            publisher: Some(publisher.clone()),
            permissions: permissions.clone(),
            agents: prompt::agents_config(&env),
        })
    });
    server::run(server::Options { socket, idle, krowk_version: super::VERSION.into() }, factory).map_err(|e| fail("host_failed", e))
}

pub(super) fn status(ctx: &mut Ctx) -> Result<(), Error> {
    let socket = daemon::socket(ctx.io.env).map_err(|e| fail("host_unavailable", e))?;
    let service = service::path(service::Platform::here(), ctx.io.env).ok().filter(|p| p.is_file());
    let status: Option<HostStatus> = daemon::status(ctx.io.env, super::VERSION).map_err(engine)?;
    let data = match &status {
        Some(s) => json!({ "running": true, "socket": s.socket, "pid": s.pid, "version": s.krowk_version, "uptimeMs": s.uptime_ms, "clients": s.clients.saturating_sub(1), "idleExitMs": s.idle_exit_ms, "sessions": s.sessions, "service": service }),
        None => json!({ "running": false, "socket": socket, "service": service }),
    };
    if ctx.format != Format::Human {
        let summary = if status.is_some() { "the host daemon is running" } else { "no host daemon is running" };
        return super::sessions::emit_data(ctx, data, summary.into());
    }
    let out = &mut *ctx.io.stdout;
    match &status {
        None => {
            let _ = writeln!(out, "not running — the next `krowk -p --daemon` starts it");
            let _ = writeln!(out, "socket   {}", socket.display());
        }
        Some(s) => {
            let running = s.sessions.iter().filter(|x| x.running).count();
            let _ = writeln!(out, "running  pid {}, krowk {}, up {}", s.pid, s.krowk_version, uptime(s.uptime_ms));
            let _ = writeln!(out, "socket   {}", s.socket);
            let _ = writeln!(out, "sessions {} ({running} running), {} other client{}", s.sessions.len(), s.clients.saturating_sub(1), if s.clients == 2 { "" } else { "s" });
            let idle = s.idle_exit_ms.map_or("never — run as a service".into(), |ms| format!("after {} idle", uptime(ms)));
            let _ = writeln!(out, "exits    {idle}");
            for x in &s.sessions {
                let _ = writeln!(out, "  {}{}", x.session_id, if x.running { "  running" } else { "" });
            }
        }
    }
    if let Some(p) = &service {
        let _ = writeln!(out, "service  {}", p.display());
    }
    Ok(())
}

fn uptime(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// Every frame of the session as stream-json, until its running turn ends.
pub(super) fn attach(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let [session] = args else {
        return Err(fail("bad_args", "`krowk host attach` takes one session id — the sessionId a result names"));
    };
    daemon::attach(ctx.io.env, super::VERSION, session, ctx.io.stdout).map_err(engine)
}

/// `enable`: the service file written, then the service manager told to
/// start it now and at every login. `disable`: stopped, then removed.
pub(super) fn enable(ctx: &mut Ctx, on: bool) -> Result<(), Error> {
    let platform = service::Platform::here();
    let file = service::path(platform, ctx.io.env).map_err(|e| fail("no_home", e))?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if on {
        let exe = std::env::current_exe().map_err(|e| fail("host_unavailable", format!("krowk cannot find its own binary: {e}")))?;
        let text = service::render(platform, &exe, &log_path(ctx)?, ctx.io.env);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| fail("write_failed", format!("{} could not be made: {e}", dir.display())))?;
        }
        std::fs::write(&file, text).map_err(|e| fail("write_failed", format!("{} could not be written: {e}", file.display())))?;
    }
    for c in service::commands(platform, on, &file, uid) {
        let ran = std::process::Command::new(&c[0]).args(&c[1..]).stdin(std::process::Stdio::null()).output();
        match ran {
            Ok(o) if o.status.success() => {}
            // Stopping one that is not loaded is no failure: it is stopped.
            Ok(_) | Err(_) if !on => {}
            Ok(o) => return Err(fail("service_failed", format!("`{}` failed: {} — {} is written; fix that and run `krowk host enable` again", c.join(" "), String::from_utf8_lossy(&o.stderr).trim(), file.display()))),
            Err(e) => return Err(fail("service_failed", format!("`{}` could not run: {e} — {} is written; start it with your service manager", c.join(" "), file.display()))),
        }
    }
    if !on {
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(fail("write_failed", format!("{} could not be removed: {e}", file.display()))),
            _ => {}
        }
    }
    let data = json!({ "enabled": on, "service": file });
    let summary = if on { format!("the host daemon runs as a service: {}", file.display()) } else { format!("the host service is stopped and {} removed", file.display()) };
    if ctx.format != Format::Human {
        return super::sessions::emit_data(ctx, data, summary);
    }
    let _ = writeln!(ctx.io.stdout, "{summary}");
    Ok(())
}
