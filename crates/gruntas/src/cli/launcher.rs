//! `gruntas run <harness> [--account <name|auto>] [--unattended "<task>"]
//! [--no-sandbox] [args…]` (Launcher): the stock `claude` or `codex` on one
//! of the person's accounts, with the gruntas plugin and the sandbox that
//! keeps it out of the gruntas home. The plan is `gruntas_harness::launch`'s;
//! this reads the command line, resolves the account and hands the plan to
//! the slice that runs it.
//!
//! Its words after the harness are the harness's, so they never reach
//! gruntas's flag parser: only `--account`, `--unattended` and
//! `--no-sandbox` are gruntas's, anywhere before a `--`, and everything else
//! (everything after a `--`, too) passes through as typed.

use super::{flags::Flags, report, Ctx, Io};
use crate::output::{self, Format};
use gruntas_api::{fail, Error};
use gruntas_harness::instances::{Registry, Resolved};
use gruntas_harness::launch::{self, Account, Harness, Mode, Plan};

/// The words `gruntas run <harness> …` was given after `run`, when that is
/// what they are: a harness word follows `run` (`gruntas run --help` and a
/// bare `gruntas run` are the parser's, and help's).
pub(super) fn words(args: &[String]) -> Option<&[String]> {
    match args {
        [run, harness, ..] if run == "run" && !harness.starts_with('-') => Some(&args[1..]),
        _ => None,
    }
}

/// What `gruntas run` was asked, the harness's own words apart.
#[derive(Debug, Default, PartialEq)]
struct Asked {
    harness: String,
    account: Option<String>,
    unattended: Option<String>,
    no_sandbox: bool,
    rest: Vec<String>,
}

fn parse(words: &[String]) -> Result<Asked, String> {
    let mut a = Asked { harness: words[0].clone(), ..Asked::default() };
    let mut it = words[1..].iter();
    while let Some(w) = it.next() {
        let (name, inline) = match w.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (w.as_str(), None),
        };
        let mut value = |flag: &str| inline.clone().or_else(|| it.next().cloned()).ok_or_else(|| format!("{flag} needs a value"));
        match name {
            "--" => {
                a.rest.extend(it.by_ref().cloned());
                break;
            }
            "--account" => {
                let v = value("--account")?;
                if v.trim().is_empty() {
                    return Err("--account needs an account's name, or auto".into());
                }
                a.account = Some(v);
            }
            "--unattended" => a.unattended = Some(value("--unattended \"<task>\"")?),
            "--no-sandbox" if inline.is_none() => a.no_sandbox = true,
            "--no-sandbox" => return Err("--no-sandbox takes no value".into()),
            _ => a.rest.push(w.clone()),
        }
    }
    if a.unattended.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err("--unattended needs the task the session starts on".into());
    }
    Ok(a)
}

/// Runs `gruntas run`, given the words after `run`, and returns the exit
/// code. Failures are reported as every command's are.
pub(super) fn run(io: &mut Io, words: &[String]) -> i32 {
    let format = output::resolve_format("", false, io.tty).unwrap_or(Format::Human);
    let colour = output::colour_for(io.tty, io.env);
    let err_colour = output::colour_for(io.err_tty, io.env);
    let mut ctx = Ctx { io, f: Flags::default(), format, colour, filter: None };
    match launch_from(&mut ctx, words) {
        Ok(code) => code,
        Err(e) => report(ctx.io, &e, format, false, err_colour, None),
    }
}

/// Its exit code: an attended harness's own, else OK.
fn launch_from(ctx: &mut Ctx, words: &[String]) -> Result<i32, Error> {
    let bad = |why: String| fail("bad_flag", format!("{why}; run `gruntas help run`"));
    let asked = parse(words).map_err(bad)?;
    let harness = Harness::parse(&asked.harness).map_err(bad)?;
    let account = Account::parse(harness, asked.account.as_deref().unwrap_or_default()).map_err(bad)?;
    let registry = Registry::resolve(&super::prompt::load_instances()?, ctx.io.env);
    let name = match account.instance(harness) {
        Some(name) => name,
        None => pick_auto(ctx, &registry, harness)?,
    };
    let instance = launch::account(&registry, harness, &name).map_err(|e| fail("no_account", e))?;
    // Checked as every command's is, and named by its resolved path (D4).
    let home = gruntas_api::home::dir(ctx.io.env)?;
    let plugin_dir = launch::plugin_dir(ctx.io.env).ok_or_else(|| fail("no_home", "there is no home directory to keep the gruntas plugin in — set HOME"))?;
    let gruntas = std::env::current_exe().map_err(|e| fail("no_binary", format!("this gruntas's own path cannot be read, which the worktree hooks run: {e}")))?;
    let mode = match asked.unattended {
        Some(task) => Mode::Unattended { task },
        None => Mode::Attended,
    };
    if harness == Harness::Claude
        && let Err(e) = install_plugin(&plugin_dir, &gruntas)
    {
        let _ = writeln!(ctx.io.stderr, "gruntas: the gruntas plugin could not be written to {}: {e}", plugin_dir.display());
    }
    let model = model_of(&asked.rest).unwrap_or_else(|| registry.model_on(instance, None, &|_| Vec::new()).unwrap_or_default());
    let req = launch::Request { harness, mode, instance, hidden: launch::hidden(&home, ctx.io.env), plugin_dir, gruntas, sandbox: !asked.no_sandbox, args: asked.rest };
    let plan = launch::plan(&req, &|p| std::fs::read_to_string(p)).map_err(bad)?;
    let ok = |()| super::exit::OK;
    match (plan.harness, &plan.mode) {
        (Harness::Claude, Mode::Unattended { .. }) => claude_unattended(ctx, plan, None).map(|_| super::exit::OK),
        (Harness::Codex, Mode::Unattended { task }) => {
            plugin_hint(ctx, instance)?;
            codex_unattended(ctx, task, &plan, instance, &registry).map(ok)
        }
        (_, Mode::Attended) => {
            if harness == Harness::Codex {
                plugin_hint(ctx, instance)?;
            }
            attended(ctx, &req, plan, model)
        }
    }
}

/// Continues session `from` on the other harness (Rollover for launched
/// sessions D6, R-RL-6), on the account and at the equivalent-tier model
/// the ladder's `other-harness` rung picked (`step`): a new unattended
/// session in `from`'s worktree, started on the handoff prompt
/// (`crossover::prompt`: its status Task and, for Claude Code, its
/// transcript's recent turns), and the daemon told to cancel `from`'s Task
/// "continued on" the new one and link the new one back. The new session's
/// id. The relaunch after an attended session's `/exit`, and the daemon's
/// move of an unattended one, call it for an `other-harness` step.
#[cfg(unix)]
pub(super) fn continue_on(ctx: &mut Ctx, step: &gruntas_harness::ladder::Step, from: &str) -> Result<String, Error> {
    continue_in(ctx, step, from, None)
}

/// `continue_on`, in `workdir` when its Task names no worktree (a session
/// outside a repository): the daemon's own directory is not the session's.
#[cfg(unix)]
pub(super) fn continue_in(ctx: &mut Ctx, step: &gruntas_harness::ladder::Step, from: &str, workdir: Option<&std::path::Path>) -> Result<String, Error> {
    use gruntas_harness::daemon::{registry::Registry as Sessions, tasks::Tasks};
    let registry = Registry::resolve(&super::prompt::load_instances()?, ctx.io.env);
    let kind = registry.instances.get(&registry.current(&step.to.instance)).map(|i| i.kind);
    let harness = [Harness::Claude, Harness::Codex].into_iter().find(|h| Some(h.kind()) == kind).ok_or_else(|| fail("no_account", format!("{} is not a Claude Code or Codex account", step.to.instance)))?;
    let instance = launch::account(&registry, harness, &step.to.instance).map_err(|e| fail("no_account", e))?;
    let socket = gruntas_harness::daemon::socket(ctx.io.env).map_err(|e| fail("no_daemon", e))?;
    let dir = socket.parent().unwrap_or(std::path::Path::new("/"));
    let task = Tasks::read(dir).get(from).cloned().ok_or_else(|| fail("no_session", format!("the gruntas daemon has no status for session {from}, which a continuation starts from")))?;
    let g = &task.metadata.gruntas;
    let cwd = match &g.worktree {
        Some(w) => std::path::PathBuf::from(&w.value),
        None => match workdir {
            Some(d) => d.to_path_buf(),
            None => std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?,
        },
    };
    // Claude Code's transcript (Codex's is not read): where its
    // registration said, while it stands, else where Claude Code keeps it
    // under the account's home.
    let claude = g.harness.as_ref().is_some_and(|h| h.value == Harness::Claude.name());
    let transcript = Sessions::read(dir).get(from).and_then(|r| r.transcript_path.clone()).map(std::path::PathBuf::from).or_else(|| {
        let env = |k: &str| Some((ctx.io.env)(k)).filter(|v| !v.is_empty());
        let home = g.account.as_ref().map(|a| a.value.clone()).or_else(|| env("CLAUDE_CONFIG_DIR")).or_else(|| env("HOME").map(|h| format!("{h}/.claude")))?;
        Some(gruntas_harness::claude::transcript_path(std::path::Path::new(&home), &cwd.display().to_string(), from))
    });
    let transcript = transcript.filter(|_| claude);
    let prompt = gruntas_harness::crossover::prompt(&task, transcript.as_deref());
    let home = gruntas_api::home::dir(ctx.io.env)?;
    let plugin_dir = launch::plugin_dir(ctx.io.env).ok_or_else(|| fail("no_home", "there is no home directory to keep the gruntas plugin in — set HOME"))?;
    let gruntas = std::env::current_exe().map_err(|e| fail("no_binary", format!("this gruntas's own path cannot be read, which the worktree hooks run: {e}")))?;
    if harness == Harness::Claude
        && let Err(e) = install_plugin(&plugin_dir, &gruntas)
    {
        let _ = writeln!(ctx.io.stderr, "gruntas: the gruntas plugin could not be written to {}: {e}", plugin_dir.display());
    }
    let args = match harness {
        Harness::Claude => vec!["--model".to_string(), step.to.model.clone()],
        Harness::Codex => Vec::new(),
    };
    let req = launch::Request { harness, mode: Mode::Unattended { task: prompt.clone() }, instance, hidden: launch::hidden(&home, ctx.io.env), plugin_dir, gruntas, sandbox: true, args };
    let plan = launch::plan(&req, &|p| std::fs::read_to_string(p)).map_err(|e| fail("launch_failed", e))?;
    let id = match harness {
        Harness::Claude => claude_unattended(ctx, plan, Some(&cwd))?,
        Harness::Codex => {
            let id = codex_start(ctx, &prompt, &plan, instance, &registry, &cwd, Some(&step.to.model))?;
            codex_thread(ctx.io.env, &id).unwrap_or(id)
        }
    };
    let continued = gruntas_harness::protocol::Activity::Continued { from: from.to_string(), to: id.clone() };
    gruntas_harness::hook::tell(ctx.io.env, &cwd.display().to_string(), super::VERSION, continued).map_err(|e| fail("no_daemon", format!("session {from} continued as {id}, but the gruntas daemon could not be told to link the two ({e})")))?;
    Ok(id)
}

/// The model the person passed the harness (`--model x`, `--model=x`, `-m x`).
fn model_of(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(m) = a.strip_prefix("--model=") {
            return Some(m.into());
        }
        if a == "--model" || a == "-m" {
            return it.next().cloned();
        }
    }
    None
}

/// D5 (Rollover for launched sessions): the harness run as gruntas's child,
/// in this terminal, with its signals and exit code passed through. Its pid
/// is marked for the daemon (`attended`), which queues the next account
/// once this one runs low and tells the session; when the harness exits
/// after that, the same session is resumed on the next account here, or
/// handed to the other harness (`other_harness`). Exiting with nothing
/// queued, or by Ctrl-C, ends the run with the harness's own code; so does
/// the daemon's pick of an account the run already left, which would only
/// go round the same accounts.
fn attended(ctx: &mut Ctx, req: &launch::Request, mut plan: Plan, mut model: String) -> Result<i32, Error> {
    use gruntas_harness::attended;
    let daemon = gruntas_harness::daemon::dir(ctx.io.env).ok();
    if let Some(d) = &daemon {
        attended::sweep(d);
    }
    let mut account = req.instance.name.clone();
    let mut left: Vec<String> = Vec::new();
    loop {
        if plan.harness == Harness::Claude {
            claude_ready(ctx, &mut plan)?;
        }
        let _ = ctx.io.stdout.flush();
        let _ = ctx.io.stderr.flush();
        let launched = attended::Launched { account: account.clone(), model: model.clone() };
        let mark = |pid: u32| {
            if let Some(d) = &daemon {
                let _ = attended::mark(d, pid, &launched);
            }
        };
        let code = child::run(&plan, mark).map_err(|e| fail("no_binary", format!("{} could not be started: {e}. Is it installed? `gruntas providers list --check` checks", plan.program.display())))?;
        let next = daemon.as_ref().and_then(|d| attended::take(d, code.pid));
        // Told to end (a closed terminal, a kill): no harness starts again.
        let Some(next) = next.filter(|_| !code.ending) else { return Ok(code.code) };
        // Quit with Ctrl-C: the person ends the run, whatever was queued.
        if code.interrupted {
            let _ = writeln!(ctx.io.stderr, "gruntas: quit with Ctrl-C, so session {} is not continued on {}", next.session, next.account);
            return Ok(code.code);
        }
        if !next.picked && (next.account == account || left.contains(&next.account)) {
            let _ = writeln!(ctx.io.stderr, "gruntas: session {} is not moved back to {}, which this run already left: every account it could go to is running low", next.session, next.account);
            return Ok(code.code);
        }
        // The accounts as they are now: the daemon picked from those.
        let registry = Registry::resolve(&super::prompt::load_instances()?, ctx.io.env);
        let Ok(to) = launch::account(&registry, plan.harness, &next.account) else { return other_harness(ctx, &next, code.code) };
        let from = launch::account(&registry, plan.harness, &account).map_err(|e| fail("no_account", e))?;
        // Claude Code's transcript: where the registration said, else under
        // the home the session ran in. None there, or one with nothing said
        // in it (a slash command the mod answered writes one), and the
        // session starts afresh: Claude Code resumes no such session.
        let from_home = runs_in(&plan, from, ctx.io.env);
        // Codex's path is kept as recorded: a missing one is the carry's to report.
        let recorded = next.transcript.as_deref().map(std::path::PathBuf::from).filter(|t| plan.harness == Harness::Codex || t.exists()).or_else(|| match plan.harness {
            Harness::Claude => from_home.as_deref().and_then(|h| find_transcript(h, &next.session)),
            Harness::Codex => None,
        });
        let empty = plan.harness == Harness::Claude
            && match &recorded {
                Some(t) => !gruntas_harness::claude::has_conversation(t),
                None => next.transcript.is_some(),
            };
        let found = recorded.filter(|_| !empty);
        let mut args = match (plan.harness, empty) {
            (_, true) => Vec::new(),
            (Harness::Claude, false) => vec!["--resume".to_string(), next.session.clone()],
            (Harness::Codex, false) => vec!["resume".to_string(), next.session.clone()],
        };
        if !next.model.is_empty() {
            args.extend(["--model".to_string(), next.model.clone()]);
        }
        let r = launch::Request { harness: req.harness, mode: Mode::Attended, instance: to, hidden: req.hidden.clone(), plugin_dir: req.plugin_dir.clone(), gruntas: req.gruntas.clone(), sandbox: req.sandbox, args };
        let next_plan = launch::plan(&r, &|p| std::fs::read_to_string(p)).map_err(|e| fail("bad_flag", e))?;
        // Into the home the next run runs in, worked out the same way.
        if let Some(t) = &found
            && let Err(e) = carry(t, from_home.as_deref(), runs_in(&next_plan, to, ctx.io.env).as_deref(), plan.harness, &next.session)
        {
            ctx.warn(&format!("session {} was not continued on {}: {e}", next.session, to.name));
            return Ok(code.code);
        }
        let _ = match empty {
            true => writeln!(ctx.io.stderr, "gruntas: session {} had nothing said yet, so a new one starts on {}", next.session, to.name),
            false => writeln!(ctx.io.stderr, "gruntas: continuing session {} on {}", next.session, to.name),
        };
        plan = next_plan;
        left.push(std::mem::replace(&mut account, next.account));
        model = next.model;
    }
}

/// The session's transcript copied from the home it ran in to the one the
/// next run runs in, where its harness finds it to resume.
fn carry(transcript: &std::path::Path, from: Option<&std::path::Path>, to: Option<&std::path::Path>, harness: Harness, session: &str) -> Result<(), String> {
    use gruntas_harness::handoff::{self, Vendor};
    let (Some(from), Some(to)) = (from, to) else { return Err("an account has no home to keep the session in".into()) };
    let vendor = if harness == Harness::Claude { Vendor::ClaudeCode } else { Vendor::Codex };
    handoff::carry(transcript, from, to, vendor, session).map(drop)
}

/// The config home `plan` runs its harness in: the account's own, or for the
/// default account the one the shell exports (as `claude` there would),
/// else the account's as gruntas knows it.
fn runs_in(plan: &launch::Plan, account: &Resolved, env: &dyn Fn(&str) -> String) -> Option<std::path::PathBuf> {
    let var = if plan.harness == Harness::Claude { "CLAUDE_CONFIG_DIR" } else { "CODEX_HOME" };
    let inherited = Some(env(var)).filter(|v| !v.is_empty() && !plan.env_remove.contains(&var)).map(std::path::PathBuf::from);
    plan.config_home.clone().or(inherited).or_else(|| account.backend.as_ref().and_then(|b| b.home.clone()))
}

/// `session`'s Claude Code transcript under `home`, in whichever project
/// directory it is.
fn find_transcript(home: &std::path::Path, session: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir(home.join("projects")).ok()?.flatten().map(|d| d.path().join(format!("{session}.jsonl"))).find(|p| p.is_file())
}

/// The continuation on the other harness, from a handoff (D6): where the
/// queued account is not one of this harness's, `continue_on` starts the
/// session there in the background, in the same worktree; the run ends
/// with the harness's code, saying where the work went on.
fn other_harness(ctx: &mut Ctx, next: &gruntas_harness::attended::Next, code: i32) -> Result<i32, Error> {
    let step = gruntas_harness::ladder::Step { rung: next.rung, to: gruntas_harness::protocol::ModelRef { instance: next.account.clone(), model: next.model.clone() } };
    let id = continue_on(ctx, &step, &next.session)?;
    let _ = writeln!(ctx.io.stderr, "gruntas: session {} continues on {} as session {id}, in the background — follow it with `gruntas status`", next.session, next.account);
    Ok(code)
}

/// The gruntas plugin for Claude Code, as this binary was built with it:
/// each file's path under the plugin directory, and its content.
const PLUGIN: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../../../../.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("../../../../hooks/hooks.json")),
    ("hooks/usage.ts", include_str!("../../../../hooks/usage.ts")),
    ("commands/account.md", include_str!("../../../../commands/account.md")),
    ("skills/gruntas/SKILL.md", include_str!("../../../../skills/gruntas/SKILL.md")),
];

/// The plugin `--plugin-dir` loads (D5), written into `dir`, gruntas's own
/// and no account's (R-RUN-5): only the files whose content differs, so it
/// follows this binary's version and an unchanged one is left alone.
fn install_plugin(dir: &std::path::Path, gruntas: &std::path::Path) -> std::io::Result<()> {
    for (path, content) in PLUGIN {
        let content = if *path == ".claude-plugin/plugin.json" { plugin_json(content, gruntas) } else { (*content).to_string() };
        let file = dir.join(path);
        if std::fs::read(&file).is_ok_and(|had| had == content.as_bytes()) {
            continue;
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file, content)?;
    }
    Ok(())
}

/// The plugin's manifest with its MCP server the `gruntas-mcp` beside
/// `gruntas`, so the worker reports to a daemon of its own version; none
/// there → as shipped, `npx -y @gruntas/mcp`. The marketplace's stays npx.
fn plugin_json(shipped: &str, gruntas: &std::path::Path) -> String {
    let mcp = gruntas.with_file_name(format!("gruntas-mcp{}", std::env::consts::EXE_SUFFIX));
    let (Some(command), true) = (mcp.to_str(), mcp.is_file()) else { return shipped.into() };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(shipped) else { return shipped.into() };
    json["mcpServers"]["gruntas"] = serde_json::json!({ "command": command, "args": [] });
    serde_json::to_string_pretty(&json).map_or_else(|_| shipped.into(), |s| s + "\n")
}

/// `--account auto` (R-RUN-2): `harness`'s accounts ranked as rollover
/// ranks them, measured from the default account's login, and the top one
/// said, with why. A signed-out account is passed over, as rollover passes
/// it over. None ranks (each limited, low on room, signed out, or on the
/// default's own login) → the default account, saying so.
fn pick_auto(ctx: &mut Ctx, registry: &Registry, harness: Harness) -> Result<String, Error> {
    use gruntas_harness::{host::clock, protocol::ModelRef, readiness, usage::Usage};
    let mut data = Usage::in_home(&gruntas_api::home::dir(ctx.io.env)?).read();
    data.link_homes(registry);
    let own = harness.name();
    let candidates: Vec<ModelRef> = registry.instances.keys().filter(|n| launch::account(registry, harness, n).is_ok()).map(|n| ModelRef { instance: n.clone(), model: String::new() }).collect();
    let (credentials, probe) = (super::providers::credentials_path()?, super::status::neutral_probe(ctx)?);
    let signed_in = |name: &str| launch::account(registry, harness, name).is_ok_and(|i| readiness::check(i, &credentials, &probe).refusal(i).is_none());
    let Some(top) = data.rank(data.login_of(own), &candidates, gruntas_store::now_ms()).into_iter().find(|r| signed_in(&r.model.instance)) else {
        let _ = writeln!(ctx.io.stderr, "--account auto: {own}, your own account — no other signed-in {own} account has room to spare");
        return Ok(own.into());
    };
    let why = match top.standing {
        Some(s) => {
            let reset = s.reset_ms.map(|r| format!(", next reset {}", clock(r))).unwrap_or_default();
            format!("first by rollover's ranking: {}% left as of {}{reset}", s.headroom.round(), clock(s.seen_at_ms))
        }
        None => format!("its usage is not known yet, and no {own} account whose usage is known has room to spare"),
    };
    let _ = writeln!(ctx.io.stderr, "--account auto: {}, {why}", top.model.instance);
    Ok(top.model.instance)
}

/// LN02: `claude` per the plan, but where the sandbox cannot run, say so,
/// and start only with `--no-sandbox`. A plugin directory that could not be
/// written is left out with a note, where Claude Code would pass over it
/// without a word.
fn claude_ready(ctx: &mut Ctx, plan: &mut Plan) -> Result<(), Error> {
    if plan.sandboxed
        && let Err(why) = claude_sandbox_runs(ctx.io.env)
    {
        return Err(fail("sandbox_unavailable", format!("Claude Code's sandbox cannot run here: {why}. Or start this run with --no-sandbox, which leaves the gruntas home readable to it")));
    }
    if let Some(i) = plan.args.iter().position(|a| a == "--plugin-dir") {
        let dir = std::path::PathBuf::from(&plan.args[i + 1]);
        if !dir.is_dir() {
            let _ = writeln!(ctx.io.stderr, "gruntas: no gruntas plugin at {} yet, so claude starts without it", dir.display());
            plan.args.drain(i..i + 2);
        }
    }
    Ok(())
}

/// The harness as a child of gruntas, its terminal its own.
mod child {
    use gruntas_harness::launch::Plan;

    /// How a harness ended: its pid, its exit code (128 + the signal that
    /// ended it), whether gruntas was told to end (SIGTERM, SIGHUP), and
    /// whether it was quit with Ctrl-C (a SIGINT within `QUIT` of its end).
    pub(super) struct Ended {
        pub pid: u32,
        pub code: i32,
        pub ending: bool,
        pub interrupted: bool,
    }

    /// How close to the harness's end the last Ctrl-C has to be to have
    /// quit it: Claude Code quits on a second Ctrl-C within about a second.
    const QUIT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Runs the plan's program to its end, `started` told its pid. Ctrl-C
    /// and Ctrl-\ reach it from the terminal, gruntas ignoring them; a
    /// SIGTERM or SIGHUP sent to gruntas is passed on to it.
    pub(super) fn run(plan: &Plan, started: impl FnOnce(u32)) -> std::io::Result<Ended> {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let blocked = relay::start();
        let mut cmd = command(plan);
        // SAFETY: between fork and exec only pthread_sigmask runs, which is
        // async-signal-safe: the harness starts with gruntas's signals
        // unblocked, which a spawned child would otherwise inherit.
        unsafe {
            cmd.pre_exec(move || {
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &blocked, std::ptr::null_mut());
                Ok(())
            });
        }
        use std::sync::atomic::Ordering::SeqCst;
        // Launching: a SIGTERM or SIGHUP now waits for the harness.
        relay::CHILD.store(relay::LAUNCHING, SeqCst);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                relay::CHILD.store(0, SeqCst);
                relay::end_if_told();
                return Err(e);
            }
        };
        relay::CHILD.store(child.id() as i32, SeqCst);
        relay::pass_on_if_told(child.id() as i32);
        started(child.id());
        let status = child.wait();
        relay::CHILD.store(0, SeqCst);
        let status = status?;
        let interrupted = relay::interrupted_within(QUIT);
        Ok(Ended { pid: child.id(), code: status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(1), ending: relay::TOLD.load(SeqCst) != 0, interrupted })
    }

    fn command(plan: &Plan) -> std::process::Command {
        let mut cmd = std::process::Command::new(&plan.program);
        cmd.args(&plan.args);
        for k in &plan.env_remove {
            cmd.env_remove(k);
        }
        cmd.envs(plan.env_set.iter().map(|(k, v)| (k, v)));
        cmd
    }

    /// gruntas's signals while a harness runs: SIGINT, SIGQUIT, SIGTERM and
    /// SIGHUP blocked (unblocked again in the child) and waited for on
    /// a thread of their own, which passes SIGTERM and SIGHUP on. The
    /// terminal sends Ctrl-C to the harness itself, so passing SIGINT on
    /// too would send it twice. With no harness running, SIGTERM or SIGHUP
    /// ends gruntas as it would have.
    mod relay {
        use std::sync::atomic::{AtomicI32, Ordering};

        /// The harness's pid; 0 with none, `LAUNCHING` while one starts.
        pub(super) static CHILD: AtomicI32 = AtomicI32::new(0);
        pub(super) const LAUNCHING: i32 = -1;
        /// The SIGTERM or SIGHUP gruntas was sent, once it was; 0 before.
        pub(super) static TOLD: AtomicI32 = AtomicI32::new(0);
        /// When the terminal last sent Ctrl-C, in milliseconds since the
        /// Unix epoch; 0 before.
        static INTERRUPTED_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

        /// Whether Ctrl-C was pressed within `d` of now.
        pub(super) fn interrupted_within(d: std::time::Duration) -> bool {
            let at = INTERRUPTED_MS.load(Ordering::SeqCst);
            at != 0 && gruntas_store::now_ms() - at <= d.as_millis() as i64
        }

        /// The signal gruntas was told to end by, passed on to the harness
        /// `pid` that started as it came.
        pub(super) fn pass_on_if_told(pid: i32) {
            match TOLD.load(Ordering::SeqCst) {
                0 => {}
                // SAFETY: kill takes any pid and signal.
                sig => unsafe {
                    libc::kill(pid, sig);
                },
            }
        }

        /// gruntas ended, as it was told to, with no harness to pass it on to.
        pub(super) fn end_if_told() {
            match TOLD.load(Ordering::SeqCst) {
                0 => {}
                sig => std::process::exit(128 + sig),
            }
        }
        static STARTED: std::sync::Once = std::sync::Once::new();

        /// The signals gruntas holds while a harness runs, held from the
        /// first call on.
        pub(super) fn start() -> libc::sigset_t {
            STARTED.call_once(|| {
                let set = set();
                // SAFETY: a valid set, and no old mask asked for.
                unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
                std::thread::spawn(move || loop {
                    let mut sig = 0;
                    // SAFETY: `set` lives in this thread; `sig` is written.
                    if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                        continue;
                    }
                    if sig == libc::SIGINT {
                        INTERRUPTED_MS.store(gruntas_store::now_ms(), Ordering::SeqCst);
                    }
                    if !matches!(sig, libc::SIGTERM | libc::SIGHUP) {
                        continue;
                    }
                    TOLD.store(sig, Ordering::SeqCst);
                    match CHILD.load(Ordering::SeqCst) {
                        // SAFETY: kill takes any pid and signal.
                        pid if pid > 0 => unsafe {
                            libc::kill(pid, sig);
                        },
                        // Passed on once it has started (`pass_on_if_told`).
                        LAUNCHING => {}
                        _ => std::process::exit(128 + sig),
                    }
                });
            });
            set()
        }

        fn set() -> libc::sigset_t {
            // SAFETY: a sigset_t is plain data, emptied before use; the
            // calls take a valid pointer to it.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                for s in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                    libc::sigaddset(&mut set, s);
                }
                set
            }
        }
    }
}

/// The Codex thread behind the engine's session `id`, once its log names
/// it: the id Codex's own hooks register the session by, and so its Task's.
/// None when it is not named within `THREAD_WAIT`.
#[cfg(unix)]
fn codex_thread(env: &dyn Fn(&str) -> String, id: &str) -> Option<String> {
    use gruntas_harness::{log, protocol::LogBody};
    let events = log::sessions_dir(env).ok()?.join(id).join(log::EVENTS_FILE);
    let deadline = std::time::Instant::now() + THREAD_WAIT;
    loop {
        let named = log::read_events(&events).ok().into_iter().flatten().find_map(|e| match e.body {
            LogBody::BackendSession { vendor_session_id, .. } => Some(vendor_session_id),
            _ => None,
        });
        if named.is_some() || std::time::Instant::now() >= deadline {
            return named;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// How long `codex_thread` waits for the thread to be named.
#[cfg(unix)]
const THREAD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// LN03: start the `--bg` session, in `cwd` (none: this one), record it
/// with the daemon as a launched unattended session (stopped once its Task
/// is done and its turn over), print its id and return it; refused where
/// the sandbox cannot run (D4).
fn claude_unattended(ctx: &mut Ctx, plan: Plan, cwd: Option<&std::path::Path>) -> Result<String, Error> {
    if let Err(why) = claude_sandbox_runs(ctx.io.env) {
        return Err(fail("no_sandbox", format!("an unattended session runs only inside Claude Code's sandbox, which cannot run here: {why}")));
    }
    let mut cmd = std::process::Command::new(&plan.program);
    cmd.args(&plan.args).stdin(std::process::Stdio::null());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    for k in &plan.env_remove {
        cmd.env_remove(k);
    }
    cmd.envs(plan.env_set.iter().map(|(k, v)| (k, v)));
    let out = cmd.output().map_err(|e| fail("no_binary", format!("{} could not be run: {e}", plan.program.display())))?;
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        // `claude --bg` asks no questions: a workspace this account has not
        // trusted is trusted at its prompt, attended, on the same account.
        let trust = match said.contains("not trusted") {
            true => " — on this account, that is `gruntas run claude --account <name>` there; plain `claude` is your default account",
            false => "",
        };
        return Err(fail("launch_failed", format!("`claude --bg` did not start the session ({}): {}{trust}", out.status, said.trim())));
    }
    let id = bg_id(&said).ok_or_else(|| fail("launch_failed", format!("`claude --bg` started, but did not say its session's id: {}", said.trim())))?;
    let config_home = plan.config_home.as_ref().map(|h| h.display().to_string());
    let stop = match &config_home {
        Some(h) => format!("CLAUDE_CONFIG_DIR={} claude stop {id}", gruntas_harness::worktree::manage::quote(std::path::Path::new(h))),
        None => format!("claude stop {id}"),
    };
    // The default account runs on whatever CLAUDE_CONFIG_DIR this shell
    // has, which the daemon, started without one, must be told.
    let inherited = Some((ctx.io.env)("CLAUDE_CONFIG_DIR")).filter(|d| !d.is_empty());
    let config_home = config_home.or(inherited);
    // Its arguments, the task (the last) left out: what rollover resumes it with.
    let args = plan.args[..plan.args.len().saturating_sub(1)].to_vec();
    let launched = gruntas_harness::protocol::Activity::Launched { id: id.clone(), program: plan.program.display().to_string(), config_home, args };
    let cwd = cwd.map(std::path::Path::to_path_buf).or_else(|| std::env::current_dir().ok()).map_or_else(|| "/".into(), |d| d.display().to_string());
    gruntas_harness::hook::tell(ctx.io.env, &cwd, super::VERSION, launched).map_err(|e| fail("no_daemon", format!("session {id} started, but the gruntas daemon could not be told to stop it when it is done ({e}); stop it yourself with `{stop}`")))?;
    let _ = writeln!(ctx.io.stdout, "{id}");
    let _ = writeln!(ctx.io.stderr, "Started {id} in the background, unattended and sandboxed; the gruntas daemon stops it once its task is done. Stop it sooner with `{stop}`.");
    Ok(id)
}

/// The id `claude --bg` says it backgrounded the session as
/// (`backgrounded · 075bf2a9`).
fn bg_id(said: &str) -> Option<String> {
    let line = said.lines().find(|l| l.trim_start().starts_with("backgrounded"))?;
    let id = line.split_once('·')?.1.trim();
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| id.to_string())
}

/// Whether Claude Code's sandbox can run here, or why not: bubblewrap that
/// makes a sandbox and socat on Linux, Seatbelt on macOS.
fn claude_sandbox_runs(env: &dyn Fn(&str) -> String) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        return match std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            true => Ok(()),
            false => Err("macOS's sandbox-exec is missing".into()),
        };
    }
    if !cfg!(target_os = "linux") {
        return Err("Claude Code's sandbox runs on Linux and macOS only".into());
    }
    gruntas_harness::sandbox::enforcer().map_err(|e| e.split(", or run without --sandbox").next().unwrap_or_default().to_string())?;
    match gruntas_harness::instances::find_binary("socat", &env("PATH")) {
        Some(_) => Ok(()),
        None => Err("it needs socat, which is not installed — install it (`apt install socat`, `dnf install socat`, `pacman -S socat`)".into()),
    }
}

/// Unattended Codex: the task started as a new session of the host
/// daemon's Codex engine, fenced off the gruntas home by the plan's profile
/// (D3), and its id printed.
#[cfg(unix)]
fn codex_unattended(ctx: &mut Ctx, task: &str, plan: &Plan, instance: &Resolved, registry: &Registry) -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?;
    codex_start(ctx, task, plan, instance, registry, &cwd, None).map(drop)
}

/// The task started in `cwd` as a new session of the host daemon's Codex
/// engine, on `model` (none: the account's default), and its id said and
/// returned.
#[cfg(unix)]
fn codex_start(ctx: &mut Ctx, task: &str, plan: &Plan, instance: &Resolved, registry: &Registry, cwd: &std::path::Path, model: Option<&str>) -> Result<String, Error> {
    use gruntas_harness::protocol::{Attendance, Command, ModelRef, PermissionMode};
    let model = registry.model_on(instance, model, &|_| Vec::new()).map_err(|e| fail("no_model", e))?;
    // The engine defines and chooses the profile itself, from the fence. A
    // worker edits the project: in acceptEdits its profile writes it, and
    // what would still be asked is refused, nobody being there to answer.
    let prompt = Command::Prompt { session_id: None, text: task.to_string(), images: Vec::new(), model: Some(ModelRef { instance: instance.name.clone(), model }), permission_mode: PermissionMode::AcceptEdits, toolset: None, effort: None, rollover: None, budget: None, attendance: Some(Attendance::Unattended), fenced: plan.sandboxed, origin: None };
    let spawn = super::host::spawner(ctx)?;
    let id = gruntas_harness::headless::start_on_daemon(ctx.io.env, cwd, super::VERSION, &spawn, prompt).map_err(|e| super::prompt::engine_error(&e.code, &e.message, e.status))?;
    let rendered = match ctx.format {
        Format::Json => output::encode(&serde_json::json!({ "ok": true, "data": { "sessionId": id, "account": instance.name }, "summary": format!("Started {} on the task as session {id}", instance.name) })),
        _ => format!("Started {} on the task, unattended, as session {id} — follow it with `gruntas sync attach {id}`", instance.name),
    };
    ctx.emit(&rendered)?;
    Ok(id)
}

#[cfg(not(unix))]
fn codex_unattended(_: &mut Ctx, _: &str, _: &Plan, _: &Resolved, _: &Registry) -> Result<(), Error> {
    Err(fail("not_supported", "an unattended session runs in the host daemon, which needs a unix socket this platform's build does not have yet"))
}

/// Where the gruntas plugin is once Codex installed it in an account.
const CODEX_PLUGIN: &str = "plugins/cache/gruntas/gruntas";

/// Codex loads a plugin only once it is installed in the account, which a
/// launch never does (D5): where it is not, how to install it is said,
/// once per account (remembered in the gruntas home).
fn plugin_hint(ctx: &mut Ctx, instance: &Resolved) -> Result<(), Error> {
    let Some(b) = &instance.backend else { return Ok(()) };
    if b.home.as_ref().is_none_or(|h| h.join(CODEX_PLUGIN).is_dir()) {
        return Ok(());
    }
    let told = gruntas_api::home::dir(ctx.io.env)?.join("launcher").join("codex-plugin-told");
    let said = std::fs::read_to_string(&told).unwrap_or_default();
    if said.lines().any(|l| l == instance.name) {
        return Ok(());
    }
    let on = match &b.config_dir {
        Some(d) => format!("CODEX_HOME={} ", gruntas_harness::worktree::manage::quote(d)),
        None => String::new(),
    };
    ctx.warn(&format!("the gruntas plugin is not installed in {}, and Codex loads a plugin only once it is — install it once with `{on}codex plugin marketplace add gruntas/gruntas && {on}codex plugin add gruntas@gruntas`", instance.name));
    // Said, or not remembered: a hint is no reason not to start.
    if let Some(dir) = told.parent() {
        let _ = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&told, format!("{said}{}\n", instance.name)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    #[test]
    fn gruntas_reads_its_own_flags_and_passes_the_rest() {
        let a = parse(&words("claude --model opus --account=work -c --no-sandbox --unattended fix")).unwrap();
        assert_eq!(a, Asked { harness: "claude".into(), account: Some("work".into()), unattended: Some("fix".into()), no_sandbox: true, rest: words("--model opus -c") });
        let a = parse(&words("codex resume -- --account x --no-sandbox")).unwrap();
        assert_eq!((a.account, a.no_sandbox, a.rest), (None, false, words("resume --account x --no-sandbox")));
        for bad in ["claude --account", "claude --account=", "claude --no-sandbox=false"] {
            assert!(parse(&words(bad)).is_err(), "{bad}");
        }
        assert!(parse(&["claude".into(), "--unattended".into(), " ".into()]).is_err());
        assert_eq!(super::words(&words("run claude -c")), Some(&words("run claude -c")[1..]));
        assert_eq!(super::words(&words("run --help")), None);
    }

    #[test]
    fn the_bg_id_is_read_from_what_claude_says() {
        let said = "Starting background service…\nbackgrounded · 075bf2a9\n  claude agents             list sessions\n";
        assert_eq!(bg_id(said).as_deref(), Some("075bf2a9"));
        assert_eq!(bg_id("backgrounded · ../x"), None);
        assert_eq!(bg_id("backgrounded"), None);
        assert_eq!(bg_id("Workspace not trusted."), None);
    }

    #[test]
    fn the_written_plugin_names_the_gruntas_mcp_beside_gruntas_else_npx() {
        let dir = std::env::temp_dir().join(format!("gruntas-plugin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (gruntas, mcp) = (dir.join("gruntas"), dir.join(format!("gruntas-mcp{}", std::env::consts::EXE_SUFFIX)));
        let server = || {
            install_plugin(&dir.join("plugin"), &gruntas).unwrap();
            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("plugin/.claude-plugin/plugin.json")).unwrap()).unwrap();
            json["mcpServers"]["gruntas"].clone()
        };
        assert_eq!(server(), serde_json::json!({ "command": "npx", "args": ["-y", "@gruntas/mcp"] }));
        std::fs::write(&mcp, "").unwrap();
        assert_eq!(server(), serde_json::json!({ "command": mcp, "args": [] }));
        std::fs::remove_file(&mcp).unwrap();
        assert_eq!(server()["command"], "npx");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
