//! What krowk says it can do, as data. The human help's command list is
//! rendered from it, `krowk help --json` serialises it, and the flag parser
//! reads it — so the surface cannot say one thing and route another.

use serde::Serialize;

/// The whole surface.
#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    pub name: &'static str,
    pub version: String,
    pub summary: &'static str,
    pub commands: Vec<Command>,
    pub global_flags: Vec<Flag>,
    pub environment: Vec<EnvVar>,
}

/// One thing krowk does, or a group of them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Command {
    pub name: String,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub usage: &'static str,
    pub summary: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<Arg>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<Flag>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subcommands: Vec<Command>,
    /// The commands that answer with something other than a record, which
    /// --jq has nothing to read on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Arg {
    pub name: &'static str,
    pub summary: &'static str,
    pub required: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub repeated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Flag {
    pub name: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<&'static str>,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub default: &'static str,
    pub usage: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub repeatable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvVar {
    pub name: &'static str,
    pub usage: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub default: &'static str,
}

pub const STRING: &str = "string";
pub const INT: &str = "int";
pub const BOOL: &str = "bool";

fn flag(name: &'static str, kind: &'static str, usage: impl Into<String>) -> Flag {
    let default = match kind {
        BOOL => "false",
        _ => "",
    };
    Flag { name, aliases: Vec::new(), kind, default, usage: usage.into(), repeatable: false }
}

fn repeatable(name: &'static str, usage: impl Into<String>) -> Flag {
    Flag { repeatable: true, ..flag(name, STRING, usage) }
}

fn with_default(f: Flag, default: &'static str) -> Flag {
    Flag { default, ..f }
}

fn arg(name: &'static str, summary: &'static str, required: bool) -> Arg {
    Arg { name, summary, required, repeated: false }
}

fn cmd(name: &str, usage: &'static str, summary: &'static str) -> Command {
    Command { name: name.into(), usage, summary, ..Command::default() }
}

const ARTIFACT_ARG: &str = "The artifact slug, or a link carrying it — the card page or the CDN URL";
const RUN_ARG: &str = "The run slug, or a link carrying it";

fn run_flag(usage: &str) -> Flag {
    flag("run", STRING, format!("{usage}. Its slug, or a link carrying it"))
}

fn upload_flags() -> Vec<Flag> {
    let mut flags = vec![
        run_flag("Attach to an existing run instead of opening one"),
        repeatable(
            "caption",
            "What this file shows, recorded on the artifact as `krowk.caption`. Repeat to caption several files, in the order they are given",
        ),
        flag(
            "destination",
            STRING,
            "Print what this tool wants pasted into it, e.g. github or slack. A tool krowk has not been told about gets the markdown block",
        ),
        flag(
            "private",
            BOOL,
            "Upload where only this workspace can read it. The image still embeds — the byte URL is the capability — but the card opens only for a signed-in member and unfurls nowhere. Needs an API key",
        ),
    ];
    flags.extend(metadata_flags());
    flags
}

fn metadata_flags() -> Vec<Flag> {
    vec![
        flag("pull-request", STRING, "Pull request the work belongs to"),
        repeatable(
            "link",
            "Link this work is about — the issue, the spec, the discussion. An absolute http(s) URL, repeatable up to 20, recorded on the run as `krowk.links`",
        ),
        repeatable("link-title", "What to call the --link before it, instead of its URL. One line"),
        repeatable(
            "link-rel",
            format!("What the --link before it is: {} — or a word of your own", crate::runctx::LINK_RELS.join(", ")),
        ),
        repeatable("reference", "Related identifier that is not a URL, e.g. a ticket key — repeat for more than one. A URL is a --link"),
        flag("session", STRING, "Agent session ID"),
        flag(
            "title",
            STRING,
            "Title for the work this push belongs to, recorded on the run. What a pasted link says is the artifact's --caption",
        ),
        flag("repo", STRING, "Override the detected repository"),
        flag("commit", STRING, "Override the detected commit"),
        flag("agent", STRING, "Override the detected agent"),
        repeatable("metadata", "Extra key=value metadata — your value wins over a detected one. Public"),
    ]
}

fn page_flags() -> Vec<Flag> {
    vec![
        with_default(flag("limit", INT, "Rows per page (1–100)"), "50"),
        flag("before", STRING, "Start after this row — the `next` of the last page"),
    ]
}

fn global_flag() -> Flag {
    flag("global", BOOL, "Write the machine-wide config instead of the repository's")
}

pub fn catalog(version: &str) -> Catalog {
    let file = Arg { repeated: true, ..arg("file", "Path to upload", true) };
    #[allow(unused_mut)]
    let mut c = Catalog {
        name: "krowk",
        version: version.into(),
        summary: "permalinks for agent output",
        commands: vec![
            Command {
                args: vec![file.clone()],
                flags: upload_flags(),
                ..cmd("push", "krowk push <file...> [flags]", "Upload files, get a link for each")
            },
            Command {
                subcommands: vec![
                    Command {
                        args: vec![file],
                        flags: upload_flags(),
                        ..cmd("create", "krowk uploads create <file...> [flags]", "The same thing, spelled out")
                    },
                    Command {
                        flags: [page_flags(), vec![run_flag("Narrow it to what one run produced")]].concat(),
                        ..cmd("list", "krowk uploads list [flags]", "List uploads, newest first — a run's, or the workspace's")
                    },
                    Command {
                        args: vec![arg("artifact", ARTIFACT_ARG, true)],
                        ..cmd("show", "krowk uploads show <artifact>", "Read one artifact back")
                    },
                    Command {
                        args: vec![arg("artifact", ARTIFACT_ARG, true)],
                        flags: vec![run_flag("The run to put it under — required, and it must be one this workspace holds")],
                        ..cmd("attach", "krowk uploads attach <art> --run <run>", "Put an upload under a run afterwards")
                    },
                    Command {
                        args: vec![
                            arg("artifact", ARTIFACT_ARG, true),
                            arg(
                                "claim-token",
                                "The claim token an anonymous upload came back with, when there is no key to authorise the takedown",
                                false,
                            ),
                        ],
                        ..cmd("delete", "krowk uploads delete <art> [token]", "Take an upload down — immediate, cannot be undone")
                    },
                ],
                ..cmd("uploads", "", "Work with uploads")
            },
            Command {
                subcommands: vec![
                    Command { flags: metadata_flags(), ..cmd("start", "krowk runs start [flags]", "Open a run to group uploads under") },
                    Command { flags: page_flags(), ..cmd("list", "krowk runs list [--limit --before]", "List the workspace's runs, newest first") },
                    Command { args: vec![arg("run", RUN_ARG, true)], ..cmd("show", "krowk runs show <run>", "Read one run back, with its metadata") },
                    Command { args: vec![arg("run", RUN_ARG, true)], ..cmd("finish", "krowk runs finish <run>", "Close a run") },
                ],
                ..cmd("runs", "", "Work with runs")
            },
            Command {
                args: vec![arg("artifact", ARTIFACT_ARG, true), arg("claim-token", "The token the anonymous upload came back with", true)],
                flags: vec![run_flag("The run to group it under while claiming — a claimed upload has none otherwise")],
                ..cmd("claim", "krowk claim <artifact> <token> [--run]", "Keep an anonymous upload past expiry")
            },
            Command {
                subcommands: vec![
                    Command {
                        flags: vec![
                            flag("token", STRING, "Check and store this key instead of asking the browser — how CI logs in, e.g. krowk_sk_..."),
                            flag(
                                "no-browser",
                                BOOL,
                                "Print the code and the page instead of opening a browser — the default over SSH, or with no display",
                            ),
                        ],
                        ..cmd("login", "krowk auth login [--token <token>] [--no-browser]", "Approve this machine in the browser, or store a key")
                    },
                    Command { no_json: true, ..cmd("token", "krowk auth token", "Print the stored token") },
                    cmd("verify", "krowk auth verify", "Check the key and its workspace"),
                ],
                ..cmd("auth", "", "Manage the API key")
            },
            Command {
                subcommands: vec![
                    cmd("list", "krowk workspaces [list]", "List the stored keys, and which workspace resolves here"),
                    Command {
                        args: vec![arg(
                            "workspace",
                            "A workspace `krowk workspaces` lists, e.g. ws_9hj3kd8a — a person at a terminal may omit it and pick from a list instead",
                            true,
                        )],
                        ..cmd("use", "krowk workspaces use <workspace>", "Make a stored key the machine-wide default")
                    },
                ],
                ..cmd("workspaces", "", "The stored keys, one per workspace")
            },
            Command {
                subcommands: vec![
                    cmd("show", "krowk config show", "The effective configuration, and which layer set each value"),
                    Command {
                        args: vec![
                            arg("key", "A configuration key, e.g. `workspace`", true),
                            arg(
                                "value",
                                "What to set it to, e.g. ws_9hj3kd8a — for `workspace`, a person at a terminal may omit it and pick from the stored keys instead",
                                true,
                            ),
                        ],
                        flags: vec![global_flag()],
                        ..cmd("set", "krowk config set <key> <value> [--global]", "Write one value into the repo config, or the global one")
                    },
                    Command {
                        args: vec![arg("key", "A configuration key, e.g. `workspace`", true)],
                        flags: vec![global_flag()],
                        ..cmd("unset", "krowk config unset <key> [--global]", "Remove one value from the repo config, or the global one")
                    },
                ],
                ..cmd("config", "", "Pin a repository, or the machine, to a workspace")
            },
            cmd("doctor", "krowk doctor", "Check the local setup"),
            Command {
                flags: vec![
                    flag("harness", STRING, "Only sessions from this harness, e.g. claude"),
                    flag("worktree", STRING, "Only sessions in this worktree, by path"),
                    with_default(flag("limit", INT, "List at most this many sessions (default 50)"), "50"),
                    flag("all", BOOL, "List every session, ignoring --limit"),
                ],
                subcommands: vec![
                    Command {
                        args: vec![arg("id", "The session id, an unambiguous id prefix of at least 8 chars, or a foreign session id", true)],
                        flags: vec![flag("thinking", BOOL, "Show full thinking parts instead of one line each")],
                        ..cmd("show", "krowk sessions show <id> [--thinking]", "Read one session back, with its turns, messages and parts")
                    },
                    Command {
                        flags: vec![
                            flag("from", STRING, "Which transcripts to read: claude, cursor, opencode, ledger (provider usage ledgers), or all. Required"),
                            flag("dry-run", BOOL, "Count what would be imported and write nothing"),
                            with_default(flag("limit", INT, "Read at most this many transcripts per source (0 is all)"), "0"),
                        ],
                        ..cmd(
                            "import",
                            "krowk sessions import --from <provider|all> [--dry-run] [--limit N]",
                            "Read agent transcripts on this machine into the local store",
                        )
                    },
                    Command {
                        flags: vec![flag("yes", BOOL, "Delete without asking — required when nobody is at a terminal to confirm")],
                        ..cmd("rebuild", "krowk sessions rebuild [--yes]", "Delete the local store and re-import every transcript")
                    },
                    Command {
                        args: vec![arg("id", "The session id, an unambiguous id prefix of at least 8 chars, or a foreign session id", true)],
                        flags: vec![
                            flag("max-usd", STRING, "Trip when the session's metered cost is over this many dollars"),
                            flag("max-tokens", STRING, "Trip when the session's generated tokens (output and reasoning) are over this many"),
                        ],
                        ..cmd(
                            "budget",
                            "krowk sessions budget <id> [--max-usd N] [--max-tokens N]",
                            "Check a session against a spend limit, by what the provider metered",
                        )
                    },
                    Command {
                        flags: vec![flag("no-network", BOOL, "Skip the models.dev price refresh")],
                        ..cmd("sync", "krowk sessions sync [--no-network]", "Import only what changed since the last import, and refresh prices")
                    },
                ],
                ..cmd(
                    "sessions",
                    "krowk sessions [--harness <name>] [--worktree <path>] [--limit N] [--all]",
                    "List every agent thread on this machine, newest first",
                )
            },
            Command {
                subcommands: vec![cmd("refresh", "krowk pricing refresh", "Refresh the models.dev price cache")],
                ..cmd("pricing", "", "Model price data")
            },
            cmd("upgrade", "krowk upgrade", "Upgrade krowk to the latest release"),
            Command {
                args: vec![arg("command", "The command to describe, e.g. `uploads attach`", false)],
                ..cmd("help", "krowk help [command]", "Show this, or one command's own help")
            },
        ],
        global_flags: global_flags(),
        environment: vec![
            EnvVar { name: "KROWK_TOKEN", usage: "API token — wins over the credentials file", default: "" },
            EnvVar {
                name: "KROWK_WORKSPACE",
                usage: "Workspace whose stored key to use, as if by --workspace — outranks the config files, loses to the flag",
                default: "",
            },
            EnvVar { name: "KROWK_API_URL", usage: "API base URL", default: krowk_api::DEFAULT_BASE_URL },
            EnvVar { name: "KROWK_DEV", usage: "1/true/yes/on — same as --dev", default: "" },
            EnvVar { name: "KROWK_AGENT", usage: "Agent name to report", default: "" },
            EnvVar { name: "KROWK_NO_UPDATE_CHECK", usage: "1/true/yes/on — never check for or mention new releases", default: "" },
        ],
    };
    #[cfg(feature = "harness")]
    c.commands.push(providers_command());
    #[cfg(feature = "harness")]
    c.commands.push(cmd(
        "status",
        "krowk status",
        "Whether each provider instance can run a turn here: its readiness, where its key or login comes from, and what fixes it. Exits 3 when none is ready",
    ));
    c
}

pub fn global_flags() -> Vec<Flag> {
    let flags = vec![
        flag("workspace", STRING, "Use this workspace's stored key for this one command — outranks KROWK_WORKSPACE and every config file"),
        flag("dev", BOOL, format!("Talk to a local registry at {}", krowk_api::DEV_BASE_URL)),
        flag("format", STRING, "human | json | markdown | url (default: human on a TTY, json when piped)"),
        flag("json", BOOL, "Shorthand for --format json"),
        flag("quiet", BOOL, "Raw JSON, no envelope"),
        flag(
            "jq",
            STRING,
            "Filter the JSON with a jq expression — built in, no jq binary needed. Implies --format json, and reads the bare record under --quiet",
        ),
        Flag { aliases: vec!["h"], ..flag("help", BOOL, "Show the help") },
        Flag { aliases: vec!["v"], ..flag("version", BOOL, "Print the version") },
    ];
    #[cfg(feature = "harness")]
    let flags = [flags, prompt_flags()].concat();
    flags
}

/// `krowk providers`: the native engine's instances — API-key profiles,
/// compatible servers, and the SuperGrok login. The harness build's only.
#[cfg(feature = "harness")]
fn providers_command() -> Command {
    const PROVIDER_ARG: &str = "anthropic, openai, xai, openrouter, openai-compatible, supergrok (xAI with a SuperGrok or X Premium subscription), claude (runs Claude Code, signed in with its own login), or codex (runs Codex, signed in with its own login)";
    Command {
        subcommands: vec![
            Command {
                args: vec![arg("provider", PROVIDER_ARG, true)],
                flags: vec![
                    flag("name", STRING, "Name the instance <provider>:<name> (for openai-compatible, <name> alone); the provider's own name when absent"),
                    flag("api-key-env", STRING, "The environment variable holding the key — krowk stores its name, never the key. Default: the conventional one, or <PROVIDER>_<NAME>_API_KEY for a named instance. For claude and codex, only when given: the key a router (with --base-url) or a Console account runs on, handed to the backend"),
                    flag("base-url", STRING, "Where the API is, for a gateway, a router or a local server — required for openai-compatible"),
                    flag("client-id", STRING, "supergrok: the OAuth client id to sign in as, when xAI's server offers no registration"),
                    flag("device", BOOL, "supergrok, codex: sign in with a code typed into any browser, instead of one opened here"),
                    flag("no-browser", BOOL, "supergrok: print the sign-in link instead of opening a browser"),
                    flag("binary", STRING, "claude, codex: the binary to run; claude or codex on PATH when absent"),
                    flag("config-dir", STRING, "claude, codex: the CLAUDE_CONFIG_DIR or CODEX_HOME this instance signs in and keeps its sessions in; a new one under krowk's data directory for a named instance"),
                ],
                ..cmd(
                    "add",
                    "krowk providers add <provider> [--name N] [--api-key-env VAR] [--base-url URL] [--device] [--binary PATH] [--config-dir DIR]",
                    "Add an instance, sign in to SuperGrok, or add a Claude Code or Codex account (signed in with `claude auth login` or `codex login`)",
                )
            },
            cmd("list", "krowk providers list", "List every instance, where it runs, and whether it is ready — the same check as `krowk status`, a Claude Code or Codex login asked of Claude Code or Codex"),
            Command {
                args: vec![arg("instance", "The instance to remove, e.g. openai:work", true)],
                ..cmd("remove", "krowk providers remove <instance>", "Remove an instance's definition, and forget its login")
            },
        ],
        ..cmd("providers", "", "The provider instances: API keys, logins, and Claude Code and Codex accounts")
    }
}

/// `krowk -p "…"`: the harness build's headless agent. Flags rather than a
/// command, because that is how every agent CLI spells it.
#[cfg(feature = "harness")]
fn prompt_flags() -> Vec<Flag> {
    vec![
        Flag {
            aliases: vec!["p"],
            ..flag("print", BOOL, "Run the prompt given as the arguments (or on stdin) headless: krowk's own agent answers it, then exits")
        },
        with_default(flag("output-format", STRING, "With -p: text (the answer), json (the result event) or stream-json (every event, one per line)"), "text"),
        flag("model", STRING, "With -p: the model, as <instance>/<model> or a model id on the anthropic instance, e.g. claude-opus-5-5, claude:work/sonnet to run Claude Code, or codex:team/gpt-5.5 to run Codex; with --resume, the session moves there"),
        flag("resume", STRING, "With -p: continue this krowk session — the sessionId a result names, or its krowk.db id"),
        with_default(
            flag(
                "permission-mode",
                STRING,
                "With -p and the agent: default, acceptEdits, plan or bypassPermissions, as in Claude Code. Without it, the settings' permissions.defaultMode; a deny rule holds in every mode",
            ),
            "default",
        ),
        flag(
            "toolset",
            STRING,
            "With -p: the tools' preset — claude (str_replace), gpt (apply_patch) or grok (search_replace). The model's family picks one when absent",
        ),
        flag(
            "effort",
            STRING,
            "With -p: how hard the model thinks — none, minimal, low, medium, high, xhigh or max, mapped onto the nearest the model takes. The instance's, else the provider's default, when absent",
        ),
        flag(
            "max-usd",
            STRING,
            "With -p, and in the TUI: stop the session before the model call that would take it, with its subagents, past this many dollars — metered, priced from models.dev. Exits 4, like `krowk sessions budget`",
        ),
        flag(
            "max-tokens",
            STRING,
            "With -p, and in the TUI: stop the session before the model call that would take its generated tokens (output and reasoning, subagents included) past this many. Exits 4",
        ),
        flag(
            "trust",
            BOOL,
            "With -p: let a backend (Claude Code, Codex) run in a repository not yet trusted — it runs the repository's hooks and MCP servers without asking. Without it, -p refuses unless a person at the terminal says yes",
        ),
    ]
}

impl Catalog {
    /// The commands that can be run, each named by its whole path. A group is
    /// not one — except `sessions`, whose bare form lists.
    pub fn leaves(&self) -> Vec<Command> {
        let mut out = Vec::new();
        for c in &self.commands {
            if c.subcommands.is_empty() {
                out.push(c.clone());
                continue;
            }
            if c.name == "sessions" && !c.usage.is_empty() {
                out.push(Command { subcommands: Vec::new(), ..c.clone() });
            }
            for sub in &c.subcommands {
                out.push(Command { name: format!("{} {}", c.name, sub.name), ..sub.clone() });
            }
        }
        out
    }

    /// What a caller typed after `help`: a leaf, or a group with everything
    /// under it. A command with nothing under it answers for whatever follows,
    /// since that is its arguments.
    pub fn find(&self, path: &[String]) -> Option<Command> {
        let first = path.first()?;
        let c = self.commands.iter().find(|c| &c.name == first)?;
        if path.len() == 1 || c.subcommands.is_empty() {
            return Some(c.clone());
        }
        let sub = c.subcommands.iter().find(|s| s.name == path[1])?;
        Some(Command { name: format!("{} {}", c.name, sub.name), ..sub.clone() })
    }

    /// Every flag any command takes, by name, with its type — what the parser
    /// accepts.
    pub fn all_flags(&self) -> Vec<Flag> {
        let mut out: Vec<Flag> = self.global_flags.clone();
        let mut walk = |flags: &[Flag]| {
            for f in flags {
                if !out.iter().any(|o| o.name == f.name) {
                    out.push(f.clone());
                }
            }
        };
        for c in &self.commands {
            walk(&c.flags);
            for s in &c.subcommands {
                walk(&s.flags);
            }
        }
        out
    }
}

/// A heading in the human help, and the leaves under it in reading order.
pub const SECTIONS: &[(&str, &[&str])] = &[
    ("PUSH & PASTE", &["push", "uploads create"]),
    ("RUNS", &["runs start", "runs finish", "runs show", "runs list"]),
    ("UPLOADS", &["uploads list", "uploads show", "uploads attach", "uploads delete", "claim"]),
    ("SESSIONS", &["sessions", "sessions show", "sessions budget", "sessions import", "sessions rebuild", "sessions sync"]),
    // The harness build's own commands.
    #[cfg(feature = "harness")]
    ("AGENT", &["status", "providers add", "providers list", "providers remove"]),
    (
        "ACCOUNT & SYSTEM",
        &[
            "auth login", "auth verify", "auth token", "workspaces list", "workspaces use", "config show", "config set",
            "config unset", "doctor", "pricing refresh", "upgrade", "help",
        ],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_leaf_sits_under_exactly_one_heading() {
        let c = catalog("dev");
        let mut listed: Vec<&str> = SECTIONS.iter().flat_map(|(_, names)| names.iter().copied()).collect();
        let mut leaves: Vec<String> = c.leaves().into_iter().map(|l| l.name).collect();
        listed.sort_unstable();
        leaves.sort_unstable();
        assert_eq!(listed, leaves.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn find_resolves_groups_leaves_and_arguments() {
        let c = catalog("dev");
        let words = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(c.find(&words("uploads attach")).unwrap().name, "uploads attach");
        assert_eq!(c.find(&words("uploads")).unwrap().subcommands.len(), 5);
        assert_eq!(c.find(&words("push shot.png")).unwrap().name, "push");
        assert!(c.find(&words("uploads bogus")).is_none());
    }
}
