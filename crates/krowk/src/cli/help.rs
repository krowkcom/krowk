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
const TAGLINE: &str = "A coding agent, and permalinks for its output";
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

const MORE: &[(&str, &str)] = &[
    ("krowk help <command>", "A command's own flags"),
    ("krowk help --all", "Every command and subcommand"),
    ("krowk help topics", "flags, exit codes, environment, workspaces, links"),
];

const ALL_MORE: &[(&str, &str)] = &[
    ("krowk help <command>", "A command's own flags and explanation"),
    ("krowk help topics", "flags, exit codes, environment, workspaces, links"),
    ("krowk help --json", "The whole surface as data, for tooling"),
];

/// `krowk help`: what krowk is, how to start, and one line per command; with
/// `all`, one line per command and subcommand instead.
pub fn help(c: &Catalog, all: bool) -> String {
    let mut out = if all {
        "krowk — every command and subcommand\n".to_string()
    } else {
        format!("{}  krowk — {}\n{}  version {}\n\nUSAGE\n", MARK[0], c.summary, MARK[1], c.version)
    };
    if !all {
        rows(&mut out, "  ", 27, USAGE);
    }
    let leaves = c.leaves();
    for (title, names) in GROUPS {
        heading(&mut out, title);
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
        out += "\nFLAGS\n";
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
    ("flags", "The flags every command takes"),
    ("exit-codes", "What each exit code means"),
    ("environment", "Environment variables, and where files live"),
    ("workspaces", "Which stored key a command uses"),
    ("links", "Naming an upload or a run by any link to it"),
];

/// `krowk help topics`.
pub fn topics() -> String {
    let mut out = "TOPICS\n".to_string();
    rows(&mut out, "  ", 13, TOPICS);
    out + "\nkrowk help <topic>"
}

const EXIT_CODES: &str = "  0  it worked
  1  the command was wrong, or krowk failed on its own — also anything
     unclassified
  2  not found — no such artifact or run in this workspace, or no such endpoint
  3  refused for want of credentials — no key, a key the registry rejects, a
     browser login somebody denied or that nothing on CI could approve, or no
     claim token where that is the only authority (a claim token the registry
     does not recognise is 2, since it answers that as no such record)
  4  refused by the registry on the request or the state of things — retrying
     unchanged answers the same; also a session over its `sessions budget`
  5  rate limited — wait and retry
  6  the bytes did not move — the registry or object storage could not be
     reached
  7  the registry failed on its side, or answered something unreadable — or a
     login page krowk will not open, which is the same news
  8  gone — the artifact expired or was taken down, or a browser login lapsed
     before anybody approved it; no retry brings any of them back";

const LINKS: &str = "\
Wherever an artifact or a run is named — a positional, or --run — a link that
carries it does just as well: the card page, the CDN URL under it, or anything
else krowk printed. A link carrying no slug of the kind the command wants, or
two different ones, is refused before anything is sent.";

/// A topic's page, or None for a name that is not one.
pub fn topic(name: &str, c: &Catalog, files: &Files) -> Option<String> {
    let (_, summary) = TOPICS.iter().find(|(n, _)| *n == name)?;
    let mut out = format!("krowk help {name} — {}\n\n", summary);
    match name {
        "flags" => {
            out += "GLOBAL FLAGS\n";
            flag_rows(&mut out, 0, &c.global_flags[..catalog::CORE_FLAGS]);
            #[cfg(feature = "harness")]
            {
                out += "\nAGENT FLAGS (krowk, krowk -p)\n";
                flag_rows(&mut out, 0, &c.global_flags[catalog::CORE_FLAGS..]);
            }
        }
        "exit-codes" => out += &format!("EXIT CODES\n{EXIT_CODES}\n"),
        "environment" => {
            out += "ENVIRONMENT\n";
            for e in &c.environment {
                let why = if e.default.is_empty() { e.usage.to_string() } else { format!("{} (default {})", e.usage, e.default) };
                row(&mut out, "  ", 23, e.name, &why);
            }
            out += &format!(
                "\nWhich registry: --dev, then KROWK_API_URL, then KROWK_DEV, then the default.\n\nFILES\n  Credentials live in {} (0600).\n  Config lives in {}, and per repository in\n  <git-root>/.krowk/config.json.\n",
                files.credentials, files.config,
            );
        }
        "links" => out += &format!("{LINKS}\n"),
        _ => return None,
    }
    out.pop();
    Some(out)
}

fn heading(out: &mut String, title: &str) {
    out.push('\n');
    out.push_str(title);
    out.push('\n');
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
pub fn command_help(cmd: &Command, globals: &[Flag]) -> String {
    // The title and the `about` are held under 80 columns by the tests below;
    // only a usage runs long enough to need wrapping.
    let mut out = format!("krowk {} — {}\n", cmd.name, cmd.summary);
    if !cmd.usage.is_empty() {
        out += "\nUSAGE\n";
        wrap(&mut out, "  ", 6, cmd.usage);
    }
    let about = catalog::about(&cmd.name);
    if !about.is_empty() {
        out += &format!("\n{about}\n");
    }
    if !cmd.subcommands.is_empty() {
        // Each under the group's name, `krowk uploads` read as said.
        out += &format!("\nCOMMANDS (krowk {} …)\n", cmd.name);
        let prefix = format!("krowk {} ", cmd.name);
        let width = cmd.subcommands.iter().map(|s| s.usage.len() - prefix.len() + 2).max().unwrap_or(0).min(28);
        for s in &cmd.subcommands {
            row(&mut out, "  ", width, s.usage.get(prefix.len()..).unwrap_or(s.usage), s.summary);
        }
    }
    let labels: Vec<String> = cmd.args.iter().map(arg_label).collect();
    let width = labels.iter().map(|l| l.len() + 2).chain(cmd.flags.iter().map(|f| flag_label(f).chars().count() + 2)).max().unwrap_or(0).min(28);
    if !cmd.args.is_empty() {
        out += "\nARGUMENTS\n";
        for (label, a) in labels.iter().zip(&cmd.args) {
            row(&mut out, "  ", width, label, a.summary);
        }
    }
    if !cmd.flags.is_empty() {
        out += "\nFLAGS\n";
        flag_rows(&mut out, width, &cmd.flags);
    }
    // Every command takes them, so they are named here and explained once.
    out += "\nGlobal flags:";
    for f in globals.iter().filter(|f| f.aliases.is_empty()) {
        out += &format!(" --{}", f.name);
    }
    out + " — `help flags`"
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
        let page = help(&catalog::catalog("0.11.0-rc.1"), false);
        assert!(page.lines().count() <= 45, "{} lines:\n{page}", page.lines().count());
        fits(&page);
    }

    #[test]
    fn help_all_lists_every_command_the_build_has_on_one_line_each() {
        let c = catalog::catalog("dev");
        let page = help(&c, true);
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
            let page = topic(name, &c, &files).or_else(|| c.find(&[name.to_string()]).map(|cmd| command_help(&cmd, globals)));
            fits(&page.unwrap_or_else(|| panic!("no page for {name}")));
        }
        fits(&topics());
        fits(&greeting("0.11.0-rc.1"));
        assert!(greeting("dev").contains(&c.summary[1..]));
        // Every command's own page, a group's and a leaf's, as `krowk help` prints it.
        for cmd in c.commands.iter().chain(&c.leaves()) {
            let page = command_help(cmd, globals);
            assert!(page.contains("--jq"), "{page}");
            fits(&page);
        }
    }
}
