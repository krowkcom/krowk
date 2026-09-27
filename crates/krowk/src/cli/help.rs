//! The human help and the greeting. The command lists are rendered from the
//! catalog under the headings in `GROUPS`, a command's own help from its
//! entry and its `about`; the topics are what belongs to no one command.

use super::catalog::{self, Catalog, Command, Flag, ALL_ONLY, BOOL, GROUPS};

/// The Krowk mark: the logo's four-by-four grid of squares, two grid rows to a
/// text row so a square stays square.
const MARK: [&str; 2] = ["█  █", "█▀▀▄"];

/// What every page is held to, so nothing wraps on a standard terminal.
const COLUMNS: usize = 80;

fn banner(version: &str) -> String {
    format!("\n{}\n{}\n\nKrowk {version}\nPermalinks for agent output\n\n", MARK[0], MARK[1])
}

const GREETING_HINTS: &[(&str, &str)] = &[
    ("krowk push screenshot.png", "Upload a file and get a link — no key needed, lasts a day"),
    ("krowk login --token …", "Add a key: uploads keep, group under runs, and stay yours"),
    ("krowk help", "The commands; --all for every one, --json as data"),
];

/// What `krowk` alone says: the first upload, the key that makes uploads
/// keep, and where the rest is.
pub fn greeting(version: &str) -> String {
    format!("{}{}\n", banner(version), hints("  ", GREETING_HINTS).join("\n"))
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

/// `krowk help`: what krowk is, how to start, and one line per command.
pub fn help(c: &Catalog) -> String {
    let groups = GROUPS.iter().map(|(title, names)| {
        let rows: Vec<String> = names
            .iter()
            .filter(|n| !ALL_ONLY.contains(n))
            .map(|n| format!("  {n:<12}  {}", c.commands.iter().find(|c| c.name == *n).map_or("", |c| c.summary)))
            .collect();
        format!("{title}\n{}", rows.join("\n"))
    });
    let blocks: Vec<String> = [format!("{}  krowk — {}\n{}  version {}", MARK[0], c.summary, MARK[1], c.version)]
        .into_iter()
        .chain([format!("USAGE\n{}", hints_at("  ", 25, USAGE).join("\n"))])
        .chain(groups)
        .chain([format!("FLAGS\n{}", hints_at("  ", 25, FLAGS).join("\n")), hints("", MORE).join("\n")])
        .collect();
    blocks.join("\n\n")
}

/// `krowk help --all`: every command and subcommand, one line each.
pub fn help_all(c: &Catalog) -> String {
    let leaves = c.leaves();
    let groups: Vec<(&str, Vec<&Command>)> = GROUPS
        .iter()
        .map(|(title, names)| {
            let rows = names.iter().flat_map(|n| leaves.iter().filter(move |l| l.name.split(' ').next() == Some(n)));
            (*title, rows.collect())
        })
        .collect();
    let width = groups.iter().flat_map(|(_, rows)| rows.iter()).map(|l| l.name.len()).max().unwrap_or(0);
    let mut blocks = vec!["krowk — every command and subcommand".to_string()];
    blocks.extend(groups.iter().map(|(title, rows)| {
        let rows: Vec<String> = rows.iter().map(|l| format!("  {:<width$}  {}", l.name, l.summary)).collect();
        format!("{title}\n{}", rows.join("\n"))
    }));
    blocks.push(
        hints(
            "",
            &[
                ("krowk help <command>", "A command's own flags and explanation"),
                ("krowk help flags", "The flags every command takes"),
                ("krowk help topics", "Exit codes, environment, workspaces, links"),
                ("krowk help --json", "The whole surface as data, for tooling"),
            ],
        )
        .join("\n"),
    );
    blocks.join("\n\n")
}

/// What belongs to no one command. `workspaces` is the command's own help,
/// which carries it; the rest are written here.
pub const TOPICS: &[(&str, &str)] = &[
    #[cfg(feature = "harness")]
    ("flags", "The flags every command takes, and the agent's"),
    #[cfg(not(feature = "harness"))]
    ("flags", "The flags every command takes"),
    ("exit-codes", "What each exit code means"),
    ("environment", "Environment variables, and where files live"),
    ("workspaces", "Which stored key a command uses"),
    ("links", "Naming an upload or a run by any link to it"),
];

/// `krowk help topics`.
pub fn topics() -> String {
    format!("TOPICS\n{}\n\nkrowk help <topic>", hints("  ", TOPICS).join("\n"))
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
    let body = match name {
        "flags" => flags_topic(),
        "exit-codes" => format!("EXIT CODES\n{EXIT_CODES}"),
        "environment" => {
            let rows: Vec<(String, String)> = c
                .environment
                .iter()
                .map(|e| {
                    let why = if e.default.is_empty() { e.usage.to_string() } else { format!("{} (default {})", e.usage, e.default) };
                    (e.name.to_string(), why)
                })
                .collect();
            format!(
                "ENVIRONMENT\n{}\n\nWhich registry: --dev, then KROWK_API_URL, then KROWK_DEV, then the default.\n\nFILES\n  Credentials live in {} (0600).\n  Config lives in {}, and per repository in\n  <git-root>/.krowk/config.json.",
                table(&rows, 0).join("\n"),
                files.credentials,
                files.config,
            )
        }
        "links" => LINKS.to_string(),
        _ => return None,
    };
    Some(format!("krowk help {name} — {}\n\n{body}", lowered(summary)))
}

fn flags_topic() -> String {
    let rows = |flags: &[Flag]| table(&flags.iter().map(|f| (flag_label(f), f.usage.clone())).collect::<Vec<_>>(), 0).join("\n");
    #[allow(unused_mut)]
    let mut out = format!("GLOBAL FLAGS\n{}", rows(&catalog::core_flags()));
    #[cfg(feature = "harness")]
    out.push_str(&format!(
        "\n\nAGENT FLAGS\n  Bare `krowk` opens the agent here; `krowk -p \"…\"` runs one prompt headless.\n\n{}",
        rows(&catalog::prompt_flags())
    ));
    out
}

/// Rows of a label and what it says, the labels in one column.
fn hints(indent: &str, rows: &[(&str, &str)]) -> Vec<String> {
    let width = rows.iter().filter(|(_, why)| !why.is_empty()).map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    hints_at(indent, width, rows)
}

fn hints_at(indent: &str, width: usize, rows: &[(&str, &str)]) -> Vec<String> {
    rows.iter().map(|(l, why)| format!("{indent}{l:<width$}  {why}").trim_end().to_string()).collect()
}

/// Rows of a label and what it means, the meaning wrapped under itself so no
/// line passes 80 columns. `width` is the label column's, at least; a label
/// too long for the column gets a line of its own.
fn table(rows: &[(String, String)], width: usize) -> Vec<String> {
    const MOST: usize = 28;
    let width = rows.iter().map(|(l, _)| l.chars().count() + 2).max().unwrap_or(0).max(width).min(MOST);
    let mut out = Vec::new();
    for (label, why) in rows {
        let mut label = label.as_str();
        if label.chars().count() + 2 > width {
            out.push(format!("  {label}"));
            label = "";
        }
        for line in wrap(why, COLUMNS - 2 - width) {
            out.push(format!("  {label:<width$}{line}").trim_end().to_string());
            label = "";
        }
    }
    out
}

fn wrap(text: &str, room: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split(' ') {
        let line = lines.last_mut().expect("never empty");
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > room {
            lines.push(word.to_string());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

fn lowered(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_lowercase().collect::<String>()).unwrap_or_default() + chars.as_str()
}

/// One command's own help: the catalog read back as text.
pub fn command_help(cmd: &Command, globals: &[Flag]) -> String {
    let mut lines = vec![format!("krowk {} — {}", cmd.name, lowered(cmd.summary))];
    if !cmd.usage.is_empty() {
        lines.extend(["".into(), "USAGE".into(), format!("  {}", cmd.usage)]);
    }
    if !cmd.about.is_empty() {
        lines.extend(["".into(), cmd.about.into()]);
    }
    if !cmd.subcommands.is_empty() {
        // Each under the group's name, `krowk uploads` read as said.
        let prefix = format!("krowk {} ", cmd.name);
        let rows: Vec<(String, String)> =
            cmd.subcommands.iter().map(|s| (s.usage.trim_start_matches(&prefix).to_string(), s.summary.to_string())).collect();
        lines.extend(["".into(), format!("COMMANDS (krowk {} …)", cmd.name)]);
        lines.extend(table(&rows, 0));
    }
    let args: Vec<(String, String)> = cmd.args.iter().map(|a| (arg_label(a), a.summary.to_string())).collect();
    let flags = |flags: &[Flag]| flags.iter().map(|f| (flag_label(f), f.usage.clone())).collect::<Vec<_>>();
    let own = flags(&cmd.flags);
    let width = args.iter().chain(&own).map(|(l, _)| l.chars().count() + 2).max().unwrap_or(0);
    for (title, rows) in [("ARGUMENTS", &args), ("FLAGS", &own)] {
        if !rows.is_empty() {
            lines.extend(["".into(), title.into()]);
            lines.extend(table(rows, width));
        }
    }
    // Every command takes them, so they are named here and explained once.
    let names: Vec<String> = globals.iter().filter(|f| f.aliases.is_empty()).map(|f| format!("--{}", f.name)).collect();
    lines.extend(["".into(), format!("Global flags: {} — `krowk help flags`", names.join(" "))]);
    lines.join("\n")
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
        let page = help(&catalog::catalog("0.11.0-rc.1"));
        assert!(page.lines().count() <= 45, "{} lines:\n{page}", page.lines().count());
        fits(&page);
    }

    #[test]
    fn help_all_lists_every_command_the_build_has_on_one_line_each() {
        let c = catalog::catalog("dev");
        let page = help_all(&c);
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
        for (name, _) in TOPICS {
            let page = topic(name, &c, &files).or_else(|| c.find(&[name.to_string()]).map(|cmd| command_help(&cmd, &[])));
            fits(&page.unwrap_or_else(|| panic!("no page for {name}")));
        }
        fits(&topics());
    }
}
