//! The human help and the greeting. The command lists are rendered from the
//! catalog under the headings in `GROUPS`, a command's own help from its
//! entry and its `about`; the topics are what belongs to no one command.

use super::catalog::{self, Catalog, Command, Flag, ALL_ONLY, BOOL, GROUPS};

/// The Krowk mark: the logo's four-by-four grid of squares, two grid rows to a
/// text row so a square stays square.
const MARK: [&str; 2] = ["█  █", "█▀▀▄"];

/// What every page is held to, so nothing wraps on a standard terminal.
const COLUMNS: usize = 80;

/// The greeting's line under the name: the catalog's summary, as a sentence.
#[cfg(feature = "harness")]
const TAGLINE: &str = "A coding agent harness: one session, any machine, model or agent";
#[cfg(not(feature = "harness"))]
const TAGLINE: &str = "Permalinks for agent output";

fn banner(version: &str) -> String {
    format!("\n{}\n{}\n\nKrowk {version}\n{TAGLINE}\n\n", MARK[0], MARK[1])
}

const GREETING_HINTS: &[(&str, &str)] = &[
    ("krowk push screenshot.png", "Upload a file, get a link — no key, lasts a day"),
    ("krowk login --token …", "Add a key: uploads keep, group under runs"),
    ("krowk help", "The commands; --all for every one, --json as data"),
];

/// What `krowk` alone says: the first upload, the key that makes uploads
/// keep, and where the rest is.
pub fn greeting(version: &str) -> String {
    // Unwrapped, as it always was: a hint is one line.
    let mut out = banner(version);
    for (hint, why) in GREETING_HINTS {
        out += &format!("  {hint:<25}  {why}\n");
    }
    out
}

/// Where the files live, which only the running process knows.
pub struct Files<'a> {
    pub credentials: &'a str,
    pub config: &'a str,
}

#[cfg(feature = "harness")]
const USAGE: &[(&str, &str)] =
    &[("krowk", "Open the agent here"), ("krowk -p \"…\"", "One prompt, headless"), ("krowk <command> [flags]", "")];
#[cfg(not(feature = "harness"))]
const USAGE: &[(&str, &str)] = &[("krowk <command> [flags]", ""), ("krowk push shot.png", "Upload a file, get a link")];

/// The flags worth knowing before any command; `krowk help flags` has them all.
const FLAGS: &[(&str, &str)] = &[
    #[cfg(feature = "harness")]
    ("--model <instance/model>", "Which model (e.g. claude/sonnet)"),
    #[cfg(feature = "harness")]
    ("--resume [id]", "Continue a session"),
    ("--json · --jq <expr>", "Output as JSON, or filter it"),
    ("-h, --help · -v, --version", ""),
];

/// What a first command looks like: the one thing each build is for, then
/// the shape of a scripted call.
#[cfg(feature = "harness")]
const EXAMPLES: &[(&str, &str)] = &[
    ("krowk connect anthropic", "Connect a model to run the agent on"),
    ("krowk push shot.png", "Upload a screenshot and get a link to paste"),
    ("krowk sessions --json", "Every agent session on this machine, as data"),
];
#[cfg(not(feature = "harness"))]
const EXAMPLES: &[(&str, &str)] = &[
    ("krowk push shot.png", "Upload a screenshot and get a link to paste"),
    ("krowk push shot.png --json", "The same, as JSON for a script or an agent"),
    ("krowk login", "Add a key, so uploads keep and group under runs"),
];

const MORE: &[(&str, &str)] = &[
    ("krowk help <command>", "A command's own flags"),
    ("krowk help --all", "Every command and subcommand"),
    ("krowk help topics", "Flags, exit codes, environment, workspaces, links"),
    ("krowk help agents", "For AI agents and scripts: the output contract"),
];

const ALL_MORE: &[(&str, &str)] = &[
    ("krowk help <command>", "A command's own flags and explanation"),
    ("krowk help topics", "Flags, exit codes, environment, workspaces, links"),
    ("krowk help agents", "For AI agents and scripts: the output contract"),
    ("krowk help --json", "The whole surface as data, for tooling"),
];

/// `krowk help`: what krowk is, how to start, and one line per command; with
/// `all`, one line per command and subcommand instead.
pub fn help(c: &Catalog, all: bool, colour: bool) -> String {
    let mut out = if all {
        format!("{}\n", bold(colour, "krowk — every command and subcommand"))
    } else {
        format!("{}  {} {}\n{}  {}\n", MARK[0], bold(colour, "Krowk"), c.version, MARK[1], c.summary)
    };
    if !all {
        heading(&mut out, "Usage", colour);
        rows(&mut out, "  ", 27, USAGE);
        heading(&mut out, "Examples", colour);
        rows(&mut out, "  ", 27, EXAMPLES);
    }
    let leaves = c.leaves();
    for (title, names) in GROUPS {
        heading(&mut out, title, colour);
        for name in *names {
            if all {
                for leaf in leaves.iter().filter(|l| l.name.split(' ').next() == Some(name)) {
                    row(&mut out, "  ", 18, &leaf.name, leaf.summary);
                }
            } else if !ALL_ONLY.contains(name) {
                row(&mut out, "  ", 14, name, c.commands.iter().find(|c| c.name == *name).map_or("", |c| c.summary));
            }
        }
    }
    if !all {
        heading(&mut out, "Flags", colour);
        rows(&mut out, "  ", 27, FLAGS);
    }
    out.push('\n');
    rows(&mut out, "", 22, if all { ALL_MORE } else { MORE });
    out.pop();
    out
}

/// What belongs to no one command. `workspaces` is the command's own help,
/// which carries it; the rest are written here.
pub const TOPICS: &[(&str, &str)] = &[
    ("agents", "Driving krowk from an AI agent or a script"),
    ("flags", "The flags every command takes"),
    ("exit-codes", "What each exit code means"),
    ("environment", "Environment variables, and where files live"),
    ("workspaces", "Which stored key a command uses"),
    ("links", "Naming an upload or a run by any link to it"),
];

/// `krowk help topics`.
pub fn topics(colour: bool) -> String {
    let mut out = format!("{}\n", bold(colour, "Topics"));
    rows(&mut out, "  ", 13, TOPICS);
    out + "\nkrowk help <topic>"
}

const EXIT_CODES: &str = "  0  It worked.
  1  The command was wrong, or krowk failed on its own — also anything
     unclassified.
  2  Not found — no such artifact or run in this workspace, or no such
     endpoint.
  3  Refused for want of credentials — no key, a key the registry rejects, a
     browser login somebody denied or that nothing on CI could approve, or no
     claim token where that is the only authority (a claim token the registry
     does not recognise is 2, since it answers that as no such record).
  4  Refused by the registry on the request or the state of things — retrying
     unchanged answers the same; also a session over its `sessions budget`.
  5  Rate limited — wait and retry.
  6  The bytes did not move — the registry or object storage could not be
     reached.
  7  The registry failed on its side, or answered something unreadable — or a
     login page krowk will not open, which is the same news.
  8  Gone — the artifact expired or was taken down, or a browser login lapsed
     before anybody approved it; no retry brings any of them back.";

/// `krowk help agents`: what an agent needs to drive krowk without reading
/// every page — how a result comes back, how a failure does, and which
/// command answers which question. Written for a model's context window.
fn agents(colour: bool) -> String {
    let mut out = String::from(AGENTS_OUTPUT);
    heading(&mut out, "Exit codes", colour);
    out += AGENTS_EXIT;
    heading(&mut out, "Tasks", colour);
    rows(&mut out, "  ", 33, AGENT_TASKS);
    heading(&mut out, "Discover more", colour);
    rows(&mut out, "  ", 33, AGENT_DISCOVER);
    out.pop();
    out
}

const AGENTS_OUTPUT: &str = "\
When stdout is not a terminal, commands answer with one JSON envelope — on
stdout when they worked, on stderr when they failed. Help is text unless
--json is given, --version is always text, and `krowk help --json` marks
the other commands that print text `no_json`. --json asks for the envelope
on a terminal too, --quiet drops it for the bare record, and --jq filters
it with no jq binary needed. Pickers are skipped when piped, under --json
or in CI, failing with a fix instead. A browser login waits for someone to
approve it, and a command that changes your keys asks a person at the
terminal, refusing without one.

  {\"ok\": true, \"data\": {…}, \"summary\": \"…\", \"breadcrumbs\": [{…}]}
  {\"ok\": false, \"error\": {\"error\": \"<code>\", \"fix\": \"…\", \"retryable\": false}}
  breadcrumb: {\"action\": \"…\", \"cmd\": \"krowk …\", \"description\": \"…\"}

Report `summary`. A breadcrumb's `cmd` is a command left to run, and its
`description` says what it does; fill in any <placeholder> first. On a
failure, `fix` says what to change, and `retryable` whether running the same
command again can help. An upload's envelope also carries `paste.markdown`
and `paste.url`, the forms to put in a pull request or a chat; under --quiet
each artifact carries its own `paste`.
";

const AGENTS_EXIT: &str = "  0 ok · 1 the command was wrong · 2 not found · 3 credentials · 4 refused
  5 rate limited · 6 unreachable · 7 registry failed · 8 gone
  Retry on 5 and 6; change the command on 1, 2 and 4. `krowk help exit-codes`
  says more.
";

const AGENT_TASKS: &[(&str, &str)] = &[
    ("krowk push <file> --json", "Publish a file; paste its .paste.markdown"),
    ("krowk runs start --title \"…\"", "Open a run for several pushes (needs a key)"),
    ("krowk push <file> --run <run>", "Add a file to that run"),
    ("krowk whoami", "Which key and workspace are in use"),
    #[cfg(feature = "harness")]
    ("krowk -p \"<prompt>\" --output-format json", "Run one prompt headless; the result as JSON"),
    #[cfg(feature = "harness")]
    ("krowk status", "Which models are connected and ready"),
    #[cfg(feature = "sessions")]
    ("krowk sessions", "Agent sessions on this machine, newest first"),
    ("krowk doctor", "What is wrong with the setup"),
];

const AGENT_DISCOVER: &[(&str, &str)] = &[
    ("krowk help --json", "Every command, argument and flag, as data"),
    ("krowk help <command> --json", "One command's arguments and flags"),
    ("krowk help <command>", "The same, with what the command is for"),
];

const LINKS: &str = "\
Wherever an artifact or a run is named — a positional, or --run — a link that
carries it does just as well: the card page, the CDN URL under it, or anything
else krowk printed. A link carrying no slug of the kind the command wants, or
two different ones, is refused before anything is sent.";

/// A topic's page, or None for a name that is not one.
pub fn topic(name: &str, c: &Catalog, files: &Files, colour: bool) -> Option<String> {
    let (_, summary) = TOPICS.iter().find(|(n, _)| *n == name)?;
    let mut out = format!("{}\n", bold(colour, &format!("krowk help {name} — {summary}")));
    match name {
        "agents" => out += &format!("\n{}\n", agents(colour)),
        "flags" => {
            heading(&mut out, "Global flags", colour);
            flag_rows(&mut out, 0, &c.global_flags[..catalog::CORE_FLAGS]);
            #[cfg(feature = "harness")]
            {
                heading(&mut out, "Agent flags (krowk, krowk -p)", colour);
                flag_rows(&mut out, 0, &c.global_flags[catalog::CORE_FLAGS..]);
            }
        }
        "exit-codes" => {
            heading(&mut out, "Exit codes", colour);
            out += &format!("{EXIT_CODES}\n");
        }
        "environment" => {
            heading(&mut out, "Environment", colour);
            for e in &c.environment {
                let why = if e.default.is_empty() { e.usage.to_string() } else { format!("{} (default {})", e.usage, e.default) };
                row(&mut out, "  ", 23, e.name, &why);
            }
            out += &format!(
                "\nWhich registry: --dev, then KROWK_API_URL, then KROWK_DEV, then the default.\n\n{}\n  Credentials live in {} (0600).\n  Everything else krowk keeps is beside them: ~/.krowk, or KROWK_HOME.\n  Config lives in {}, and per repository in\n  <git-root>/.krowk/config.json.\n",
                bold(colour, "Files"),
                files.credentials,
                files.config,
            );
        }
        "links" => out += &format!("\n{LINKS}\n"),
        _ => return None,
    }
    out.pop();
    Some(out)
}

fn heading(out: &mut String, title: &str, colour: bool) {
    out.push('\n');
    out.push_str(&bold(colour, title));
    out.push('\n');
}

fn bold(colour: bool, s: &str) -> String {
    crate::output::paint(colour, crate::output::BOLD, s)
}

/// Rows of a label and what it says, the text starting `width` columns in.
fn rows(out: &mut String, indent: &str, width: usize, rows: &[(&str, &str)]) {
    for (label, why) in rows {
        row(out, indent, width, label, why);
    }
}

/// One label and what it means, the meaning wrapped under itself so no line
/// passes 80 columns; a label too wide for its column gets a line of its own.
fn row(out: &mut String, indent: &str, width: usize, label: &str, text: &str) {
    if text.is_empty() {
        *out += &format!("{indent}{label}\n");
        return;
    }
    let mut label = label;
    if label.chars().count() + 2 > width {
        wrap(out, indent, indent.len() + 4, label);
        label = "";
    }
    wrap(out, &format!("{indent}{label:<width$}"), indent.len() + width, text);
}

/// `text` on lines of at most 80 columns, the first after `first` and the
/// rest `hang` columns in.
fn wrap(out: &mut String, first: &str, hang: usize, text: &str) {
    out.push_str(first);
    let mut used = first.chars().count();
    let mut fresh = true;
    for word in text.split(' ') {
        let wide = word.chars().count();
        if !fresh && used + 1 + wide > COLUMNS {
            out.push('\n');
            for _ in 0..hang {
                out.push(' ');
            }
            used = hang;
            fresh = true;
        }
        if !fresh {
            out.push(' ');
            used += 1;
        }
        out.push_str(word);
        used += wide;
        fresh = false;
    }
    out.push('\n');
}

fn flag_rows(out: &mut String, width: usize, flags: &[Flag]) {
    let width = flags.iter().map(|f| flag_label(f).chars().count() + 2).max().unwrap_or(0).clamp(width, 28);
    for f in flags {
        row(out, "  ", width, &flag_label(f), &f.usage);
    }
}

/// One command's own help: the catalog read back as text.
pub fn command_help(cmd: &Command, globals: &[Flag], colour: bool) -> String {
    // The title and the `about` are held under 80 columns by the tests below;
    // only a usage runs long enough to need wrapping.
    let mut out = format!("{}\n", bold(colour, &format!("krowk {} — {}", cmd.name, cmd.summary)));
    if !cmd.usage.is_empty() {
        heading(&mut out, "Usage", colour);
        wrap(&mut out, "  ", 6, cmd.usage);
    }
    let about = catalog::about(&cmd.name);
    if !about.is_empty() {
        out += &format!("\n{about}\n");
    }
    if !cmd.subcommands.is_empty() {
        // Each under the group's name, `krowk artifacts` read as said.
        heading(&mut out, &format!("Commands (krowk {} …)", cmd.name), colour);
        let prefix = format!("krowk {} ", cmd.name);
        let width = cmd.subcommands.iter().map(|s| s.usage.len() - prefix.len() + 2).max().unwrap_or(0).min(28);
        for s in &cmd.subcommands {
            row(&mut out, "  ", width, s.usage.get(prefix.len()..).unwrap_or(s.usage), s.summary);
        }
    }
    let labels: Vec<String> = cmd.args.iter().map(arg_label).collect();
    let width = labels.iter().map(|l| l.len() + 2).chain(cmd.flags.iter().map(|f| flag_label(f).chars().count() + 2)).max().unwrap_or(0).min(28);
    if !cmd.args.is_empty() {
        heading(&mut out, "Arguments", colour);
        for (label, a) in labels.iter().zip(&cmd.args) {
            row(&mut out, "  ", width, label, a.summary);
        }
    }
    if !cmd.flags.is_empty() {
        heading(&mut out, "Flags", colour);
        flag_rows(&mut out, width, &cmd.flags);
    }
    // Every command takes them, so they are named here and explained once.
    out += "\nGlobal flags:";
    for f in globals.iter().filter(|f| f.aliases.is_empty()) {
        out += &format!(" --{}", f.name);
    }
    out + " — see `help flags`"
}

fn arg_label(a: &super::catalog::Arg) -> String {
    let label = if a.required { format!("<{}>", a.name) } else { format!("[{}]", a.name) };
    if a.repeated { label.trim_end_matches('>').to_string() + "...>" } else { label }
}

fn flag_label(f: &Flag) -> String {
    let mut label = format!("--{}", f.name);
    for alias in &f.aliases {
        label += &format!(", -{alias}");
    }
    if f.kind == BOOL {
        return label;
    }
    label += &format!(" <{}>", f.kind);
    if !f.default.is_empty() {
        label += &format!(" (default {})", f.default);
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fits(page: &str) {
        for line in page.lines() {
            assert!(line.chars().count() <= COLUMNS, "over {COLUMNS} columns: {line:?}");
        }
    }

    #[test]
    fn the_overview_fits_on_one_screen() {
        let page = help(&catalog::catalog("0.11.0-rc.1"), false, false);
        assert!(page.lines().count() <= 52, "{} lines:\n{page}", page.lines().count());
        fits(&page);
    }

    #[test]
    fn help_all_lists_every_command_the_build_has_on_one_line_each() {
        let c = catalog::catalog("dev");
        let page = help(&c, true, false);
        fits(&page);
        for leaf in c.leaves() {
            let built = cfg!(feature = "sessions") || !matches!(leaf.name.split(' ').next(), Some("sessions" | "pricing"));
            let listed = page.lines().any(|l| l.trim_start().starts_with(&format!("{}  ", leaf.name)));
            assert_eq!(listed, built, "{}", leaf.name);
        }
    }

    #[test]
    fn every_topic_has_a_page_and_every_page_fits() {
        let c = catalog::catalog("dev");
        let files = Files { credentials: "/home/me/.config/krowk/credentials.json", config: "/home/me/.config/krowk/config.json" };
        let globals = &c.global_flags[..catalog::CORE_FLAGS];
        for (name, _) in TOPICS {
            let page = topic(name, &c, &files, false).or_else(|| c.find(&[name.to_string()]).map(|cmd| command_help(&cmd, globals, false)));
            fits(&page.unwrap_or_else(|| panic!("no page for {name}")));
        }
        fits(&topics(false));
        fits(&greeting("0.11.0-rc.1"));
        assert!(greeting("dev").contains(&c.summary[1..]));
        // Every command's own page, a group's and a leaf's, as `krowk help` prints it.
        for cmd in c.commands.iter().chain(&c.leaves()) {
            let page = command_help(cmd, globals, false);
            assert!(page.contains("--jq"), "{page}");
            fits(&page);
        }
    }
}
