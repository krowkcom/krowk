//! The gruntas command line. `run` is the whole entry point, taking its streams
//! and environment as arguments so tests never touch the process.

mod agent;
#[cfg(all(feature = "harness", unix))]
mod agents;
mod auth;
mod billing;
pub mod catalog;
mod doctor;
mod health;
pub mod exit;
pub mod flags;
pub mod help;
#[cfg(feature = "sessions")]
mod budget;
#[cfg(feature = "harness")]
mod chain;
#[cfg(feature = "harness")]
mod devices;
#[cfg(feature = "harness")]
mod first_run;
#[cfg(all(feature = "harness", unix))]
mod delegate;
#[cfg(all(feature = "harness", unix))]
mod god;
#[cfg(all(feature = "harness", unix))]
mod host;
#[cfg(all(feature = "harness", unix))]
mod accept;
#[cfg(all(feature = "harness", unix))]
mod power;
mod machine;
#[cfg(feature = "harness")]
mod mcp;
#[cfg(feature = "harness")]
mod own_worktree;
#[cfg(all(feature = "harness", unix))]
mod launcher;
#[cfg(all(feature = "harness", unix))]
pub mod account;
#[cfg(feature = "harness")]
mod pairing;
#[cfg(feature = "harness")]
mod prompt;
#[cfg(all(feature = "harness", unix))]
mod relay;
#[cfg(feature = "harness")]
mod providers;
#[cfg(feature = "sessions")]
mod sessions;
mod suggest;
#[cfg(feature = "harness")]
mod status;
#[cfg(feature = "harness")]
mod recovery;
#[cfg(feature = "harness")]
mod reseal;
#[cfg(feature = "harness")]
mod sync;
#[cfg(all(feature = "harness", unix))]
mod synced;
#[cfg(feature = "harness")]
mod trust;
mod upgrade;
mod workspace;
#[cfg(feature = "harness")]
mod worktrees;

use crate::output::{self, jq, Format};
use flags::Flags;
use gruntas_api::{fail, Error};
use std::io::Write;

/// Stamped at build time from GRUNTAS_VERSION. An unstamped build is a source
/// build, and calling it `dev` keeps `upgrade` honest about it.
pub const VERSION: &str = match option_env!("GRUNTAS_VERSION") {
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
    /// Whether stdin is: with stdout, whether `gruntas -p` may ask a question.
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

    /// A warning on stderr, painted when stderr is a terminal that wants it.
    pub fn warn(&mut self, said: &str) {
        let colour = output::colour_for(self.io.err_tty, self.io.env);
        let _ = writeln!(self.io.stderr, "{}", output::warning(colour, said));
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
/// gruntas-mcp's delegation tools (`mcp.rs`): the `Placer` its tasks are
/// placed by, and `gruntas machines --json`.
#[cfg(all(feature = "harness", unix))]
pub use delegate::{machines, placer};
/// gruntas-mcp's god agent tools: a question for the machine holding a session.
#[cfg(all(feature = "harness", unix))]
pub use agents::ask_holder;
/// gruntas-mcp's gruntas_status, gruntas_changes and gruntas_ping (GA03).
#[cfg(all(feature = "harness", unix))]
pub(crate) use agents::{ask_every, boards, ping_data, span, state_word, tree, Asked, Machine};
/// gruntas-mcp's steering tools: this machine's name, for a steer's origin.
#[cfg(all(feature = "harness", unix))]
pub use god::this_machine;

pub fn run(args: &[String], io: &mut Io) -> i32 {
    // `gruntas run <harness> …`: the words after the harness are the
    // harness's, so they never reach gruntas's flag parser.
    #[cfg(all(feature = "harness", unix))]
    if let Some(words) = launcher::words(args) {
        return launcher::run(io, words);
    }
    let (f, positionals, parsed) = flags::parse(args);
    let typed = positionals;
    let positionals = command_words(&f, &typed);
    let jq_given = f.given.contains("jq");

    // Resolved before anything is reported, the parse error included; --jq
    // settles the format the way --json does.
    let format = match output::resolve_format(&f.format, f.json || jq_given, io.tty) {
        Ok(format) => format,
        Err(e) => return report(io, &e, Format::Json, f.quiet, false, None),
    };
    // Each stream is painted only when it is a terminal that wants paint, so a
    // failure on a terminal is red even while stdout is piped.
    let colour = output::colour_for(io.tty, io.env);
    let err_colour = output::colour_for(io.err_tty, io.env);
    // A resumed session runs in the directory it ran in.
    #[cfg(feature = "harness")]
    let parsed = parsed.and_then(|()| match f.own_worktree && !f.resume.is_empty() {
        true => Err("--worktree starts a new session in a worktree of its own, and --resume continues one in the directory it ran in — drop one of them".to_string()),
        false => Ok(()),
    });
    if let Err(why) = parsed {
        return report(io, &fail("bad_flag", format!("{why}; run `{}`", help_for(&positionals))), format, f.quiet, err_colour, None);
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
        Err(e) => return report(io, &e, format, f.quiet, err_colour, None),
    };
    if let Err(e) = filter_has_something_to_read(filter.is_some(), &f, &positionals) {
        return report(io, &e, format, f.quiet, err_colour, None);
    }

    if f.version {
        let _ = writeln!(io.stdout, "{VERSION}");
        return exit::OK;
    }
    // A home gruntas cannot use — a GRUNTAS_HOME that is not absolute, a
    // symlink or another user's directory in its place, an older layout
    // that could not be moved in — is refused before anything runs, rather
    // than read around. One `lstat` when it is there; none is fine (a
    // container's anonymous push needs no home).
    if let Err(e) = gruntas_api::home::dir(io.env)
        && e.code() != "no_home"
    {
        return report(io, &e, format, f.quiet, err_colour, None);
    }
    // `-p` is a mode rather than a command: its arguments are the prompt.
    #[cfg(feature = "harness")]
    if prompting(&f) {
        let mut ctx = Ctx { io, f, format, colour, filter };
        return match prompt::run(&mut ctx, &positionals) {
            Ok(()) => exit::OK,
            Err(e) => {
                let quiet = ctx.f.quiet;
                report(ctx.io, &e, format, quiet, err_colour, None)
            }
        };
    }
    // `gruntas --resume <id>` for a session another machine runs: attach it
    // through sync, terminal or not (ticket 21's cards copy this command).
    #[cfg(all(feature = "harness", unix))]
    let (io, f, filter) = if positionals.is_empty() && !f.resume.is_empty() {
        let mut ctx = Ctx { io, f, format, colour, filter };
        if let Some(r) = synced::resume(&mut ctx) {
            return match r {
                Ok(()) => exit::OK,
                Err(e) => {
                    let quiet = ctx.f.quiet;
                    report(ctx.io, &e, format, quiet, err_colour, None)
                }
            };
        }
        (ctx.io, ctx.f, ctx.filter)
    } else {
        (io, f, filter)
    };
    // The flags that once opened an interactive client mean nothing without a command
    // or `-p`: refused with the way to the same thing, on a terminal or not.
    #[cfg(feature = "harness")]
    if positionals.is_empty() && !f.help {
        for name in ["model", "permission-mode", "toolset", "effort", "resume", "worktree", "max-usd", "max-tokens"] {
            if f.given.contains(name) {
                let e = fail("bad_flag", format!("`--{name}` has nothing to act on without a command or `-p` — gruntas has no interactive screen; run `gruntas -p \"<prompt>\" --{name} …`"));
                return report(io, &e, format, f.quiet, err_colour, None);
            }
        }
    }
    // Bare `gruntas` is the overview wherever it runs; the short greeting is
    // for the build with no agent, or when the human format is asked for.
    if positionals.is_empty() && !f.help && format != Format::Json && (!cfg!(feature = "harness") || f.format == "human") {
        let _ = write!(io.stdout, "{}", help::greeting(VERSION));
        return exit::OK;
    }

    let mut ctx = Ctx { io, f, format, colour, filter };
    let result = if ctx.f.help || positionals.is_empty() || positionals[0] == "help" {
        let topic = if positionals.first().is_some_and(|p| p == "help") { &positionals[1..] } else { &positionals[..] };
        // Bare `gruntas`: the first machine's setup, with a person at the
        // terminal to answer it, then the overview with what needs doing.
        #[cfg(feature = "harness")]
        let bare = positionals.is_empty() && !ctx.f.help;
        #[cfg(feature = "harness")]
        if bare && interactive(&ctx) && ctx.io.stdin_tty && ctx.io.err_tty {
            first_run::bare(&mut ctx);
        }
        let shown = show_help(&mut ctx, topic);
        #[cfg(feature = "harness")]
        if bare && shown.is_ok() && help_is_human(&ctx) {
            for line in first_run::overview_lines(&ctx) {
                let _ = writeln!(ctx.io.stdout, "\n{}", output::warning(ctx.colour, &line));
            }
        }
        shown
    } else {
        reject_misplaced_sessions_flags(&ctx.f, &positionals).and_then(|()| dispatch(&mut ctx, &positionals, &typed))
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
            let (quiet, err_tty) = (ctx.f.quiet, ctx.io.err_tty);
            let filter = ctx.filter.take();
            report(ctx.io, &e, format, quiet, err_colour, filter.as_ref().map(|f| (f, err_tty)))
        }
    }
}

/// The words as the router reads them: an older name for a command put back
/// as the one it now is — except under -p, where the words are the prompt,
/// which reaches the model as typed.
fn command_words(f: &Flags, typed: &[String]) -> Vec<String> {
    if prompting(f) { typed.to_vec() } else { catalog::canonical(typed) }
}

/// Whether this is `gruntas -p`, whose words are a prompt rather than a command.
fn prompting(f: &Flags) -> bool {
    #[cfg(feature = "harness")]
    return f.print && !f.help;
    #[cfg(not(feature = "harness"))]
    {
        let _ = f;
        false
    }
}

/// `p` is what was typed with any older command name put back as the one it
/// now is; `typed` is as typed, for quoting back.
fn dispatch(ctx: &mut Ctx, p: &[String], typed: &[String]) -> Result<(), Error> {
    let words: Vec<&str> = p.iter().map(String::as_str).collect();
    let rest = |n: usize| &p[n.min(p.len())..];
    match words.as_slice() {
        ["push", ..] => agent::upload(ctx, rest(1)),
        ["artifacts", "create", ..] => agent::upload(ctx, rest(2)),
        ["artifacts", "list", ..] => agent::uploads_list(ctx),
        ["artifacts", "show", ..] => agent::uploads_show(ctx, rest(2)),
        ["artifacts", "attach", ..] => agent::uploads_attach(ctx, rest(2)),
        ["artifacts", "delete", ..] => agent::uploads_delete(ctx, rest(2)),
        ["artifacts", "claim", ..] => agent::claim(ctx, rest(2)),
        ["runs", "start", ..] => agent::runs_start(ctx),
        ["runs", "list", ..] => agent::runs_list(ctx),
        ["runs", "show", ..] => agent::runs_show(ctx, rest(2)),
        ["runs", "finish", ..] => agent::runs_finish(ctx, rest(2)),
        ["claim", ..] => agent::claim(ctx, rest(1)),
        ["auth", "login", ..] => auth::login(ctx, rest(2)),
        ["auth", "logout", ..] => auth::logout(ctx, rest(2)),
        ["auth", "token", ..] => auth::token(ctx),
        ["auth", "verify", ..] => auth::verify(ctx),
        // Your gruntas account, short: `gruntas connect` is a model provider.
        ["login", ..] => auth::login(ctx, rest(1)),
        ["logout", ..] => auth::logout(ctx, rest(1)),
        ["whoami", ..] => auth::verify(ctx),
        ["billing", ..] => billing::billing(ctx, rest(1)),
        ["upgrade", ..] => upgrade::upgrade(ctx),
        ["doctor", ..] => doctor::doctor(ctx),
        ["config", "show", ..] => workspace::config_show(ctx),
        ["config", "get", ..] => workspace::config_get(ctx, rest(2)),
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
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "archive", ..] => sessions::archive(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "restore", ..] => sessions::restore(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "vintage", ..] => sessions::vintage(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "pin", ..] => sessions::pin(ctx, rest(2), true),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "unpin", ..] => sessions::pin(ctx, rest(2), false),
        #[cfg(feature = "sessions")]
        ["pricing", "refresh", ..] => sessions::pricing_refresh(ctx),
        #[cfg(feature = "harness")]
        ["connect", ..] => providers::connect(ctx, rest(1)),
        #[cfg(feature = "harness")]
        ["disconnect", ..] => providers::disconnect(ctx, rest(1)),
        #[cfg(feature = "harness")]
        ["providers", "add", ..] => providers::add(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["providers"] | ["providers", "list", ..] if ctx.f.check => status::status(ctx),
        #[cfg(feature = "harness")]
        ["providers"] | ["providers", "list", ..] => providers::list(ctx),
        #[cfg(feature = "harness")]
        ["providers", "next", ..] => providers::next(ctx),
        #[cfg(feature = "harness")]
        ["providers", "remove", ..] => providers::remove(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["providers", "rename", ..] => providers::rename(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["trust", ..] => trust::run(ctx, rest(1)),
        #[cfg(feature = "harness")]
        ["mcp", "add", ..] => mcp::add(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["mcp", "remove", ..] => mcp::remove(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["mcp"] | ["mcp", "list", ..] => mcp::list(ctx),
        // The agent tree; `agents` is the name it had for a release.
        #[cfg(all(feature = "harness", unix))]
        ["status", ..] | ["agents", ..] => agents::agents(ctx, rest(1)),
        #[cfg(all(feature = "harness", unix))]
        ["ping", ..] => agents::ping(ctx, rest(1)),
        #[cfg(feature = "harness")]
        ["mcp", "login", ..] => mcp::login(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["mcp", "logout", ..] => mcp::logout(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["host"] => host::status(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["host", "serve", ..] => host::serve(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["host", "status", ..] => host::status(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["host", "attach", ..] => host::attach(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["host", "stop", ..] => host::stop(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["host", "enable", ..] => host::enable(ctx, true),
        #[cfg(all(feature = "harness", unix))]
        ["host", "disable", ..] => host::enable(ctx, false),
        #[cfg(all(feature = "harness", unix))]
        ["machines", "accept", ..] => accept::accept(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["machines"] | ["machines", "list", ..] | ["hosts", ..] => synced::machines(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["machines", "add", ..] => pairing::add(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["machines", "remove", ..] => devices::remove(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["machines", "always-on", ..] => match rest(2).first().map(String::as_str) {
            Some("on") => host::enable(ctx, true),
            Some("off") => host::enable(ctx, false),
            _ => Err(fail("missing_argument", "say on or off: `gruntas machines always-on on` keeps this machine available while you're away, `off` stops that")),
        },
        #[cfg(all(feature = "harness", unix))]
        ["machines", "recovery"] => recovery::status(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["machines", "recovery", "new", ..] => recovery::new_kit(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["machines", "recovery", "check", ..] => recovery::check(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["machines", "recovery", "discard-old", ..] => recovery::discard_old(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["relay"] => show_help(ctx, p),
        #[cfg(all(feature = "harness", unix))]
        ["relay", "serve", ..] => relay::serve(ctx),
        #[cfg(feature = "harness")]
        ["sync"] => show_help(ctx, p),
        #[cfg(feature = "harness")]
        ["sync", "init", ..] => recovery::init(ctx),
        #[cfg(feature = "harness")]
        ["sync", "recover", ..] => recovery::recover(ctx),
        #[cfg(feature = "harness")]
        ["sync", "status", ..] => recovery::status(ctx),
        #[cfg(feature = "harness")]
        ["sync", "recovery"] => show_help(ctx, p),
        #[cfg(feature = "harness")]
        ["sync", "recovery", "new", ..] => recovery::new_kit(ctx),
        #[cfg(feature = "harness")]
        ["sync", "recovery", "check", ..] => recovery::check(ctx),
        #[cfg(feature = "harness")]
        ["sync", "recovery", "discard-old", ..] => recovery::discard_old(ctx),
        #[cfg(feature = "harness")]
        ["sync", "join", ..] => pairing::join(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sync", "sessions", ..] => synced::sessions(ctx),
        #[cfg(all(feature = "harness", unix))]
        ["sync", "host", ..] => synced::host_session(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sync", "attach", ..] => synced::attach(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sync", "take", ..] => synced::take(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sync", "new", ..] => synced::new_session(ctx, rest(2), "sync new"),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "attach", ..] => synced::attach(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "take", ..] => synced::take(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "new", ..] => synced::new_session(ctx, rest(2), "sessions new"),
        // `gruntas run <harness> …` was routed before the flags were read;
        // here it is a flag before the harness, or no harness.
        #[cfg(all(feature = "harness", unix))]
        ["run", ..] => Err(fail("missing_argument", "name the harness right after `run`, before any flag: `gruntas run claude --account work`, or `gruntas run codex`")),
        #[cfg(feature = "harness")]
        ["worktrees"] | ["worktrees", "list", ..] => worktrees::list(ctx),
        #[cfg(feature = "harness")]
        ["worktrees", "add", ..] => worktrees::add(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["worktrees", "remove", ..] => worktrees::remove(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["worktrees", "apply", ..] => worktrees::apply(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["worktrees", "prune", ..] => worktrees::prune(ctx),
        #[cfg(feature = "harness")]
        ["devices"] | ["devices", "list", ..] => devices::list(ctx),
        #[cfg(feature = "harness")]
        ["devices", "add", ..] => pairing::add(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["devices", "remove", ..] => devices::remove(ctx, rest(2)),
        _ if missing(p) => Err(not_in_build(p)),
        _ => Err(unknown_command(p, typed)),
    }
}

/// A command gruntas does not have, with the nearest one it does when the
/// words look like a typo of it.
fn unknown_command(p: &[String], typed: &[String]) -> Error {
    let c = catalog::catalog(VERSION);
    let typed = clip(typed, 2).join(" ");
    let Some(group) = c.commands.iter().find(|cmd| cmd.name == p[0] && !cmd.subcommands.is_empty()) else {
        let near = suggest::closest(&p[0], c.commands.iter().map(|cmd| cmd.name.as_str()).chain(aliases())).map(canonical_name);
        // A subcommand the guessed group has rides along: `artifacts list`.
        let near = near.map(|n| match (c.find(std::slice::from_ref(&n)), p.get(1)) {
            (Some(g), Some(sub)) if g.subcommands.iter().any(|s| &s.name == sub) => format!("{n} {sub}"),
            _ => n,
        });
        return match near {
            Some(near) => fail("unknown_command", format!("`{typed}` is not a gruntas command — did you mean `gruntas {near}`?")),
            None => fail("unknown_command", format!("`{typed}` is not a gruntas command — run `gruntas --help`")),
        };
    };
    // A group alone, or with a subcommand it lacks: name the ones it has.
    let subs: Vec<&str> = group.subcommands.iter().map(|s| s.name.as_str()).collect();
    let near = p.get(1).and_then(|w| suggest::closest(w, subs.iter().copied()));
    match (p.get(1), near) {
        (Some(_), Some(near)) => fail("unknown_command", format!("`{typed}` is not a gruntas command — did you mean `gruntas {} {near}`?", group.name)),
        (Some(_), None) => fail(
            "unknown_command",
            format!("`{typed}` is not a gruntas command; the {} command takes {}; run `gruntas help {}`", group.name, either(&subs), group.name),
        ),
        (None, _) => fail("unknown_command", format!("the {} command needs a subcommand: {}; run `gruntas help {}`", group.name, either(&subs), group.name)),
    }
}

/// "a, b or c".
fn either(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The older names commands still answer to, offered as guesses too.
fn aliases<'a>() -> impl Iterator<Item = &'a str> {
    catalog::ALIASES.iter().map(|(old, _)| *old)
}

/// A guess named the way help names it now.
fn canonical_name(name: &str) -> String {
    catalog::ALIASES.iter().find(|(old, _)| *old == name).map_or(name, |(_, now)| *now).to_string()
}

/// Where the flags of what was typed are explained: the command's own help,
/// or the overview when no command was named.
fn help_for(p: &[String]) -> String {
    let c = catalog::catalog(VERSION);
    match c.find(p).or_else(|| c.find(clip(p, 1))) {
        Some(cmd) => format!("gruntas help {}", cmd.name),
        None => "gruntas --help".into(),
    }
}

/// Whether the words name a command the catalog has and this build does
/// not: `sessions` and `pricing`, in the agent build. Bare or with a
/// subcommand, typed to run or to `help`.
fn missing(p: &[String]) -> bool {
    !cfg!(feature = "sessions") && p.first().is_some_and(|w| w == "sessions" || w == "pricing")
}

/// A command the catalog names that this build does not have: the agent
/// build, without `sessions`.
fn not_in_build(p: &[String]) -> Error {
    fail(
        "not_in_build",
        format!(
            "`{}` is not in this build — it is the agent build, without `sessions`; install a release, or build with `--features sessions`",
            clip(p, 2).join(" ")
        ),
    )
}

fn clip(s: &[String], n: usize) -> &[String] {
    &s[..n.min(s.len())]
}

/// Help is read, by a person or an agent, so it is the page unless JSON was
/// asked for by name: piped, the whole catalog would bury the overview.
fn help_is_human(ctx: &Ctx) -> bool {
    let asked_for_json = ctx.f.json || ctx.filter.is_some() || ctx.f.format == "json";
    ctx.format != Format::Json || !asked_for_json
}

fn show_help(ctx: &mut Ctx, topic: &[String]) -> Result<(), Error> {
    let c = catalog::catalog(VERSION);
    // `gruntas help uploads` is the help of what `uploads` now is.
    let (typed, topic) = (topic, &catalog::canonical(topic));
    let human = help_is_human(ctx);
    if topic.is_empty() {
        if !human {
            return ctx.emit(&output::encode(&c));
        }
        let text = help::help(&c, ctx.f.all, ctx.colour);
        let _ = writeln!(ctx.io.stdout, "{text}");
        return Ok(());
    }
    if missing(topic) {
        return Err(not_in_build(topic));
    }
    if let Some(cmd) = c.find(topic) {
        if !human {
            return ctx.emit(&output::encode(&cmd));
        }
        let _ = writeln!(ctx.io.stdout, "{}", help::command_help(&cmd, &c.global_flags[..catalog::CORE_FLAGS], ctx.colour));
        return Ok(());
    }
    let (credentials, config) = (gruntas_api::creds::credentials_text(), crate::config::global_text());
    let files = help::Files { credentials: &credentials, config: &config };
    let page = match topic[0].as_str() {
        "topics" if topic.len() == 1 => Some(help::topics(ctx.colour)),
        name if topic.len() == 1 => help::topic(name, &c, &files, ctx.colour),
        _ => None,
    };
    let Some(page) = page else {
        let names = c.commands.iter().map(|cmd| cmd.name.as_str()).chain(help::TOPICS.iter().map(|(n, _)| *n));
        let typed = clip(typed, 2).join(" ");
        let near = suggest::closest(&topic[0], names.chain(aliases())).map(canonical_name);
        return Err(match near {
            Some(near) => fail("unknown_command", format!("`{typed}` is not a gruntas command or help topic — did you mean `gruntas help {near}`?")),
            None => fail("unknown_command", format!("`{typed}` is not a gruntas command or help topic — run `gruntas help`")),
        });
    };
    // A topic is prose, so its JSON is the prose as one string.
    if !human {
        return ctx.emit(&output::encode(&page));
    }
    let _ = writeln!(ctx.io.stdout, "{page}");
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

/// Who takes `--all`: `help` (handled before this check), and `sessions` in
/// the builds that have it.
#[cfg(feature = "sessions")]
const ALL_OWNERS: &str = "`gruntas sessions` and `gruntas help`";
#[cfg(not(feature = "sessions"))]
const ALL_OWNERS: &str = "`gruntas help`";

/// Who takes `--worktree`: `sessions`, whose `--worktree <path>` filters
/// the listing, and in the agent's build a new session, which starts in a
/// worktree of its own.
#[cfg(feature = "harness")]
const WORKTREE_OWNERS: &str = "`gruntas sessions` (`--worktree <path>`) and `gruntas -p`";
#[cfg(not(feature = "harness"))]
const WORKTREE_OWNERS: &str = "`gruntas sessions`";

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
        ("dry-run", "`gruntas sessions import`", import),
        ("from", "`gruntas sessions import`, `gruntas sessions restore`, `gruntas worktrees add` and `gruntas providers next`", import || words.starts_with(&["sessions", "restore"]) || words.starts_with(&["worktrees", "add"]) || words.starts_with(&["providers", "next"])),
        ("harness", "`gruntas sessions`", list),
        ("worktree", WORKTREE_OWNERS, list),
        ("all", ALL_OWNERS, list),
        ("thinking", "`gruntas sessions show`", show),
        ("older-than", "`gruntas sessions archive`", words.starts_with(&["sessions", "archive"])),
        ("weekly", "`gruntas sessions archive`", words.starts_with(&["sessions", "archive"])),
        ("yes", "`gruntas sessions rebuild`", rebuild),
        ("no-network", "`gruntas sessions sync`", sync),
    ];
    #[cfg(feature = "harness")]
    {
        // `sync new` takes its own flags and those every command takes, no
        // other: no `--permission-mode` above all, since the host runs the
        // session in its own default mode (R-HOST-4).
        let new = cfg!(unix) && (words.starts_with(&["sync", "new"]) || words.starts_with(&["sessions", "new"]));
        if new {
            let own = catalog::catalog("").find(&[words[0].into(), "new".into()]).map(|c| c.flags).unwrap_or_default();
            let takes = |name: &str| own.iter().chain(&catalog::global_flags()[..catalog::CORE_FLAGS]).any(|f| f.name == name);
            if let Some(name) = f.given.iter().find(|n| !takes(n)) {
                return Err(fail("bad_flag", format!("unknown flag --{name} for `gruntas {} new`, which takes {}, --cwd and --model; the host runs the session in its own default permission mode", words[0], if words[0] == "sync" { "--host" } else { "--on" })));
            }
        }
        for name in ["host", "on", "cwd"] {
            if f.given.contains(name) && !new {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `gruntas sessions new` (`gruntas sync new` for --host)")));
            }
        }
        for name in ["output-format", "model", "resume", "permission-mode", "toolset", "effort", "rollover", "image", "trust", "daemon", "sandbox"] {
            if name == "model" && new {
                continue;
            }
            if f.given.contains(name) && !f.print {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `gruntas -p`")));
            }
        }
        // A budget holds a -p session or is the one `sessions budget` checks; anywhere else it would
        // hold nothing.
        let budget = words.starts_with(&["sessions", "budget"]);
        for name in ["max-usd", "max-tokens"] {
            if f.given.contains(name) && !f.print && !budget {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `gruntas -p` and `gruntas sessions budget`")));
            }
        }
        let add = words.starts_with(&["providers", "add"]);
        let connect = words.first() == Some(&"connect");
        // `--name` also names this machine where sync sets it up or
        // registers it: what the workspace's device list calls it.
        let names_device = matches!(words.as_slice(), ["sync", "init" | "recover" | "join", ..]);
        if f.given.contains("name") && !add && !connect && !names_device {
            return Err(fail("bad_flag", "`--name` is only a flag of `gruntas connect`, `gruntas providers add`, and `gruntas sync`"));
        }
        for name in ["api-key-env", "base-url", "binary", "config-dir"] {
            if f.given.contains(name) && !add && !connect {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `gruntas connect` and `gruntas providers add`")));
            }
        }
        let kit = words.starts_with(&["sync", "init"]) || words.starts_with(&["sync", "recovery", "new"]) || words.starts_with(&["machines", "recovery", "new"]);
        if f.given.contains("save") && !kit {
            return Err(fail("bad_flag", "`--save` is only a flag of `gruntas sync init` and `gruntas machines recovery new`"));
        }
        if f.given.contains("to") && !words.starts_with(&["worktrees", "apply"]) {
            return Err(fail("bad_flag", "`--to` is only a flag of `gruntas worktrees apply`"));
        }
        if f.given.contains("start-over") && !words.starts_with(&["sync", "init"]) {
            return Err(fail("bad_flag", "`--start-over` is only a flag of `gruntas sync init`"));
        }
        let mcp_add = words.starts_with(&["mcp", "add"]);
        if f.given.contains("force") && !words.starts_with(&["host", "stop"]) && !words.starts_with(&["worktrees", "remove"]) && !mcp_add {
            return Err(fail("bad_flag", "`--force` is only a flag of `gruntas host stop`, `gruntas worktrees remove` and `gruntas mcp add`"));
        }
        for name in ["env", "header"] {
            if f.given.contains(name) && !mcp_add {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `gruntas mcp add`")));
            }
        }
        for name in ["account", "unattended", "no-sandbox", "no-plugin", "passthrough"] {
            if f.given.contains(name) && words.first() != Some(&"run") {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `gruntas run`, after the harness: `gruntas run claude --{name} …`")));
            }
        }
        let trusting = words.first() == Some(&"trust") && !f.print;
        for name in ["list", "revoke"] {
            if f.given.contains(name) && !trusting {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `gruntas trust`")));
            }
        }
        let owners = [("device", "`gruntas providers add` (`gruntas connect` takes --method device)", add), ("method", "`gruntas connect`", connect), ("default", "`gruntas connect`", connect), ("key-stdin", "`gruntas connect`", connect), ("key-ref", "`gruntas connect`", connect), ("remove", "`gruntas disconnect`", words.first() == Some(&"disconnect")), ("sign-out-vendor", "`gruntas disconnect`", words.first() == Some(&"disconnect")), ("check", "`gruntas providers list`", matches!(words.as_slice(), ["providers"] | ["providers", "list", ..])), ("tried", "`gruntas providers next`", words.starts_with(&["providers", "next"]))];
        for (name, owner, allowed) in owners {
            if f.given.contains(name) && !allowed {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of {owner}")));
            }
        }
    }
    for (name, owner, allowed) in owners {
        if f.given.contains(name) && !allowed {
            return Err(fail("bad_flag", format!("`--{name}` is only a flag of {owner}")));
        }
    }
    let why = if show {
        "`gruntas sessions show` reads one session"
    } else if rebuild {
        "`gruntas sessions rebuild` re-imports everything"
    } else if sync {
        "`gruntas sessions sync` reads everything that changed"
    } else {
        return Ok(());
    };
    if f.given.contains("limit") {
        return Err(fail("bad_flag", format!("`--limit` is only a flag of `gruntas sessions` and `gruntas sessions import` — {why}")));
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
    gruntas_api::truthy(&ctx.env("CI")) || !ctx.env("GITHUB_ACTIONS").is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    #[test]
    fn an_older_command_name_is_routed_as_the_new_one_but_a_prompt_is_left_as_typed() {
        let (f, typed, _) = flags::parse(&words("uploads list"));
        assert_eq!(command_words(&f, &typed), words("artifacts list"));
        #[cfg(feature = "harness")]
        {
            let (f, typed, _) = flags::parse(&words("-p uploads are broken"));
            assert_eq!(command_words(&f, &typed), words("uploads are broken"));
        }
    }

    #[test]
    fn an_unknown_subcommand_is_quoted_as_typed() {
        let e = unknown_command(&words("artifacts bogus"), &words("uploads bogus"));
        assert!(e.fix().starts_with("`uploads bogus` is not a gruntas command"), "{}", e.fix());
    }
}
