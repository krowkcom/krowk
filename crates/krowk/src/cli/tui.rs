//! Bare `krowk` on a terminal: the inline TUI (R-PKG-1). The TUI itself is
//! `krowk-tui`; this is the command line around it — when it opens, the
//! flags it takes (`--model`, `--permission-mode`, `--toolset`, `--effort`, `--resume [id]`), the
//! config it reads, and the session it leaves in krowk.db.

use super::flags::Flags;
use super::{prompt, sessions, Ctx, Io};
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::Registry;
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
    sessions::check_os()?;
    let flag_mode = prompt::permission_flag(ctx)?;
    let config = prompt::config_json()?;
    let registry = Registry::resolve(&prompt::instances_from(&config)?, ctx.io.env);
    registry.check_rollover().map_err(|e| fail("bad_config", e))?;
    let asked = prompt::model_flag(ctx, &registry)?;
    let sessions_dir = log::sessions_dir(ctx.io.env)
        .ok_or_else(|| fail("store_unavailable", "no home directory in environment: set HOME (or XDG_DATA_HOME to an absolute path) so sessions have a place to live"))?;
    let resume = if ctx.f.resume_pick {
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
    let history_file = sessions_dir.parent().map(|d| d.join("tui-history.jsonl"));
    let toolset = prompt::toolset_flag(ctx)?;
    let effort = prompt::effort_flag(ctx)?;
    let budget = prompt::budget_flag(ctx)?;
    // R-BACK-6: asked now, while the terminal is still the person's — the
    // TUI takes it raw from here on. A resumed session runs on its last
    // model and in the directory it started in.
    let past = resume.as_ref().and_then(|id| log::read_events(&sessions_dir.join(id).join(log::EVENTS_FILE)).ok()).unwrap_or_default();
    let session_model = past.iter().rev().find_map(|e| match &e.body {
        krowk_harness::protocol::LogBody::TurnStarted { model, .. } => Some(model.clone()),
        _ => None,
    });
    let session_cwd = past.first().and_then(|e| match &e.body {
        krowk_harness::protocol::LogBody::SessionStarted { cwd, .. } => Some(std::path::PathBuf::from(cwd)),
        _ => None,
    });
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let runs_in = session_cwd.clone().unwrap_or_else(|| cwd.clone());
    // The model is routed now, so the trust question below names the
    // vendor it runs. With nothing ready and nothing asked, the TUI still
    // opens — to say so on the first prompt, and let the person connect
    // one — but a bare --model that nothing can run is refused here.
    //
    // With nothing asked and an API instance ready by its key, routing
    // waits for the first prompt instead: to tell one ready instance from
    // several it would ask every installed vendor (a Node start for
    // `claude`) before the first frame, and its answer cannot be a backend
    // — the key's instance, or a refusal to guess — so no trust question
    // hangs on it.
    let known = krowk_harness::trust::Store::new(krowk_api::creds::config_dir().join(krowk_harness::trust::FILE), home.clone()).trusts(&krowk_harness::trust::root(&runs_in));
    let credentials = super::providers::credentials_path();
    let later = asked.is_none()
        && session_model.is_none()
        && registry.default_model.is_none()
        && registry.candidates(None).iter().any(|i| i.backend.is_none() && krowk_harness::readiness::local(i, &credentials).is_some_and(|r| r.is_ready()));
    let (model, route_notice) = match later {
        true => (None, None),
        false => match prompt::route(ctx, &registry, asked.as_ref(), session_model.as_ref(), &runs_in, known) {
            Ok(m) => (m, None),
            Err(e) if asked.is_none() => (None, Some(e.fix())),
            Err(e) => return Err(e),
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
    let (trust, trusted) = prompt::tui_trust_gate(effective.as_ref(), &registry, &runs_in, home, widens);
    let permissions = prompt::permissions_config(ctx, &config, trusted, true);
    let (permission_mode, mode_notices) = prompt::resolve_mode(flag_mode, &permissions, &runs_in)?;
    let host = HostConfig {
        sessions_dir,
        cwd,
        registry,
        krowk_version: super::VERSION.into(),
        pricer: prompt::pricer(ctx.io.env),
        catalog: prompt::catalog(ctx.io.env),
        credentials: super::providers::credentials_path(),
        trust,
        publisher: Some(prompt::publisher(ctx)),
        permissions,
        agents: prompt::agents_config(ctx.io.env),
    };
    let outcome = krowk_tui::run(krowk_tui::Options {
        host,
        resume,
        model,
        permission_mode,
        toolset,
        effort,
        budget,
        settings,
        history_file,
        notices: notices.into_iter().chain(mode_notices).chain(route_notice).collect(),
        version: super::VERSION.into(),
    });
    // As after `krowk -p`: the log is the session, krowk.db its listing.
    if let Some(id) = &outcome.session_id
        && let Err(e) = sessions::project_native(ctx, id)
    {
        let _ = writeln!(ctx.io.stderr, "! the session is saved, but krowk.db was not updated: {} — `krowk sessions sync` retries", e.fix());
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
