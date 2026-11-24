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
        Ok(()) => super::exit::OK,
        Err(e) => report(ctx.io, &e, format, false, err_colour, None),
    }
}

fn launch_from(ctx: &mut Ctx, words: &[String]) -> Result<(), Error> {
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
        && let Err(e) = install_plugin(&plugin_dir)
    {
        let _ = writeln!(ctx.io.stderr, "gruntas: the gruntas plugin could not be written to {}: {e}", plugin_dir.display());
    }
    let req = launch::Request { harness, mode, instance, hidden: launch::hidden(&home, ctx.io.env), plugin_dir, gruntas, sandbox: !asked.no_sandbox, args: asked.rest };
    let plan = launch::plan(&req, &|p| std::fs::read_to_string(p)).map_err(bad)?;
    match (plan.harness, &plan.mode) {
        (Harness::Claude, Mode::Attended) => claude_attended(ctx, plan),
        (Harness::Claude, Mode::Unattended { .. }) => claude_unattended(ctx, plan),
        (Harness::Codex, _) => codex(ctx, plan, instance, &registry),
    }
}

/// The gruntas plugin for Claude Code, as this binary was built with it:
/// each file's path under the plugin directory, and its content.
const PLUGIN: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../../../../.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("../../../../hooks/hooks.json")),
    ("hooks/usage.ts", include_str!("../../../../hooks/usage.ts")),
    ("skills/gruntas/SKILL.md", include_str!("../../../../skills/gruntas/SKILL.md")),
];

/// The plugin `--plugin-dir` loads (D5), written into `dir`, gruntas's own
/// and no account's (R-RUN-5): only the files whose content differs, so it
/// follows this binary's version and an unchanged one is left alone.
fn install_plugin(dir: &std::path::Path) -> std::io::Result<()> {
    for (path, content) in PLUGIN {
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

/// `--account auto` (R-RUN-2): `harness`'s accounts ranked as rollover
/// ranks them, measured from the default account's login, and the top one
/// said, with why. A signed-out account is passed over, as rollover passes
/// it over. None ranks (each limited, low on room, signed out, or on the
/// default's own login) → the default account, saying so.
fn pick_auto(ctx: &mut Ctx, registry: &Registry, harness: Harness) -> Result<String, Error> {
    use gruntas_harness::{host::clock, protocol::ModelRef, readiness, usage::Usage};
    let data = Usage::in_home(&gruntas_api::home::dir(ctx.io.env)?).read();
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

/// LN02: replace gruntas with `claude` per the plan; where the sandbox
/// cannot run, say so, and start only with `--no-sandbox`. A plugin
/// directory that could not be written is left out with a note, where
/// Claude Code would pass over it without a word.
fn claude_attended(ctx: &mut Ctx, mut plan: Plan) -> Result<(), Error> {
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
    let _ = ctx.io.stdout.flush();
    let _ = ctx.io.stderr.flush();
    let mut cmd = std::process::Command::new(&plan.program);
    cmd.args(&plan.args);
    for k in &plan.env_remove {
        cmd.env_remove(k);
    }
    cmd.envs(plan.env_set.iter().map(|(k, v)| (k, v)));
    let cant = |e: std::io::Error| fail("no_binary", format!("{} could not be started: {e}. Is Claude Code installed? `gruntas providers list --check` checks", plan.program.display()));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cant(cmd.exec()))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().map_err(cant)?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// LN03: start the `--bg` session, record it with the daemon as a launched
/// unattended session (stopped once its Task is done and its turn over),
/// print its id; refused where the sandbox cannot run (D4).
fn claude_unattended(ctx: &mut Ctx, plan: Plan) -> Result<(), Error> {
    if let Err(why) = claude_sandbox_runs(ctx.io.env) {
        return Err(fail("no_sandbox", format!("an unattended session runs only inside Claude Code's sandbox, which cannot run here: {why}")));
    }
    let mut cmd = std::process::Command::new(&plan.program);
    cmd.args(&plan.args).stdin(std::process::Stdio::null());
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
    let launched = gruntas_harness::protocol::Activity::Launched { id: id.clone(), program: plan.program.display().to_string(), config_home };
    let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|_| "/".into());
    gruntas_harness::hook::tell(ctx.io.env, &cwd, super::VERSION, launched).map_err(|e| fail("no_daemon", format!("session {id} started, but the gruntas daemon could not be told to stop it when it is done ({e}); stop it yourself with `{stop}`")))?;
    let _ = writeln!(ctx.io.stdout, "{id}");
    let _ = writeln!(ctx.io.stderr, "Started {id} in the background, unattended and sandboxed; the gruntas daemon stops it once its task is done. Stop it sooner with `{stop}`.");
    Ok(())
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

/// Attended, gruntas replaced with `codex` per the plan; unattended, the
/// task started as a new session of the host daemon's Codex engine, fenced
/// off the gruntas home by the plan's profile (D3), and its id printed.
fn codex(ctx: &mut Ctx, plan: Plan, instance: &Resolved, registry: &Registry) -> Result<(), Error> {
    plugin_hint(ctx, instance)?;
    match &plan.mode {
        Mode::Attended => exec(&plan),
        Mode::Unattended { task } => codex_unattended(ctx, task, &plan, instance, registry),
    }
}

/// Replaces gruntas with the plan's program, its environment and arguments.
#[cfg(unix)]
fn exec(plan: &Plan) -> Result<(), Error> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&plan.program);
    for k in &plan.env_remove {
        cmd.env_remove(k);
    }
    let e = cmd.envs(plan.env_set.iter().map(|(k, v)| (k, v))).args(&plan.args).exec();
    Err(fail("no_binary", format!("{} could not be started: {e}", plan.program.display())))
}

#[cfg(not(unix))]
fn exec(_plan: &Plan) -> Result<(), Error> {
    Err(fail("not_supported", "`gruntas run` replaces itself with the harness, which this platform's build cannot do yet"))
}

#[cfg(unix)]
fn codex_unattended(ctx: &mut Ctx, task: &str, plan: &Plan, instance: &Resolved, registry: &Registry) -> Result<(), Error> {
    use gruntas_harness::protocol::{Attendance, Command, ModelRef, PermissionMode};
    let model = registry.model_on(instance, None, &|_| Vec::new()).map_err(|e| fail("no_model", e))?;
    let cwd = std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?;
    // The engine defines and chooses the profile itself, from the fence. A
    // worker edits the project: in acceptEdits its profile writes it, and
    // what would still be asked is refused, nobody being there to answer.
    let prompt = Command::Prompt { session_id: None, text: task.to_string(), images: Vec::new(), model: Some(ModelRef { instance: instance.name.clone(), model }), permission_mode: PermissionMode::AcceptEdits, toolset: None, effort: None, rollover: None, budget: None, attendance: Some(Attendance::Unattended), fenced: plan.sandboxed, origin: None };
    let spawn = super::host::spawner(ctx)?;
    let id = gruntas_harness::headless::start_on_daemon(ctx.io.env, &cwd, super::VERSION, &spawn, prompt).map_err(|e| super::prompt::engine_error(&e.code, &e.message, e.status))?;
    let rendered = match ctx.format {
        Format::Json => output::encode(&serde_json::json!({ "ok": true, "data": { "sessionId": id, "account": instance.name }, "summary": format!("Started {} on the task as session {id}", instance.name) })),
        _ => format!("Started {} on the task, unattended, as session {id} — follow it with `gruntas sync attach {id}`", instance.name),
    };
    ctx.emit(&rendered)
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
}
