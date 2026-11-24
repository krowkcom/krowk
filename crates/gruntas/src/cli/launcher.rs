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
    account: String,
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
            "--account" => a.account = value("--account")?,
            "--unattended" => a.unattended = Some(value("--unattended \"<task>\"")?),
            "--no-sandbox" if inline.is_none() => a.no_sandbox = true,
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
    let account = Account::parse(harness, &asked.account).map_err(bad)?;
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

/// LN05: `--account auto`, the account of `harness` with the most headroom
/// by rollover's ranking, said which and why.
fn pick_auto(_ctx: &mut Ctx, _registry: &Registry, _harness: Harness) -> Result<String, Error> {
    Err(not_yet("`--account auto`", "LN05"))
}

/// LN02: replace gruntas with `claude` per the plan; where the sandbox
/// cannot run, say so, and start only with `--no-sandbox`.
fn claude_attended(_ctx: &mut Ctx, _plan: Plan) -> Result<(), Error> {
    Err(not_yet("`gruntas run claude`", "LN02"))
}

/// LN03: start the `--bg` session, record it as unattended, print its id;
/// refused where the sandbox cannot run.
fn claude_unattended(_ctx: &mut Ctx, _plan: Plan) -> Result<(), Error> {
    Err(not_yet("`gruntas run claude --unattended`", "LN03"))
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
        assert_eq!(a, Asked { harness: "claude".into(), account: "work".into(), unattended: Some("fix".into()), no_sandbox: true, rest: words("--model opus -c") });
        let a = parse(&words("codex resume -- --account x --no-sandbox")).unwrap();
        assert_eq!((a.account.as_str(), a.no_sandbox, a.rest), ("", false, words("resume --account x --no-sandbox")));
        assert!(parse(&words("claude --account")).is_err());
        assert!(parse(&["claude".into(), "--unattended".into(), " ".into()]).is_err());
        assert_eq!(super::words(&words("run claude -c")), Some(&words("run claude -c")[1..]));
        assert_eq!(super::words(&words("run --help")), None);
    }
}
