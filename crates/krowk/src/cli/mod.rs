//! The krowk command line. `run` is the whole entry point, taking its streams
//! and environment as arguments so tests never touch the process.

mod agent;
mod auth;
pub mod catalog;
mod doctor;
pub mod exit;
pub mod flags;
pub mod help;
#[cfg(feature = "sessions")]
mod budget;
#[cfg(feature = "harness")]
mod prompt;
#[cfg(feature = "harness")]
mod providers;
#[cfg(feature = "sessions")]
mod sessions;
#[cfg(feature = "harness")]
mod status;
#[cfg(feature = "harness")]
mod tui;
mod upgrade;
mod workspace;

use crate::output::{self, jq, Format};
use flags::Flags;
use krowk_api::{fail, Error};
use std::io::Write;

/// Stamped at build time from KROWK_VERSION. An unstamped build is a source
/// build, and calling it `dev` keeps `upgrade` honest about it.
pub const VERSION: &str = match option_env!("KROWK_VERSION") {
    Some(v) => v,
    None => "dev",
};

/// Everything one invocation reads and writes.
pub struct Io<'a> {
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
    pub env: &'a dyn Fn(&str) -> String,
    /// Whether stdout is a terminal: colour, and the human format by default.
    pub tty: bool,
    /// Whether stderr is: a string --jq prints raw there could repaint it.
    pub err_tty: bool,
    /// Whether stdin is: with stdout, whether bare `krowk` opens the TUI.
    pub stdin_tty: bool,
}

/// One command's context once the command line has been read.
pub(crate) struct Ctx<'a, 'b> {
    pub io: &'a mut Io<'b>,
    pub f: Flags,
    pub format: Format,
    pub colour: bool,
    pub filter: Option<jq::Filter>,
}

impl Ctx<'_, '_> {
    pub fn env(&self, key: &str) -> String {
        (self.io.env)(key)
    }

    /// Where a rendered result reaches the caller, and the one place --jq gets
    /// to touch it, so a filter applies to every result or none.
    pub fn emit(&mut self, rendered: &str) -> Result<(), Error> {
        let Some(filter) = &self.filter else {
            let _ = writeln!(self.io.stdout, "{rendered}");
            return Ok(());
        };
        let (out, _) = filter.write(rendered, self.io.tty).map_err(|e| after_the_command_ran(&e))?;
        let _ = write!(self.io.stdout, "{out}");
        Ok(())
    }
}

/// A filter that failed after the command had already done what it does:
/// worth a sentence, since a wrapper that retries on a non-zero exit would
/// repeat an upload.
fn after_the_command_ran(err: &Error) -> Error {
    fail(
        &err.code(),
        format!(
            "{}. The command itself succeeded — this is the reading of its result failing, so running it again repeats whatever it did",
            err.fix()
        ),
    )
}

/// Runs one invocation and returns the process exit code.
pub fn run(args: &[String], io: &mut Io) -> i32 {
    let (f, positionals, parsed) = flags::parse(args);
    let jq_given = f.given.contains("jq");

    // Resolved before anything is reported, the parse error included; --jq
    // settles the format the way --json does.
    let format = match output::resolve_format(&f.format, f.json || jq_given, io.tty) {
        Ok(format) => format,
        Err(e) => return report(io, &e, Format::Json, f.quiet, false, None),
    };
    let colour = io.tty;
    // A bare `--resume` opens the session picker in the TUI; anywhere else
    // it is the missing value it always was.
    #[cfg(feature = "harness")]
    let parsed = parsed.and_then(|()| match f.resume_pick && !tui::wanted(io, &f, format, &positionals, jq_given) {
        true => Err("flag needs an argument: -resume".to_string()),
        false => Ok(()),
    });
    if let Err(why) = parsed {
        return report(io, &fail("bad_flag", format!("{why} — run `krowk --help`")), format, f.quiet, colour, None);
    }

    // Compiled before the command runs, so a typo is refused before anything
    // is uploaded, claimed or taken down.
    let mut filter = jq::compile(&f.jq, jq_given);
    if filter.is_ok() && !f.destination.is_empty() {
        let given = destination_conflict(&f, jq_given);
        if !given.is_empty() {
            filter = Err(fail(
                "bad_flag",
                format!("--destination prints the form that destination wants, so it cannot be combined with {given} — drop one of the two"),
            ));
        }
    }
    if matches!(filter, Ok(Some(_))) && !f.format.is_empty() && f.format != "json" {
        filter = Err(fail(
            "bad_flag",
            format!("--jq reads the JSON, so it cannot be combined with --format {} — drop one of the two", f.format),
        ));
    }
    let filter = match filter {
        Ok(filter) => filter,
        Err(e) => return report(io, &e, format, f.quiet, colour, None),
    };
    if let Err(e) = filter_has_something_to_read(filter.is_some(), &f, &positionals) {
        return report(io, &e, format, f.quiet, colour, None);
    }

    if f.version {
        let _ = writeln!(io.stdout, "{VERSION}");
        return exit::OK;
    }
    // `-p` is a mode rather than a command: its arguments are the prompt.
    #[cfg(feature = "harness")]
    if f.print && !f.help {
        let mut ctx = Ctx { io, f, format, colour, filter };
        return match prompt::run(&mut ctx, &positionals) {
            Ok(()) => exit::OK,
            Err(e) => {
                let quiet = ctx.f.quiet;
                report(ctx.io, &e, format, quiet, colour, None)
            }
        };
    }
    // Bare `krowk` with a person at the terminal: the agent (R-PKG-1).
    // Without one — a pipe, a file, CI capturing output — everything below
    // runs exactly as it did before the TUI existed.
    #[cfg(feature = "harness")]
    if tui::wanted(io, &f, format, &positionals, jq_given) {
        let mut ctx = Ctx { io, f, format, colour, filter };
        return match tui::run(&mut ctx) {
            Ok(()) => exit::OK,
            Err(e) => {
                let quiet = ctx.f.quiet;
                report(ctx.io, &e, format, quiet, colour, None)
            }
        };
    }
    if positionals.is_empty() && !f.help && format != Format::Json {
        let _ = write!(io.stdout, "{}", help::greeting(VERSION));
        return exit::OK;
    }

    let mut ctx = Ctx { io, f, format, colour, filter };
    let result = if ctx.f.help || positionals.is_empty() || positionals[0] == "help" {
        let topic = if positionals.first().is_some_and(|p| p == "help") { &positionals[1..] } else { &positionals[..] };
        show_help(&mut ctx, topic)
    } else {
        reject_misplaced_sessions_flags(&ctx.f, &positionals).and_then(|()| dispatch(&mut ctx, &positionals))
    };
    match result {
        Ok(()) => {
            // The nudge comes after the command's own output, and only when it
            // worked: a failure has the floor. `upgrade` just answered it.
            if positionals.first().is_some_and(|p| p != "upgrade") {
                upgrade::maybe_notify(&mut ctx);
            }
            exit::OK
        }
        Err(e) => {
            let (quiet, colour, err_tty) = (ctx.f.quiet, ctx.colour, ctx.io.err_tty);
            let filter = ctx.filter.take();
            report(ctx.io, &e, format, quiet, colour, filter.as_ref().map(|f| (f, err_tty)))
        }
    }
}

fn dispatch(ctx: &mut Ctx, p: &[String]) -> Result<(), Error> {
    let words: Vec<&str> = p.iter().map(String::as_str).collect();
    let rest = |n: usize| &p[n.min(p.len())..];
    match words.as_slice() {
        ["push", ..] => agent::upload(ctx, rest(1)),
        ["uploads", "create", ..] => agent::upload(ctx, rest(2)),
        ["uploads", "list", ..] => agent::uploads_list(ctx),
        ["uploads", "show", ..] => agent::uploads_show(ctx, rest(2)),
        ["uploads", "attach", ..] => agent::uploads_attach(ctx, rest(2)),
        ["uploads", "delete", ..] => agent::uploads_delete(ctx, rest(2)),
        ["runs", "start", ..] => agent::runs_start(ctx),
        ["runs", "list", ..] => agent::runs_list(ctx),
        ["runs", "show", ..] => agent::runs_show(ctx, rest(2)),
        ["runs", "finish", ..] => agent::runs_finish(ctx, rest(2)),
        ["claim", ..] => agent::claim(ctx, rest(1)),
        ["auth", "login", ..] => auth::login(ctx, rest(2)),
        ["auth", "token", ..] => auth::token(ctx),
        ["auth", "verify", ..] => auth::verify(ctx),
        ["upgrade", ..] => upgrade::upgrade(ctx),
        ["doctor", ..] => doctor::doctor(ctx),
        ["config", "show", ..] => workspace::config_show(ctx),
        ["config", "set", ..] => workspace::config_set(ctx, rest(2)),
        ["config", "unset", ..] => workspace::config_unset(ctx, rest(2)),
        ["workspaces"] | ["workspaces", "list", ..] => workspace::workspaces_list(ctx),
        ["workspaces", "use", ..] => workspace::workspaces_use(ctx, rest(2)),
        #[cfg(feature = "sessions")]
        ["sessions"] => sessions::list(ctx),
        #[cfg(feature = "sessions")]
        ["sessions", "show", ..] => sessions::show(ctx, rest(2)),
        #[cfg(feature = "sessions")]
        ["sessions", "budget", ..] => budget::budget(ctx, rest(2)),
        #[cfg(feature = "sessions")]
        ["sessions", "import", ..] => sessions::import(ctx),
        #[cfg(feature = "sessions")]
        ["sessions", "rebuild", ..] => sessions::rebuild(ctx),
        #[cfg(feature = "sessions")]
        ["sessions", "sync", ..] => sessions::sync(ctx),
        #[cfg(feature = "sessions")]
        ["pricing", "refresh", ..] => sessions::pricing_refresh(ctx),
        #[cfg(feature = "harness")]
        ["providers", "add", ..] => providers::add(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["providers"] | ["providers", "list", ..] => providers::list(ctx),
        #[cfg(feature = "harness")]
        ["providers", "remove", ..] => providers::remove(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["status", ..] => status::status(ctx),
        _ if catalog::catalog(VERSION).leaves().iter().any(|l| p.starts_with(&l.name.split(' ').map(String::from).collect::<Vec<_>>())) => Err(fail(
            "not_in_build",
            format!(
                "`{}` is not in this build — it is the agent build, without `sessions`; install a release, or build with `--features sessions`",
                clip(p, 2).join(" ")
            ),
        )),
        _ => Err(fail("unknown_command", format!("`{}` is not a krowk command — run `krowk --help`", clip(p, 2).join(" ")))),
    }
}

fn clip(s: &[String], n: usize) -> &[String] {
    &s[..n.min(s.len())]
}

fn show_help(ctx: &mut Ctx, topic: &[String]) -> Result<(), Error> {
    let c = catalog::catalog(VERSION);
    if topic.is_empty() {
        if ctx.format == Format::Json {
            return ctx.emit(&output::encode(&c));
        }
        let text = help::help(
            &c,
            &krowk_api::creds::credentials_path().display().to_string(),
            &crate::config::global_path().display().to_string(),
        );
        let _ = writeln!(ctx.io.stdout, "{text}");
        return Ok(());
    }
    let Some(cmd) = c.find(topic) else {
        return Err(fail(
            "unknown_command",
            format!("`{}` is not a krowk command — run `krowk help` for the list", clip(topic, 2).join(" ")),
        ));
    };
    if ctx.format == Format::Json {
        return ctx.emit(&output::encode(&cmd));
    }
    let _ = writeln!(ctx.io.stdout, "{}", help::command_help(&cmd, &c.global_flags));
    Ok(())
}

/// --destination picks a paste form, so it cannot ride with a flag that picks
/// another one.
fn destination_conflict(f: &Flags, jq_given: bool) -> String {
    if !f.format.is_empty() {
        format!("--format {}", f.format)
    } else if jq_given {
        "--jq".into()
    } else if f.json {
        "--json".into()
    } else {
        String::new()
    }
}

/// --jq is refused where a command answers with something other than JSON —
/// `auth token`'s bare secret above all — read off the catalog.
fn filter_has_something_to_read(filtering: bool, f: &Flags, p: &[String]) -> Result<(), Error> {
    if !filtering {
        return Ok(());
    }
    let name = if f.version {
        "--version".to_string()
    } else if f.help || p.first().is_some_and(|w| w == "help") {
        return Ok(());
    } else {
        match catalog::catalog(VERSION).find(p) {
            Some(cmd) if cmd.no_json => clip(p, 2).join(" "),
            _ => return Ok(()),
        }
    };
    Err(fail(
        "jq_unsupported",
        format!("`{name}` prints no JSON, so there is nothing for --jq to read — drop the flag, or ask a command that answers with a record"),
    ))
}

/// Each sessions flag is refused anywhere it does not belong: a flag that
/// means nothing where it was typed was misunderstood by whoever typed it.
fn reject_misplaced_sessions_flags(f: &Flags, p: &[String]) -> Result<(), Error> {
    let words: Vec<&str> = p.iter().map(String::as_str).collect();
    let (list, show, import, rebuild, sync) = (
        words == ["sessions"],
        words.starts_with(&["sessions", "show"]),
        words.starts_with(&["sessions", "import"]),
        words.starts_with(&["sessions", "rebuild"]),
        words.starts_with(&["sessions", "sync"]),
    );
    let owners = [
        ("dry-run", "`krowk sessions import`", import),
        ("from", "`krowk sessions import`", import),
        ("harness", "`krowk sessions`", list),
        ("worktree", "`krowk sessions`", list),
        ("all", "`krowk sessions`", list),
        ("thinking", "`krowk sessions show`", show),
        ("yes", "`krowk sessions rebuild`", rebuild),
        ("no-network", "`krowk sessions sync`", sync),
    ];
    #[cfg(feature = "harness")]
    {
        for name in ["output-format", "model", "resume", "permission-mode", "toolset", "effort", "trust"] {
            if f.given.contains(name) && !f.print {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `krowk -p`")));
            }
        }
        // A budget holds a -p session (and the TUI's, which takes no words)
        // or is the one `sessions budget` checks; anywhere else it would
        // hold nothing.
        let budget = words.starts_with(&["sessions", "budget"]);
        for name in ["max-usd", "max-tokens"] {
            if f.given.contains(name) && !f.print && !budget {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `krowk -p`, the TUI and `krowk sessions budget`")));
            }
        }
        let add = words.starts_with(&["providers", "add"]);
        for name in ["name", "api-key-env", "base-url", "client-id", "device", "binary", "config-dir"] {
            if f.given.contains(name) && !add {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `krowk providers add`")));
            }
        }
    }
    for (name, owner, allowed) in owners {
        if f.given.contains(name) && !allowed {
            return Err(fail("bad_flag", format!("`--{name}` is only a flag of {owner}")));
        }
    }
    let why = if show {
        "`krowk sessions show` reads one session"
    } else if rebuild {
        "`krowk sessions rebuild` re-imports everything"
    } else if sync {
        "`krowk sessions sync` reads everything that changed"
    } else {
        return Ok(());
    };
    if f.given.contains("limit") {
        return Err(fail("bad_flag", format!("`--limit` is only a flag of `krowk sessions` and `krowk sessions import` — {why}")));
    }
    Ok(())
}

/// Renders a failure and answers with the exit code that classifies it — the
/// one place a failure becomes a number. A failure is filtered like any other
/// result, except one --jq caused, and one whose filtering fails falls back
/// to the whole envelope rather than to silence.
fn report(io: &mut Io, err: &Error, format: Format, quiet: bool, colour: bool, filter: Option<(&jq::Filter, bool)>) -> i32 {
    let rendered = output::error(err, format, quiet, colour);
    if let Some((filter, err_tty)) = filter.filter(|_| !jq::is_filter_failure(err)) {
        match filter.write(&rendered, err_tty) {
            // An expression written for a result answers `null` over a failure;
            // printing that instead of the envelope would leave no reason.
            Ok((out, said)) if said > 0 => {
                let _ = write!(io.stderr, "{out}");
                return exit::code_for(err);
            }
            Ok(_) => {}
            Err(filter_err) => {
                let _ = writeln!(io.stderr, "{}", output::error(&filter_err, format, quiet, colour));
            }
        }
    }
    let _ = writeln!(io.stderr, "{rendered}");
    exit::code_for(err)
}

/// Whether a person is at the terminal to be asked a question.
pub(crate) fn interactive(ctx: &Ctx) -> bool {
    ctx.io.tty && ctx.format == Format::Human && !ctx.f.quiet && !in_ci(ctx)
}

fn in_ci(ctx: &Ctx) -> bool {
    krowk_api::truthy(&ctx.env("CI")) || !ctx.env("GITHUB_ACTIONS").is_empty()
}
