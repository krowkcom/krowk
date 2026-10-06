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
mod chain;
#[cfg(feature = "harness")]
mod devices;
#[cfg(all(feature = "harness", unix))]
mod host;
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
mod tui;
mod upgrade;
mod workspace;
#[cfg(feature = "harness")]
mod worktrees;

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
pub fn run(args: &[String], io: &mut Io) -> i32 {
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
    // A bare `--resume` opens the session picker in the TUI; anywhere else
    // it is the missing value it always was.
    #[cfg(feature = "harness")]
    let parsed = parsed.and_then(|()| match f.resume_pick && !tui::wanted(io, &f, format, &positionals, jq_given) {
        true => Err("--resume needs a value".to_string()),
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
    // A home krowk cannot use — a KROWK_HOME that is not absolute, a
    // symlink or another user's directory in its place, an older layout
    // that could not be moved in — is refused before anything runs, rather
    // than read around. One `lstat` when it is there; none is fine (a
    // container's anonymous push needs no home).
    if let Err(e) = krowk_api::home::dir(io.env)
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
    // `krowk --resume <id>` for a session another machine runs: attach it
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
                report(ctx.io, &e, format, quiet, err_colour, None)
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

/// Whether this is `krowk -p`, whose words are a prompt rather than a command.
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
        // Your krowk account, short: `krowk connect` is a model provider.
        ["login", ..] => auth::login(ctx, rest(1)),
        ["logout", ..] => auth::logout(ctx, rest(1)),
        ["whoami", ..] => auth::verify(ctx),
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
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "archive", ..] => sessions::archive(ctx, rest(2)),
        #[cfg(all(feature = "harness", unix))]
        ["sessions", "restore", ..] => sessions::restore(ctx, rest(2)),
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
        ["providers"] | ["providers", "list", ..] => providers::list(ctx),
        #[cfg(feature = "harness")]
        ["providers", "remove", ..] => providers::remove(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["providers", "rename", ..] => providers::rename(ctx, rest(2)),
        #[cfg(feature = "harness")]
        ["status", ..] => status::status(ctx),
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
        ["hosts", ..] => synced::hosts(ctx),
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
        #[cfg(feature = "harness")]
        ["worktrees"] | ["worktrees", "list", ..] => worktrees::list(ctx),
        #[cfg(feature = "harness")]
        ["worktrees", "remove", ..] => worktrees::remove(ctx, rest(2)),
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

/// A command krowk does not have, with the nearest one it does when the
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
            Some(near) => fail("unknown_command", format!("`{typed}` is not a krowk command — did you mean `krowk {near}`?")),
            None => fail("unknown_command", format!("`{typed}` is not a krowk command — run `krowk --help`")),
        };
    };
    // A group alone, or with a subcommand it lacks: name the ones it has.
    let subs: Vec<&str> = group.subcommands.iter().map(|s| s.name.as_str()).collect();
    let near = p.get(1).and_then(|w| suggest::closest(w, subs.iter().copied()));
    match (p.get(1), near) {
        (Some(_), Some(near)) => fail("unknown_command", format!("`{typed}` is not a krowk command — did you mean `krowk {} {near}`?", group.name)),
        (Some(_), None) => fail(
            "unknown_command",
            format!("`{typed}` is not a krowk command; the {} command takes {}; run `krowk help {}`", group.name, either(&subs), group.name),
        ),
        (None, _) => fail("unknown_command", format!("the {} command needs a subcommand: {}; run `krowk help {}`", group.name, either(&subs), group.name)),
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
        Some(cmd) => format!("krowk help {}", cmd.name),
        None => "krowk --help".into(),
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

fn show_help(ctx: &mut Ctx, topic: &[String]) -> Result<(), Error> {
    let c = catalog::catalog(VERSION);
    // `krowk help uploads` is the help of what `uploads` now is.
    let (typed, topic) = (topic, &catalog::canonical(topic));
    // Help is read, by a person or an agent, so it is the page unless JSON was
    // asked for by name: piped, the whole catalog would bury the overview.
    let asked_for_json = ctx.f.json || ctx.filter.is_some() || ctx.f.format == "json";
    let human = ctx.format != Format::Json || !asked_for_json;
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
    let (credentials, config) = (krowk_api::creds::credentials_text(), crate::config::global_text());
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
            Some(near) => fail("unknown_command", format!("`{typed}` is not a krowk command or help topic — did you mean `krowk help {near}`?")),
            None => fail("unknown_command", format!("`{typed}` is not a krowk command or help topic — run `krowk help`")),
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
const ALL_OWNERS: &str = "`krowk sessions` and `krowk help`";
#[cfg(not(feature = "sessions"))]
const ALL_OWNERS: &str = "`krowk help`";

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
        ("all", ALL_OWNERS, list),
        ("thinking", "`krowk sessions show`", show),
        ("older-than", "`krowk sessions archive`", words.starts_with(&["sessions", "archive"])),
        ("weekly", "`krowk sessions archive`", words.starts_with(&["sessions", "archive"])),
        ("yes", "`krowk sessions rebuild`", rebuild),
        ("no-network", "`krowk sessions sync`", sync),
    ];
    #[cfg(feature = "harness")]
    {
        for name in ["output-format", "model", "resume", "permission-mode", "toolset", "effort", "trust", "daemon", "sandbox"] {
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
        let connect = words.first() == Some(&"connect");
        // `--name` also names this machine where sync sets it up or
        // registers it: what the workspace's device list calls it.
        let names_device = matches!(words.as_slice(), ["sync", "init" | "recover" | "join", ..]);
        if f.given.contains("name") && !add && !connect && !names_device {
            return Err(fail("bad_flag", "`--name` is only a flag of `krowk connect`, `krowk providers add`, and `krowk sync`"));
        }
        for name in ["api-key-env", "base-url", "client-id", "binary", "config-dir"] {
            if f.given.contains(name) && !add && !connect {
                return Err(fail("bad_flag", format!("`--{name}` is only a flag of `krowk connect` and `krowk providers add`")));
            }
        }
        let kit = words.starts_with(&["sync", "init"]) || words.starts_with(&["sync", "recovery", "new"]);
        if f.given.contains("save") && !kit {
            return Err(fail("bad_flag", "`--save` is only a flag of `krowk sync init` and `krowk sync recovery new`"));
        }
        if f.given.contains("start-over") && !words.starts_with(&["sync", "init"]) {
            return Err(fail("bad_flag", "`--start-over` is only a flag of `krowk sync init`"));
        }
        if f.given.contains("force") && !words.starts_with(&["host", "stop"]) && !words.starts_with(&["worktrees", "remove"]) {
            return Err(fail("bad_flag", "`--force` is only a flag of `krowk host stop` and `krowk worktrees remove`"));
        }
        let owners = [("device", "`krowk providers add` (`krowk connect` takes --method device)", add), ("method", "`krowk connect`", connect), ("default", "`krowk connect`", connect), ("key-stdin", "`krowk connect`", connect), ("key-ref", "`krowk connect`", connect), ("remove", "`krowk disconnect`", words.first() == Some(&"disconnect")), ("sign-out-vendor", "`krowk disconnect`", words.first() == Some(&"disconnect"))];
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
        assert!(e.fix().starts_with("`uploads bogus` is not a krowk command"), "{}", e.fix());
    }
}
