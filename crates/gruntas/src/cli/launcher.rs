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
use gruntas_harness::instances::Registry;
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
    let req = launch::Request { harness, mode, instance, home, plugin_dir, gruntas, sandbox: !asked.no_sandbox, args: asked.rest };
    let plan = launch::plan(&req, &|p| std::fs::read_to_string(p)).map_err(bad)?;
    match (plan.harness, &plan.mode) {
        (Harness::Claude, Mode::Attended) => claude_attended(ctx, plan),
        (Harness::Claude, Mode::Unattended { .. }) => claude_unattended(ctx, plan),
        (Harness::Codex, _) => codex(ctx, plan),
    }
}

fn not_yet(what: &str, ticket: &str) -> Error {
    fail("not_yet", format!("{what} is not in this build yet ({ticket})"))
}

/// `--account auto` (R-RUN-2): `harness`'s accounts ranked as rollover
/// ranks them, measured from the default account's login, and the top one
/// said, with why. None ranks (each limited, low on room, or on the
/// default's own login) → the default account, saying so.
fn pick_auto(ctx: &mut Ctx, registry: &Registry, harness: Harness) -> Result<String, Error> {
    use gruntas_harness::{host::clock, protocol::ModelRef, usage::Usage};
    let data = Usage::in_home(&gruntas_api::home::dir(ctx.io.env)?).read();
    let own = harness.name();
    let candidates: Vec<ModelRef> = registry.instances.keys().filter(|n| launch::account(registry, harness, n).is_ok()).map(|n| ModelRef { instance: n.clone(), model: String::new() }).collect();
    let Some(top) = data.rank(data.login_of(own), &candidates, gruntas_store::now_ms()).into_iter().next() else {
        let _ = writeln!(ctx.io.stderr, "--account auto: {own}, your own account — no other {own} account has room to spare");
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
/// directory nothing has filled yet is left out with a note, where Claude
/// Code would pass over it without a word.
fn claude_attended(ctx: &mut Ctx, mut plan: Plan) -> Result<(), Error> {
    if plan.sandboxed
        && let Err(why) = claude_sandbox_runs(&ctx.env("PATH"))
    {
        return Err(fail("sandbox_unavailable", format!("{why}; or start this run with --no-sandbox, which leaves the gruntas home readable to it")));
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
    let cant = |e: std::io::Error| fail("no_binary", format!("{} could not be started: {e} — is Claude Code installed? `gruntas providers list --check` says", plan.program.display()));
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

/// Whether Claude Code's sandbox can run here, or why not: on Linux it
/// needs bubblewrap and socat on `path`; macOS has Seatbelt built in; other
/// platforms have none.
fn claude_sandbox_runs(path: &str) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        return Err("Claude Code has no sandbox on this platform, so it cannot keep the session out of the gruntas home".into());
    }
    let missing: Vec<&str> = ["bwrap", "socat"].into_iter().filter(|b| gruntas_harness::instances::find_binary(b, path).is_none()).collect();
    match missing.as_slice() {
        [] => Ok(()),
        m => Err(format!("Claude Code's sandbox needs bubblewrap and socat, and {} is not installed — install them (`apt install bubblewrap socat`, `dnf install bubblewrap socat`, `pacman -S bubblewrap socat`)", m.join(" and "))),
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

/// LN04: attended, replace gruntas with `codex` per the plan; unattended,
/// run the task through the Codex engine in the plan's profile.
fn codex(_ctx: &mut Ctx, _plan: Plan) -> Result<(), Error> {
    Err(not_yet("`gruntas run codex`", "LN04"))
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
