//! `krowk -p "…"`: one prompt, answered headless by krowk's own engine, then
//! projected into krowk.db so `krowk sessions` lists it. The engine, the log
//! and the output formats live in `krowk-harness`; this is the command line
//! around them — flags, the prompt from stdin, the config, the exit code.

use super::{own_worktree, sessions, Ctx};
use crate::pricing;
use krowk_api::{fail, Error};
use krowk_harness::headless::{self, OutputFormat};
use krowk_harness::host::{HostConfig, SessionSetup};
use krowk_harness::instances::{self, Registry};
use krowk_harness::log;
use krowk_harness::readiness;
use krowk_harness::evidence::{PublishRequest, Publisher};
use krowk_harness::permissions;
use krowk_harness::sandbox::Profile;
use krowk_harness::protocol::{BudgetLimits, Effort, PermissionMode, Rollover, TurnStatus, Usage};
use std::collections::HashMap;
use krowk_harness::agents::{AgentsConfig, Models};
use krowk_harness::trust;
use crate::osc7501::{self, Report, State};
use std::io::{IsTerminal, Read};
use std::sync::Arc;

pub(super) fn run(ctx: &mut Ctx, positionals: &[String]) -> Result<(), Error> {
    sessions::check_os()?;
    if ctx.filter.is_some() || !ctx.f.format.is_empty() || ctx.f.json {
        return Err(fail("bad_flag", "-p prints its own output — pick it with --output-format text|json|stream-json instead of --format, --json or --jq"));
    }
    let format = OutputFormat::parse(&ctx.f.output_format).ok_or_else(|| {
        fail("bad_flag", format!("--output-format {} is not one krowk -p writes — one of {}", ctx.f.output_format, OutputFormat::NAMES.join(", ")))
    })?;
    let flag_mode = permission_flag(ctx)?;
    let prompt = prompt_text(positionals)?;
    // Refused before anything starts: the file named, no turn begun.
    let (images, prompt) = image_flags(ctx, prompt)?;
    let config = config_json()?;
    let registry = Registry::resolve(&instances_from(&config)?, ctx.io.env);
    registry.check_rollover().map_err(|e| fail("bad_config", e))?;
    let asked = model_flag(ctx, &registry)?;
    let sessions_dir = log::sessions_dir(ctx.io.env)?;
    let asked_resume = ctx.f.resume.clone();
    let resume = match asked_resume.as_str() {
        "" => None,
        r => Some(resolve_resume(ctx, &sessions_dir, r)?),
    };
    let cwd = std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?;
    let toolset = toolset_flag(ctx)?;
    let effort = effort_flag(ctx)?;
    let rollover = rollover_flag(ctx)?;
    let budget = budget_flag(ctx)?;
    // Who the trust prompt names: the vendor of the model the turn will run
    // on — the flag, else a resumed session's last, else the default.
    let session_model = resume.as_ref().and_then(|id| log::read_events(&sessions_dir.join(id).join(log::EVENTS_FILE)).ok()).and_then(|events| {
        events.iter().rev().find_map(|e| match &e.body {
            // On the name its instance has now, renamed or not.
            krowk_harness::protocol::LogBody::TurnStarted { model, .. } => Some(registry.current_model(model)),
            _ => None,
        })
    });
    // R-PERM-1: a repository's own allow rules, directories and hooks count
    // once it is trusted — the same list, and the same --trust, as a
    // backend's. Headless, nobody answers an approval: what would be asked
    // is refused with what would allow it.
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let store = trust::Store::new(Some(super::providers::krowk_dir()?.join(trust::FILE)), home.clone());
    let flag_trust = ctx.f.trust;
    let session_cwd = resume.as_ref().and_then(|id| log::read_events(&sessions_dir.join(id).join(log::EVENTS_FILE)).ok()).and_then(|events| {
        events.first().and_then(|e| match &e.body {
            krowk_harness::protocol::LogBody::SessionStarted { cwd, .. } => Some(std::path::PathBuf::from(cwd)),
            _ => None,
        })
    });
    // WT6: a session resumed in a krowk worktree is held there again, and
    // its worktree is trusted as the repository it was made from.
    let trust_as = own_worktree::TrustAs::default();
    let resumed = match (&resume, &session_cwd) {
        (Some(id), Some(dir)) => own_worktree::resumed(ctx, &registry, id, dir)?,
        _ => None,
    };
    if let Some(r) = &resumed {
        trust_as.set(&r.worktree);
    }
    // The daemon trusts only a repository on the trusted list, so `--trust`
    // has nothing to reach it with.
    #[cfg(not(unix))]
    if ctx.f.daemon {
        return Err(fail("not_supported", "--daemon needs a unix socket, which this platform's build does not have yet"));
    }
    #[cfg(unix)]
    if ctx.f.daemon && ctx.f.trust {
        return Err(fail("bad_flag", "--trust holds for one run in this process, and --daemon runs the turn in the host daemon — trust the repository for good (`krowk trust`), or drop --daemon"));
    }
    if ctx.f.daemon && (ctx.f.own_worktree || resumed.is_some()) {
        return Err(fail("bad_flag", "a session in a worktree of its own runs in this process, which holds the worktree and finishes it when the run ends, and --daemon runs the turn in the host daemon — drop --daemon"));
    }
    // A bare --model, or none on a session with no model yet, is routed to
    // an instance ready here now — before the trust prompt, which names the
    // vendor it runs.
    let runs_in = session_cwd.clone().unwrap_or_else(|| cwd.clone());
    let trusted = flag_trust || store.trusts(&trust_as.root(&trust::root(&runs_in)));
    let model = route(ctx, &registry, asked.as_ref(), session_model.as_ref(), &runs_in, trusted)?;
    let runs_native = model.as_ref().or(session_model.as_ref()).and_then(|m| registry.get(&m.instance).ok()).is_some_and(|i| i.backend.is_none());
    // D4: krowk runs a turn only in Claude Code or Codex — an instance named
    // here (`--model`, `defaultModel`, a resumed session's) that is neither
    // is refused before anything starts.
    if runs_native {
        return Err(fail("removed_provider", krowk_harness::connect::NO_DIRECT_MODELS));
    }
    let vendor = vendor_of(model.clone().or(session_model).as_ref(), &registry);
    let as_repo = trust_as.clone();
    let permissions = permissions_config(ctx, &config, Arc::new(move |root: &std::path::Path| flag_trust || store.trusts(&as_repo.root(root))), false);
    let (permission_mode, notices) = resolve_mode(flag_mode, &permissions, session_cwd.as_deref().unwrap_or(&cwd))?;
    sandbox_flag(&ctx.f.sandbox)?;
    for n in notices {
        ctx.warn(&n);
    }
    // Everything that can still refuse the run is asked first: a worktree
    // made before a refusal would be left behind.
    let (credentials, krowk_dir) = (super::providers::credentials_path()?, super::providers::krowk_dir()?);
    // WT6: `--worktree` makes the session's worktree now, from this
    // directory's repository, with the settings it would have run with
    // here; the session starts in it. So does `worktrees.default`, for a
    // new session, which the host daemon cannot hold one for: refused
    // with --daemon, as the flag is, never quietly run in the checkout.
    ctx.f.own_worktree = own_worktree::wanted(ctx, resume.is_none(), &cwd, &permissions, &registry);
    if ctx.f.daemon && ctx.f.own_worktree {
        return Err(fail("bad_flag", "worktrees.default starts a new session in a worktree of its own, which this process holds, and --daemon runs the turn in the host daemon — drop --daemon, or pass --worktree=false to run in place"));
    }
    let (worktree, session) = match resumed {
        Some(w) => {
            let (env, on_agent_slot) = (w.env(), w.agent.is_some());
            (Some(w), SessionSetup { env, on_agent_slot, ..SessionSetup::default() })
        }
        None if ctx.f.own_worktree => {
            let id = krowk_store::new_id();
            let (w, notes) = own_worktree::start(ctx, &cwd, &permissions, &registry, &id)?;
            trust_as.set(&w.worktree);
            let (env, on_agent_slot) = (w.env(), w.agent.is_some());
            (Some(w), SessionSetup { id: Some(id), notes, env, on_agent_slot })
        }
        None => (None, SessionSetup::default()),
    };
    let cwd = match &worktree {
        Some(w) if ctx.f.own_worktree => w.worktree.path.clone(),
        _ => cwd,
    };
    let cfg = HostConfig {
        sessions_dir,
        cwd,
        registry,
        krowk_version: super::VERSION.into(),
        pricer: pricer(ctx.io.env),
        catalog: catalog(ctx.io.env),
        credentials,
        trust: trust_gate(ctx.f.trust, super::interactive(ctx) && std::io::stdin().is_terminal() && ctx.io.err_tty, krowk_dir, home, vendor, trust_as),
        publisher: Some(publisher(ctx)),
        permissions,
        agents: agents_config(ctx.io.env),
        session,
    };
    // R-PRES-6: the run's record, on stderr when that is a terminal: working
    // now, and what it came to right before krowk exits, left behind for a
    // person who started it in another tab.
    // Its first line with text: a prompt piped in may open on a blank one.
    let topic = prompt.trim_start().lines().next().unwrap_or_default().to_string();
    report(ctx, State::Working, &topic);
    let opts = headless::Options { prompt, images, resume, model, permission_mode, toolset, effort, rollover, budget, format, worktree };
    let ran = turn(ctx, cfg, opts);
    match &ran {
        Ok(()) => report(ctx, State::Done, &topic),
        Err(e) if e.code() == "interrupted" => report(ctx, State::Idle, &topic),
        Err(e) => report(ctx, State::Error, &crate::output::headline(e)),
    }
    ran
}

/// One OSC 7501 report of the root record, on stderr and only on a
/// terminal: a file or a pipe would keep the bytes as text. Never stdout,
/// which holds the answer in the format asked for.
fn report(ctx: &mut Ctx, state: State, msg: &str) {
    if !ctx.io.err_tty {
        return;
    }
    if let Some(r) = osc7501::encode(&Report { state, msg, ..Report::default() }) {
        let _ = ctx.io.stderr.write_all(r.as_bytes());
        let _ = ctx.io.stderr.flush();
    }
}

/// The turn, from its start to what krowk exits with.
fn turn(ctx: &mut Ctx, cfg: HostConfig, opts: headless::Options) -> Result<(), Error> {
    let format = opts.format;
    let outcome = if ctx.f.daemon { on_daemon(ctx, &cfg.cwd.clone(), opts)? } else { headless::run(cfg, opts, ctx.io.stdout) };
    let _ = ctx.io.stdout.flush();
    // A kept worktree is named: in the result, when one was printed as
    // JSON, else on stderr.
    if let Some((wt, finished)) = &outcome.worktree {
        let named = format != OutputFormat::Text && outcome.result.as_ref().is_some_and(|r| r.worktree.is_some());
        if !named {
            own_worktree::report(ctx, wt, finished);
        }
    }

    // The log is the session; krowk.db is its projection, brought up to date
    // now so the listing has it. A store that cannot take it costs the
    // listing, never the answer — `krowk sessions sync` catches up.
    if let Some(id) = &outcome.session_id
        && let Err(e) = sessions::project_native(ctx.io.env, id)
    {
        ctx.warn(&format!("the session is saved, but krowk.db was not updated: {} — `krowk sessions sync` retries", e.fix()));
    }
    if let Some(e) = outcome.error {
        return Err(engine_error(&e.code, &e.message, e.status));
    }
    match outcome.result {
        Some(r) if r.status == TurnStatus::Failed => Err(match r.error {
            Some(e) => engine_error(&e.code, &e.message, e.http_status.unwrap_or(0)),
            None => fail("turn_failed", "the turn failed"),
        }),
        Some(r) if r.status == TurnStatus::Interrupted => {
            Err(fail("interrupted", format!("the turn was interrupted; what it produced is kept — continue with `krowk -p --resume {} …`", r.session_id)))
        }
        // An early offer (D9): the turn succeeded, and the next one can run elsewhere.
        Some(r) => {
            if let Some(o) = r.switch_offer.as_ref().filter(|o| o.early) {
                let room = o.room.as_ref().map(|r| format!(" ({r})")).unwrap_or_default();
                ctx.warn(&format!("{} is running low — continue on {}{room} with `krowk -p --resume {} --model {}`", o.from, o.to, r.session_id, o.to));
            }
            Ok(())
        }
        None => Ok(()),
    }
}

/// `--daemon`: the turn is the host daemon's, started first when none runs.
#[cfg(unix)]
fn on_daemon(ctx: &mut Ctx, cwd: &std::path::Path, opts: headless::Options) -> Result<headless::Outcome, Error> {
    let spawn = super::host::spawner(ctx)?;
    Ok(headless::run_on_daemon(ctx.io.env, cwd, super::VERSION, &spawn, opts, ctx.io.stdout))
}

#[cfg(not(unix))]
fn on_daemon(_: &mut Ctx, _: &std::path::Path, _: headless::Options) -> Result<headless::Outcome, Error> {
    Err(fail("not_supported", "--daemon needs a unix socket, which this platform's build does not have yet"))
}

/// `--model`, read: an instance and a model, or a bare id to route.
pub(super) fn model_flag(ctx: &Ctx, registry: &Registry) -> Result<Option<instances::Asked>, Error> {
    match ctx.f.model.as_str() {
        "" => Ok(None),
        m => registry.read_model(m).map(Some).map_err(|e| fail("bad_flag", format!("--model: {e}"))),
    }
}

/// The model a turn is asked for, routed (`readiness::route`): `--model`
/// as given when it names its instance, a bare one on an instance ready
/// here — the session's own first — and, with no `--model` on a session
/// that has no model yet, the default. None: the session keeps its model.
/// The vendors are asked in the session's directory when its repository
/// is trusted already (`trusted`: `--trust`, or remembered), so the turn's
/// own check before it starts is answered from the cache; else in krowk's
/// own directory, and no trust question is asked for routing.
pub(super) fn route(ctx: &Ctx, registry: &Registry, asked: Option<&instances::Asked>, session_model: Option<&krowk_harness::protocol::ModelRef>, runs_in: &std::path::Path, trusted: bool) -> Result<Option<krowk_harness::protocol::ModelRef>, Error> {
    if asked.is_none() && session_model.is_some() {
        return Ok(None);
    }
    if let Some(instances::Asked::Exact(m)) = asked {
        return Ok(Some(m.clone()));
    }
    let probe = match runs_in.canonicalize() {
        Ok(at) if trusted => readiness::Probe::at(at),
        _ => super::status::neutral_probe(ctx)?,
    };
    let listed = agents_config(ctx.io.env).models;
    readiness::route(registry, asked, session_model, &super::providers::credentials_path()?, &probe, &*listed).map(Some).map_err(|e| engine_error(&e.code, &e.message, e.status))
}

/// `--permission-mode`, when given.
pub(super) fn permission_flag(ctx: &Ctx) -> Result<Option<PermissionMode>, Error> {
    match ctx.f.permission_mode.as_str() {
        "" => Ok(None),
        m => PermissionMode::parse(m)
            .map(Some)
            .ok_or_else(|| fail("bad_flag", format!("--permission-mode {m} is not a mode — one of {}", PermissionMode::NAMES.join(", ")))),
    }
}

/// `--sandbox`: krowk's OS sandbox held the tools of its own turns, and
/// Claude Code and Codex run their own tools outside it, so a profile is
/// refused rather than ignored; `off` is what every run is.
fn sandbox_flag(flag: &str) -> Result<(), Error> {
    match flag {
        "" | "off" => Ok(()),
        s if Profile::parse(s).is_some() => Err(fail("sandbox_unsupported", "Claude Code and Codex run their own tools outside krowk's sandbox — drop --sandbox")),
        s => Err(fail("bad_flag", format!("--sandbox {s} is not a profile — one of {}, or off", Profile::NAMES.join(", ")))),
    }
}

/// The mode a prompt runs in: the flag, else the most specific
/// `permissions.defaultMode` the settings name (a repository's only once it
/// is trusted, and never bypassPermissions or unhinged), else default — with the
/// notices to show: a `defaultMode` krowk does not run is read as default,
/// and with the flag given no file's mode matters. A settings file that
/// does not parse is named here, flag or not, before anything runs — or
/// any trust question is asked.
pub(super) fn resolve_mode(flag: Option<PermissionMode>, cfg: &permissions::Config, cwd: &std::path::Path) -> Result<(PermissionMode, Vec<String>), Error> {
    let loaded = permissions::settings::load(cfg, cwd).map_err(bad_settings)?;
    Ok(match flag {
        Some(m) => (m, Vec::new()),
        None => (loaded.default_mode.unwrap_or_default(), loaded.notices),
    })
}

/// A settings file that does not load, as the command's error.
pub(super) fn bad_settings(e: String) -> Error {
    fail("bad_settings", format!("{e} — fix the file and run again"))
}

/// What the harness reads permission rules, instructions, skills and hooks
/// from: krowk's own config.json, Claude Code's user directory
/// (`CLAUDE_CONFIG_DIR`, else `~/.claude`), and krowk's home,
/// where remembered grants are kept and which no file tool writes.
pub(super) fn permissions_config(ctx: &Ctx, config: &serde_json::Value, trusted: permissions::settings::Trusted, approvals: bool) -> permissions::Config {
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let claude_dir = Some(ctx.env("CLAUDE_CONFIG_DIR")).filter(|d| !d.trim().is_empty()).map(std::path::PathBuf::from);
    permissions::Config {
        user: Some(config.clone()),
        user_path: super::providers::config_path().ok(),
        reread: false,
        home,
        claude_dir,
        krowk_dir: super::providers::krowk_dir().ok(),
        trusted: Some(trusted),
        approvals,
        sandbox: None,
    }
}

/// The arguments; stdin only when there are none and it is not a terminal
/// (`git diff | krowk -p`). Never both: an agent that spawns krowk with a
/// stdin it never closes would otherwise hang a prompt it passed as words.
fn prompt_text(positionals: &[String]) -> Result<String, Error> {
    let mut prompt = positionals.join(" ");
    let stdin = std::io::stdin();
    if prompt.trim().is_empty() && !stdin.is_terminal() {
        // A closed or unreadable stdin is simply no input.
        let _ = stdin.lock().take(8 << 20).read_to_string(&mut prompt);
    }
    if prompt.trim().is_empty() {
        return Err(fail("empty_prompt", "-p needs a prompt: `krowk -p \"what does this repo do?\"`, or pipe one in"));
    }
    Ok(prompt.trim_end().to_string())
}

/// The harness's part of the global config.json, whose `workspace` key the
/// rest of krowk reads. A file that does not parse is an error: somebody
/// wrote it meaning something.
/// `--image`: the files' images as a prompt carries them, and the prompt
/// with each one's `[Image #N]` put before it.
fn image_flags(ctx: &Ctx, prompt: String) -> Result<(Vec<krowk_harness::protocol::ImageInput>, String), Error> {
    if ctx.f.image.is_empty() {
        return Ok((Vec::new(), prompt));
    }
    let paths: Vec<std::path::PathBuf> = ctx.f.image.iter().map(std::path::PathBuf::from).collect();
    let images = krowk_harness::pasted::load(&paths).map_err(|e| fail("bad_image", format!("--image: {e}")))?;
    let (inputs, names) = krowk_harness::pasted::inputs(&images);
    Ok((inputs, format!("{names}{prompt}")))
}

/// `--effort`, a rung of the harness's ladder.
pub(super) fn effort_flag(ctx: &Ctx) -> Result<Option<Effort>, Error> {
    match ctx.f.effort.trim() {
        "" => Ok(None),
        e => Ok(Some(Effort::parse(e).ok_or_else(|| fail("bad_flag", format!("--effort {e} is not a rung of the ladder — one of {}", Effort::names().join(", "))))?)),
    }
}

/// `--rollover`: auto, offer or off.
pub(super) fn rollover_flag(ctx: &Ctx) -> Result<Option<Rollover>, Error> {
    match ctx.f.rollover.trim() {
        "" => Ok(None),
        r => Rollover::parse(r).map(Some).map_err(|e| fail("bad_flag", format!("--{e}"))),
    }
}

/// `--max-usd` and `--max-tokens`, as `krowk sessions budget` reads them:
/// the engine refuses the model call that would take the session past
/// either (R-BUDGET-1).
pub(super) fn budget_flag(ctx: &Ctx) -> Result<Option<BudgetLimits>, Error> {
    let l = super::budget::given_limits(ctx)?;
    let limits = BudgetLimits { max_usd: l.usd, max_tokens: l.tokens };
    Ok((!limits.is_empty()).then_some(limits))
}

/// R-EVID-1: what the host's `publish` runs — krowk_push, the MCP server's
/// own code, against the registry this invocation pushes to (`--dev` is the
/// stand-in), with the key read the way krowk-mcp reads it: KROWK_TOKEN, then
/// the workspace the config names. A workspace that names no stored key
/// refuses uploads rather than landing them anonymously, as there.
pub(super) fn publisher(ctx: &Ctx) -> Publisher {
    let base = krowk_api::base_url_for(ctx.f.dev, ctx.io.env);
    let (token, workspace_err) = match crate::config::load("", ctx.io.env, &ctx.f.workspace) {
        Err(e) => (String::new(), Some(fail("bad_config", e))),
        Ok(cfg) => match krowk_api::creds::resolve_token(ctx.io.env, &cfg.workspace) {
            Ok(t) => (t, None),
            Err(e) => (String::new(), Some(e)),
        },
    };
    // The engine runs on its own thread, so the environment the run
    // metadata is detected from is captured now.
    let vars: HashMap<String, String> = std::env::vars().collect();
    Arc::new(move |req: &PublishRequest| {
        let env = |k: &str| vars.get(k).cloned().unwrap_or_default();
        let server = crate::mcp::Server {
            client: krowk_api::Client::new(&base, &token),
            env: &env,
            version: super::VERSION.to_string(),
            root: req.root.display().to_string(),
            workspace_err: workspace_err.clone(),
        };
        server.publish(req)
    })
}

/// `--toolset`, checked against the presets the harness has.
pub(super) fn toolset_flag(ctx: &Ctx) -> Result<Option<String>, Error> {
    match ctx.f.toolset.trim() {
        "" => Ok(None),
        t if krowk_harness::toolset::by_name(t).is_some() => Ok(Some(t.to_string())),
        t => Err(fail("bad_flag", format!("--toolset {t} is not a toolset — one of {}", krowk_harness::toolset::names().join(", ")))),
    }
}

pub(super) fn load_instances() -> Result<instances::InstancesConfig, Error> {
    instances_from(&config_json()?)
}

/// The person's own config's instances, with their stored keys: the one
/// place the CLI names the credentials file keys are read from. An API key
/// or a gateway it defines for krowk to run itself is turned into a Claude
/// Code or Codex account here, on first use, with a line on stderr for
/// each (R-NAT-2).
pub(super) fn instances_from(v: &serde_json::Value) -> Result<instances::InstancesConfig, Error> {
    let path = super::providers::config_path()?;
    let credentials = super::providers::credentials_path()?;
    let env = krowk_api::home::process_env;
    let converted = krowk_harness::connect::ProviderAuth { config: path.clone(), credentials, env: &env }.convert_native();
    let now;
    let v = match converted.is_empty() {
        true => v,
        false => {
            for line in &converted {
                eprintln!("krowk: {line}");
            }
            now = config_json()?;
            &now
        }
    };
    let mut cfg = instances::from_config_json(v).map_err(|e| fail("bad_config", format!("{}: {e}", path.display())))?;
    cfg.keys_from = Some(super::providers::credentials_path()?);
    Ok(cfg)
}

/// The global config.json as JSON, or an empty object when there is none.
pub(super) fn config_json() -> Result<serde_json::Value, Error> {
    let path = super::providers::config_path()?;
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::json!({})),
        Err(e) => return Err(fail("bad_config", format!("reading {}: {e}", path.display()))),
    };
    serde_json::from_slice(&raw).map_err(|e| fail("bad_config", format!("{} is not valid JSON: {e}", path.display())))
}

/// `--resume` takes the session id a result names, or anything `krowk
/// sessions show` takes that resolves to a krowk session.
pub(super) fn resolve_resume(ctx: &mut Ctx, sessions_dir: &std::path::Path, reference: &str) -> Result<String, Error> {
    let reference = reference.trim();
    // Resuming an archived session brings it back first (R-VINT-4), and
    // one a restore was stopped before projecting is projected.
    #[cfg(unix)]
    if log::valid_id(reference) && sessions::restore_if_archived(ctx, reference)? {
        return Ok(reference.to_string());
    }
    if log::valid_id(reference) && sessions_dir.join(reference).join(log::EVENTS_FILE).is_file() {
        return Ok(reference.to_string());
    }
    let conn = sessions::open_store(ctx)?;
    let id = sessions::resolve_arg(ctx, &conn, &[reference.to_string()], "show")?;
    let d = sessions::load_by_id(ctx, &conn, &id)?;
    if d.session.harness != krowk_harness::project::HARNESS {
        return Err(fail(
            "no_session",
            format!("{reference:?} is a {} session — `krowk -p --resume` continues krowk's own sessions only", if d.session.harness.is_empty() { "foreign" } else { &d.session.harness }),
        ));
    }
    #[cfg(unix)]
    sessions::restore_if_archived(ctx, &d.session.foreign_session_id)?;
    Ok(d.session.foreign_session_id)
}

/// R-BACK-6: `claude -p` runs a repository's hooks and MCP servers without
/// the trust dialog Claude Code shows on a terminal, and Codex what its
/// project config names, so krowk asks its own before a backend is spawned. A repository trusted before, or `--trust`,
/// goes ahead; a person at the terminal is asked, and a yes is remembered;
/// anything headless is refused. The home directory and `/` are never
/// offered: only `--trust`, for one run, starts a backend there. Nothing is
/// spawned until this answers.
fn trust_gate(flag: bool, ask: bool, dir: std::path::PathBuf, home: Option<std::path::PathBuf>, vendor: &'static str, trust_as: own_worktree::TrustAs) -> trust::Gate {
    let store = trust::Store::new(Some(dir.join(trust::FILE)), home);
    Arc::new(move |root: &std::path::Path| {
        let root = &trust_as.root(root);
        if flag || store.trusts(root) {
            return Ok(());
        }
        if let Some(why) = store.refuses(root) {
            return Err(trust::untrusted(root, &format!("It cannot be trusted for good — {why}. Pass --trust to run there this once, or run krowk -p from a repository of its own.")));
        }
        if !ask {
            return Err(store.untrusted(root, "Look at what it would run, then pass --trust to run it anyway, or run `krowk trust` there to trust it for good."));
        }
        if ask_trust(&store, root, vendor) { Ok(()) } else { Err(store.untrusted(root, "Nothing was run.")) }
    })
}

/// The vendor a model runs through, for the trust prompt's words: a
/// backend instance's (`Claude Code`, `Codex`), else "the backend".
pub(super) fn vendor_of(model: Option<&krowk_harness::protocol::ModelRef>, registry: &Registry) -> &'static str {
    model.and_then(|m| registry.get(&m.instance).ok()).filter(|i| i.backend.is_some()).map(|i| i.vendor).unwrap_or("the backend")
}

/// The trust prompt itself, in plain text on stderr, answered with a line
/// on stdin: what the repository would make the vendor run, then
/// `Trust <repo>? [y/N]`. `y` trusts it and a yes is remembered; anything
/// else, or the end of input, does not.
pub(super) fn ask_trust(store: &trust::Store, root: &std::path::Path, vendor: &str) -> bool {
    use std::io::{BufRead as _, Write as _};
    let runs = trust::what_runs(root);
    let why = match vendor {
        "Claude Code" => "Claude Code runs a repository's own hooks and MCP servers without asking.".to_string(),
        "krowk" => "krowk takes a repository's hooks, allow rules and extra directories only once you trust it.".to_string(),
        v => format!("{v} runs a repository's own hooks and MCP servers without asking."),
    };
    let has = if runs.is_empty() { "Nothing of that kind is there now.".to_string() } else { format!("It has {}.", runs.join(", ")) };
    let changed = store.changes(root);
    let has = if changed.is_empty() { has } else { format!("{has} You trusted it before; {} changed since.", changed.join(", ")) };
    let mut err = std::io::stderr();
    let _ = write!(err, "{}\n{}\nTrust {}? [y/N] ", crate::termclean::cell(&why), crate::termclean::cell(&has), crate::termclean::cell(&crate::home_relative(root)));
    let _ = err.flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        let _ = writeln!(err, "Not trusted.");
        return false;
    }
    if let Err(e) = store.trust(root) {
        let _ = writeln!(err, "! Trusted for this run, but not remembered: {e}");
    }
    true
}

/// Prices a model call from the models.dev cache or the embedded snapshot,
/// as every figure `krowk sessions` shows is priced. The environment is
/// captured now: the engine runs on its own thread.
pub(super) fn pricer(env: &dyn Fn(&str) -> String) -> krowk_harness::host::Pricer {
    let (own, home) = (env("KROWK_HOME"), env("HOME"));
    Arc::new(move |provider: &str, model: &str, u: &Usage| {
        let env = |k: &str| match k {
            "KROWK_HOME" => own.clone(),
            "HOME" => home.clone(),
            _ => String::new(),
        };
        let rates = pricing::price(&env, provider, model)?;
        Some(rates.cost(pricing::Tokens {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_tokens,
            cache_write: u.cache_write_tokens,
            reasoning: u.reasoning_tokens,
        }))
    })
}

/// What the models.dev cache says of a model — its family, limits,
/// efforts and wire API (R-PROV-2). Read from the cache only: the embedded
/// snapshot is trimmed to prices, and a model it would miss is read for its
/// family off its id instead. Captured like the pricer's environment.
pub(super) fn catalog(env: &dyn Fn(&str) -> String) -> krowk_harness::host::Catalog {
    let (own, home) = (env("KROWK_HOME"), env("HOME"));
    Arc::new(move |provider: &str, model: &str| {
        let env = |k: &str| match k {
            "KROWK_HOME" => own.clone(),
            "HOME" => home.clone(),
            _ => String::new(),
        };
        let raw = std::fs::read(pricing::cache_path(&env)?).ok()?;
        krowk_harness::catalog::lookup(&raw, provider, model)
    })
}

/// The models.dev cache's listing, which a bare model id is routed by,
/// read from the cache only, like the catalog; and where krowk's worktrees
/// are made.
pub(super) fn agents_config(env: &dyn Fn(&str) -> String) -> AgentsConfig {
    let home = env("HOME");
    let own = env("KROWK_HOME");
    let models: Models = Arc::new(move |provider: &str| {
        let env = |k: &str| match k {
            "KROWK_HOME" => own.clone(),
            "HOME" => home.clone(),
            _ => String::new(),
        };
        pricing::cache_path(&env).and_then(|p| std::fs::read(p).ok()).map(|raw| krowk_harness::catalog::models(&raw, provider)).unwrap_or_default()
    });
    // Under the data directory the sandbox knows krowk's worktrees by.
    let worktrees = krowk_api::home::worktrees_root(env);
    AgentsConfig { models, worktrees }
}

/// An engine failure as krowk's error: the code and its fix, with the HTTP
/// status the provider answered, so the exit code classifies it the way it
/// classifies a registry failure.
pub(super) fn engine_error(code: &str, message: &str, status: u16) -> Error {
    Error { status, ..fail(code, message) }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;

    /// R-PERM-3: a sandbox profile is refused with why, never ignored.
    #[test]
    fn r_perm_3_a_sandbox_profile_is_refused_and_off_runs() {
        assert!(sandbox_flag("").is_ok() && sandbox_flag("off").is_ok());
        assert!(sandbox_flag("workspace").unwrap_err().fix().contains("outside krowk's sandbox"));
        assert!(sandbox_flag("loose").unwrap_err().fix().contains("not a profile"));
    }
}
