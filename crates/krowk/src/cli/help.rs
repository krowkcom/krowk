//! The human help and the greeting. The command list is rendered from the
//! catalog under the headings in `SECTIONS`; everything after it is prose,
//! which is the half a struct has no way to say.

use super::catalog::{Catalog, Command, Flag, BOOL, SECTIONS};

/// The Krowk mark: the logo's four-by-four grid of squares, two grid rows to a
/// text row so a square stays square.
const MARK: &str = "█  █\n█▀▀▄";

fn banner(version: &str) -> String {
    format!("\n{MARK}\n\nKrowk {version}\nPermalinks for agent output\n\n")
}

const GREETING_HINTS: &[(&str, &str)] = &[
    ("krowk push screenshot.png", "Upload a file and get a link — no key needed, lasts a day"),
    ("krowk login --token …", "Add a key: uploads keep, group under runs, and stay yours"),
    ("krowk help", "Every command and flag — add --json for the surface as data"),
];

const LEARN_MORE: &[(&str, &str)] = &[
    ("krowk help <command>", "One command's own arguments and flags"),
    ("krowk help --json", "The whole surface as data, for tooling"),
    ("krowk doctor", "Check this machine's setup and reach the registry"),
];

/// What `krowk` alone says: the first upload, the key that makes uploads
/// keep, and where the rest is.
pub fn greeting(version: &str) -> String {
    format!("{}{}\n", banner(version), hint_block(GREETING_HINTS))
}

const HELP: &str = r#"USAGE
  krowk <command> [flags]
  krowk push shot.png     Upload a file, get a link

{commands}

UPLOAD FLAGS
  --run <slug|link>      Attach to an existing run instead of opening one, by
                         slug or by any link carrying one. On
                         `claim` and `uploads attach` it names the run an upload
                         already made joins — a claimed upload has none otherwise
  --pull-request <url>   Pull request the work belongs to
  --link <url>           Link this work is about — the issue, the spec, the
                         discussion. Repeat for more than one, up to 20, and
                         label or classify each with the two flags below
  --link-title <text>    What to call the --link before it, instead of its URL
  --link-rel <kind>      What the --link before it is: tracks, fixes, spec,
                         discussion, source, supersedes — or your own word
  --reference <id>       Related identifier that is not a URL, e.g. a ticket
                         key — repeat for more than one. A URL is a --link
  --title <text>         Title for the work, recorded on the run. What a pasted
                         link says about a file is that file's --caption
  --caption <text>       What this file shows, recorded on the artifact as
                         `krowk.caption` and used wherever it is pasted. Repeat
                         to caption several files in the order they are given
  --destination <tool>   Print what this tool wants pasted into it — the krowk
                         block for `github`, `linear` and the like, the bare link for
                         the ones that unfurl it themselves, like `slack`. A tool
                         krowk has not been told about gets the block
  --private              Upload where only this workspace can read it. The
                         image still embeds anywhere — its byte URL is the
                         capability — but the card opens only for a signed-in
                         member, reads as not found to everyone else, and
                         unfurls nowhere. Needs an API key
  --session <id>         Override the detected agent session
  --repo <owner/name>    Override the detected repository
  --commit <sha>         Override the detected commit
  --agent <name>         Override the detected agent
  --metadata <key=val>   Extra metadata — repeat for more than one. On push it
                         lands on each artifact; on `runs start`, on the run.
                         Your value wins over a detected one. Metadata is public.

LIST FLAGS
  --limit <n>            Rows per page (1–100, default 50)
  --before <slug>        Start after this row — the `next` of the last page
  --run <slug|link>      On `uploads list`, narrow it to what one run produced

SESSIONS FLAGS
  --harness <name>       On `sessions`, only sessions from this harness
  --worktree <path>      On `sessions`, only sessions in this worktree, by path
  --limit <n>            On `sessions`, list at most this many (default 50)
  --all                  On `sessions`, list every session, ignoring --limit
  --thinking             On `sessions show`, show full thinking parts
  --max-usd <n>          On `sessions budget`, trip over this metered cost
  --max-tokens <n>       On `sessions budget`, trip over this many generated
                         tokens (output and reasoning) — the session's and
                         its subagents'. Over a limit it exits 4; a Claude
                         Code hook blocks only on exit 2, so block on a trip
                         alone: `krowk sessions budget "$ID" --max-usd 5;
                         [ $? -ne 4 ] || exit 2` — any other failure then
                         warns without stopping the agent
  --from <source>        On `sessions import`, whose transcripts to read:
                         claude, cursor, opencode, ledger, or all.
                         Required
  --dry-run              Count what would be imported and write nothing
  --limit <n>            On `sessions import`, read at most this many
                         transcripts per source (0, the default, is all)
  --yes                  On `sessions rebuild`, delete krowk.db without asking —
                         required when nobody is at a terminal to confirm
  --no-network           On `sessions sync`, skip the models.dev price refresh

AUTH FLAGS
  --token <key>          Store this key rather than asking the browser — how CI
                         logs in, and it opens nothing
  --no-browser           Print the code and the page instead of opening a browser

CONFIG FLAGS
  --global               On `config set` and `config unset`, write the machine-wide
                         file instead of the repository's

GLOBAL FLAGS
  --workspace <name>     Use this workspace's stored key for this one command
  --dev                  Talk to a local registry at {dev_url}
  --format <fmt>         human | json | markdown | url (default: human on a TTY, json when piped)
                         markdown and url describe an upload; other commands fall back to json
  --json                 Shorthand for --format json
  --quiet                Raw JSON, no envelope
  --jq <expr>            Filter the JSON with a jq expression, built in — implies
                         --format json, and reads the bare record under --quiet
  -h, --help             Show this
  -v, --version          Print the version

ENVIRONMENT
  KROWK_TOKEN            API token — wins over the credentials file
  KROWK_WORKSPACE        Workspace to use, as if by --workspace
  KROWK_API_URL          API base URL (default {default_url})
  KROWK_DEV              1/true/yes/on — same as --dev
  KROWK_AGENT            Agent name to report
  KROWK_NO_UPDATE_CHECK  1/true/yes/on — never check for or mention new releases
  KROWK_HOME             Where krowk keeps its files, an absolute path (default ~/.krowk)

EXIT CODES
  0  it worked
  1  the command was wrong, or krowk failed on its own — also anything unclassified
  2  not found — no such artifact or run in this workspace, or no such endpoint
  3  refused for want of credentials — no key, a key the registry rejects, a
     browser login somebody denied or that nothing on CI could approve, or no
     claim token where that is the only authority (a claim token the registry
     does not recognise is 2, since it answers that as no such record)
  4  refused by the registry on the request or the state of things — retrying
     unchanged answers the same; also a session over its `sessions budget`
  5  rate limited — wait and retry
  6  the bytes did not move — the registry or object storage could not be reached
  7  the registry failed on its side, or answered something unreadable — or a
     login page krowk will not open, which is the same news
  8  gone — the artifact expired or was taken down, or a browser login lapsed
     before anybody approved it; no retry brings any of them back

Registry precedence: --dev, then KROWK_API_URL, then KROWK_DEV, then the default.

Run metadata — the pull request, the links, the references, the session — is
recorded on a run, and a run belongs to a workspace, so it needs an API key.
Without one an upload still works: it lands anonymously, expires within a day,
and comes back with a claim token that `krowk claim` spends to move it
into a workspace — where a paid plan keeps it and a free one gives it another day.

Wherever an artifact or a run is named — a positional, or --run — a link that
carries it does just as well: the card page, the CDN URL under it, or anything
else krowk printed. A link carrying no slug of the kind the command wants, or
two different ones, is refused before anything is sent.

Taking an upload down removes the bytes at once and leaves the link reporting
that it was taken down. There is no undo and no confirmation — it is what to
reach for when something was published by accident. A key takes down anything in
its workspace; an upload that is still anonymous is taken down with the claim
token it came back with, passed after the slug.

Logging in goes through a browser. `krowk login` asks the registry to open
an authorization, prints a short code and opens the page that approves it;
approving mints a key and this command collects it, once. Over SSH or with no
display it prints the code and the page instead of opening anything, which is
what --no-browser asks for everywhere else. On CI it is refused outright, since
nothing there can approve it — that is what --token is for, and --token never
opens or waits for anything. `krowk login` is your krowk account; a model
provider — a Claude or ChatGPT subscription, SuperGrok, an API key — is
connected with the full build's `krowk connect`.

A key belongs to one workspace, and the credentials file holds one key per
workspace: logging in against a second workspace adds a key rather than
replacing the first. Which key a command uses is decided in order by
--workspace, KROWK_WORKSPACE, the repository's .krowk/config.json, the global
config, and finally whichever key logged in last. `krowk config set workspace <name>`
pins a repository to a workspace, so every command run inside it — by anyone,
agent or person — lands there without saying so.

Credentials live in {credentials} (0600), and everything else krowk keeps
beside them in ~/.krowk, or KROWK_HOME.
Config lives in {config}, and per repository in <git-root>/.krowk/config.json.

LEARN MORE
{learn_more}"#;

pub fn help(c: &Catalog, credentials: &str, config: &str) -> String {
    banner(&c.version)
        + &HELP
            .replace("{commands}", &command_block(c))
            .replace("{dev_url}", krowk_api::DEV_BASE_URL)
            .replace("{default_url}", krowk_api::DEFAULT_BASE_URL)
            .replace("{credentials}", credentials)
            .replace("{config}", config)
            .replace("{learn_more}", &hint_block(LEARN_MORE))
}

/// The sections, each command beside what it does, in one column width
/// across the whole page.
fn command_block(c: &Catalog) -> String {
    let leaves = c.leaves();
    let summary = |name: &str| leaves.iter().find(|l| l.name == name).map_or("", |l| l.summary);
    let width = SECTIONS.iter().flat_map(|(_, names)| names.iter()).map(|n| n.len()).max().unwrap_or(0);
    SECTIONS
        .iter()
        .map(|(title, names)| {
            let rows: Vec<String> = names.iter().map(|n| format!("  {n:<width$}  {}", summary(n))).collect();
            format!("{title}\n{}", rows.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn hint_block(hints: &[(&str, &str)]) -> String {
    let width = hints.iter().map(|(cmd, _)| cmd.chars().count()).max().unwrap_or(0);
    hints.iter().map(|(cmd, why)| format!("  {cmd:<width$}  {why}")).collect::<Vec<_>>().join("\n")
}

/// One command's own help: the catalog read back as text.
pub fn command_help(cmd: &Command, globals: &[Flag]) -> String {
    let mut summary = cmd.summary.chars();
    let lowered: String = summary.next().map(|c| c.to_lowercase().collect::<String>()).unwrap_or_default() + summary.as_str();
    let mut lines = vec![format!("krowk {} — {lowered}", cmd.name)];
    if !cmd.usage.is_empty() {
        lines.extend(["".into(), "USAGE".into(), format!("  {}", cmd.usage)]);
    }
    if !cmd.subcommands.is_empty() {
        lines.extend(["".into(), "COMMANDS".into()]);
        let width = cmd.subcommands.iter().map(|s| s.usage.len()).max().unwrap_or(0) + 4;
        lines.extend(cmd.subcommands.iter().map(|s| format!("  {:<width$}{}", s.usage, s.summary)));
    }
    let width = cmd
        .args
        .iter()
        .map(|a| arg_label(a).len())
        .chain(cmd.flags.iter().chain(globals).map(|f| flag_label(f).len()))
        .max()
        .unwrap_or(0)
        + 2;
    if !cmd.args.is_empty() {
        lines.extend(["".into(), "ARGUMENTS".into()]);
        lines.extend(cmd.args.iter().map(|a| format!("  {:<width$}{}", arg_label(a), a.summary)));
    }
    if !cmd.flags.is_empty() {
        lines.extend(["".into(), "FLAGS".into()]);
        lines.extend(cmd.flags.iter().map(|f| format!("  {:<width$}{}", flag_label(f), f.usage)));
    }
    lines.extend(["".into(), "GLOBAL FLAGS".into()]);
    lines.extend(globals.iter().map(|f| format!("  {:<width$}{}", flag_label(f), f.usage)));
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
