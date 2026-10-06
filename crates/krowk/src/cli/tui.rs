//! Bare `krowk` on a terminal: the inline TUI (R-PKG-1). The TUI itself is
//! `krowk-tui`; this is the command line around it — when it opens, the
//! flags it takes (`--model`, `--permission-mode`, `--toolset`, `--effort`, `--resume [id]`), the
//! config it reads, and the session it leaves in krowk.db.

use super::flags::Flags;
use super::{prompt, sessions, Ctx, Io};
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{self, Registry};
use krowk_harness::log;
use std::sync::Arc;

/// Whether this invocation opens the TUI: nothing asked but `krowk` itself
/// (and its TUI flags), the human format, and a person at both ends of the
/// terminal — stdin to type into and stdout to draw on. A dumb terminal
/// cannot draw it. Anything else keeps what bare `krowk` always did.
pub(super) fn wanted(io: &Io, f: &Flags, format: Format, positionals: &[String], jq_given: bool) -> bool {
    positionals.is_empty()
        && !f.help
        && !f.version
        && !f.print
        && !f.quiet
        && !jq_given
        && format == Format::Human
        && io.tty
        && io.stdin_tty
        && (io.env)("TERM") != "dumb"
}

pub(super) fn run(ctx: &mut Ctx) -> Result<(), Error> {
    run_with(ctx, None)
}

/// The TUI on a session another machine runs, followed through sync
/// (`krowk sync attach`, `krowk --resume` of a synced id): drawn as any
/// session is, its prompts, approvals and interrupts sent to its host.
#[cfg(unix)]
pub(super) fn run_synced(ctx: &mut Ctx, o: krowk_tui::synced::Options) -> Result<(), Error> {
    run_with(ctx, Some(o))
}

fn run_with(ctx: &mut Ctx, sync: Option<krowk_tui::synced::Options>) -> Result<(), Error> {
    sessions::check_os()?;
    let flag_mode = prompt::permission_flag(ctx)?;
    let config = prompt::config_json()?;
    let registry = Registry::resolve(&prompt::instances_from(&config)?, ctx.io.env);
    registry.check_rollover().map_err(|e| fail("bad_config", e))?;
    let asked = prompt::model_flag(ctx, &registry)?;
    let sessions_dir = log::sessions_dir(ctx.io.env)?;
    // A synced session is not one of this machine's: nothing to resume or
    // route here, and no log to project — the host runs it as it is.
    let synced = sync.is_some();
    let resume = if synced {
        None
    } else if ctx.f.resume_pick {
        Some(pick(ctx)?)
    } else {
        match ctx.f.resume.as_str() {
            "" => None,
            r => Some(prompt::resolve_resume(ctx, &sessions_dir, r)?),
        }
    };
    let (settings, notices) = krowk_tui::settings::from_config(&config);
    let cwd = std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?;
    // Beside krowk.db and the session logs, so it goes where they go.
    let history_file = Some(sessions_dir.join("tui-history.jsonl"));
    let toolset = prompt::toolset_flag(ctx)?;
    let effort = prompt::effort_flag(ctx)?;
    let budget = prompt::budget_flag(ctx)?;
    // R-BACK-6: asked now, while the terminal is still the person's — the
    // TUI takes it raw from here on. A resumed session runs on its last
    // model and in the directory it started in.
    let past = resume.as_ref().and_then(|id| log::read_events(&sessions_dir.join(id).join(log::EVENTS_FILE)).ok()).unwrap_or_default();
    let session_model = past.iter().rev().find_map(|e| match &e.body {
        // On the name its instance has now, renamed or not.
        krowk_harness::protocol::LogBody::TurnStarted { model, .. } => Some(registry.current_model(model)),
        _ => None,
    });
    let session_cwd = past.first().and_then(|e| match &e.body {
        krowk_harness::protocol::LogBody::SessionStarted { cwd, .. } => Some(std::path::PathBuf::from(cwd)),
        _ => None,
    });
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let runs_in = session_cwd.clone().unwrap_or_else(|| cwd.clone());
    // A model that needs no vendor asked is known now: `--model` naming
    // its instance, the session's own, a `defaultModel` naming its
    // instance. Any other — a bare `--model`, or none at all — the TUI
    // routes once its first frame is up (R-PERF-1: nothing before the
    // prompt waits on a vendor's status check), and asks the trust question
    // itself when the route lands on a backend.
    let exact_default = registry.default_model.as_deref().and_then(|d| match registry.read_model(d) {
        Ok(instances::Asked::Exact(m)) => Some(m),
        _ => None,
    });
    let chosen = match &asked {
        Some(instances::Asked::Exact(m)) => Some(m.clone()),
        _ => None,
    };
    let (model, route) = match asked {
        _ if synced => (None, None),
        Some(instances::Asked::Exact(m)) => (Some(m), None),
        Some(bare) => (None, Some(krowk_tui::Route { asked: Some(bare), current: session_model.clone() })),
        None if session_model.is_some() => (None, None),
        None => match exact_default {
            Some(m) => (Some(m), None),
            None => (None, Some(krowk_tui::Route { asked: None, current: None })),
        },
    };
    let effective = model.clone().or(session_model);
    // What a repository's own settings would widen is asked about with the
    // trust question too; the TUI answers approvals itself (R-PERM-2).
    let probe = prompt::permissions_config(ctx, &config, Arc::new(|_: &std::path::Path| false), true);
    // Every settings file is read — trusted or not, each rule parsed — before
    // the trust question is asked and saved: one that does not load is named
    // now, not after the answer was kept.
    krowk_harness::permissions::settings::load(&probe, &runs_in).map_err(prompt::bad_settings)?;
    let widens = krowk_harness::permissions::settings::widens(&probe, &runs_in);
    let (trust, trusted, trust_ask) = prompt::tui_trust_gate(effective.as_ref(), &registry, &runs_in, super::providers::krowk_dir()?, home, widens);
    let permissions = prompt::permissions_config(ctx, &config, trusted, true);
    let (permission_mode, mode_notices) = prompt::resolve_mode(flag_mode, &permissions, &runs_in)?;
    let host = HostConfig {
        sessions_dir,
        cwd,
        registry,
        krowk_version: super::VERSION.into(),
        pricer: prompt::pricer(ctx.io.env),
        catalog: prompt::catalog(ctx.io.env),
        credentials: super::providers::credentials_path()?,
        trust,
        publisher: Some(prompt::publisher(ctx)),
        permissions,
        agents: prompt::agents_config(ctx.io.env),
    };
    let projector = Projector::default();
    let outcome = krowk_tui::run(krowk_tui::Options {
        host,
        resume,
        model,
        chosen,
        route,
        trust: (!synced).then_some(trust_ask),
        permission_mode,
        toolset,
        effort,
        budget,
        settings,
        history_file,
        notices: notices.into_iter().chain(mode_notices).collect(),
        no_recovery_kit: no_recovery_kit(ctx),
        // A synced session prints no header to say it under.
        update: if synced {
            None
        } else {
            super::upgrade::for_tui(ctx).map(|n| krowk_tui::app::Update { current: super::VERSION.into(), latest: n.latest, security: n.security, due: n.due })
        },
        version: super::VERSION.into(),
        config: Some(super::providers::config_path()?),
        daemon: if synced { None } else { daemon(ctx)? },
        project: (!synced).then(|| projector.after_turns()),
        sync,
    });
    // As after `krowk -p`: the log is the session, krowk.db its listing —
    // each session `/sessions` moved away from, and the one shown last.
    // Projected after each turn already, so only a log that has grown
    // since is read again on the way out.
    for id in outcome.left.iter().chain(&outcome.session_id).filter(|_| !synced) {
        if let Err(e) = projector.project(ctx.io.env, id) {
            ctx.warn(&format!("session {id} is saved, but krowk.db was not updated: {} — `krowk sessions sync` retries", e.fix()));
        }
    }
    // Left without waiting for a turn (a second Ctrl-C, SIGTERM or SIGHUP):
    // recorded above, and exits the way an interrupted command does.
    if outcome.abandoned {
        let _ = ctx.io.stdout.flush();
        std::process::exit(130);
    }
    match outcome.error {
        Some(e) => Err(fail("tui_failed", e)),
        None => Ok(()),
    }
}

/// Where the TUI's sessions run: the host daemon, started when none runs,
/// so a session outlives the terminal (R-HOST-1) — unless
/// `KROWK_TUI_HOST=local` keeps them in this process.
#[cfg(unix)]
fn daemon(ctx: &Ctx) -> Result<Option<krowk_tui::Daemon>, Error> {
    if ctx.env("KROWK_TUI_HOST") == "local" {
        return Ok(None);
    }
    let spawn = super::host::spawner(ctx)?;
    Ok(Some(krowk_tui::Daemon { env: Box::new(krowk_api::home::process_env), version: super::VERSION.into(), spawn: Box::new(spawn) }))
}

#[cfg(not(unix))]
fn daemon(_: &Ctx) -> Result<Option<krowk_tui::Daemon>, Error> {
    Ok(None)
}

/// Projects sessions into krowk.db, remembering how long each log was when
/// it last was, so a log that has not grown since is not read again.
#[derive(Clone, Default)]
struct Projector {
    done: Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
}

impl Projector {
    /// Projects `id` unless its log has not grown since it last was. The
    /// length is taken before the log is read: what is
    /// appended while it is read is read again next time, never missed.
    fn project(&self, env: krowk_import::Env, id: &str) -> Result<(), Error> {
        let len = log::sessions_dir(env).ok().and_then(|d| std::fs::metadata(d.join(id).join(log::EVENTS_FILE)).ok()).map(|m| m.len());
        let done = || self.done.lock().unwrap_or_else(|e| e.into_inner());
        if len.is_some() && done().get(id) == len.as_ref() {
            return Ok(());
        }
        sessions::project_native(env, id)?;
        if let Some(len) = len {
            done().insert(id.to_string(), len);
        }
        Ok(())
    }

    /// For the TUI to run after each turn, on its own thread. That thread
    /// outlives `ctx`, so it reads the process's environment, which is what
    /// `ctx.io.env` is whenever the TUI runs (only `main` opens it). A
    /// failure is left for the projection on the way out to report.
    fn after_turns(&self) -> krowk_tui::Project {
        let me = self.clone();
        Arc::new(move |id: &str| {
            let _ = me.project(&|k| std::env::var(k).unwrap_or_default(), id);
        })
    }
}

/// `krowk --resume`: the sessions picker, over krowk's own sessions.
fn pick(ctx: &Ctx) -> Result<String, Error> {
    let conn = sessions::open_store(ctx)?;
    let rows = krowk_store::list_sessions(&conn, krowk_harness::project::HARNESS, "", sessions::DEFAULT_SESSION_LIMIT as i64)
        .map_err(|e| sessions::store_fail(&e, &sessions::db_path_string(ctx)))?;
    if rows.is_empty() {
        return Err(fail("no_session", "there is no krowk session to resume yet — run `krowk` to start one"));
    }
    let id = sessions::pick_session(&rows, krowk_store::now_ms())?;
    let d = sessions::load_by_id(ctx, &conn, &id)?;
    Ok(d.session.foreign_session_id)
}

/// Whether sync is set up here with no recovery kit on the list, as this
/// device last verified it: read from the list it keeps, never the network, so the
/// TUI starts as fast without a registry as with one.
fn no_recovery_kit(ctx: &Ctx) -> bool {
    let Ok(home) = krowk_api::home::dir(ctx.io.env) else { return false };
    matches!(krowk_client::keystore::Keystore::new(&home).device_list(), Ok(Some(chain)) if chain.recovery().is_none())
}
