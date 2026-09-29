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

// What `krowk help <command>` explains beyond the summary, wrapped for 80
// columns: what the overview used to carry under its command list, beside
// the command it is about.

const PUSH_ABOUT: &str = "\
Run metadata — the pull request, the links, the references, the session — is
recorded on a run, and a run belongs to a workspace, so it needs an API key.
Without one an upload still works: it lands anonymously, expires within a
day, and comes back with a claim token that `krowk claim` spends to move it
into a workspace — where a paid plan keeps it and a free one gives it another
day.";

#[cfg(feature = "harness")]
const SYNC_ATTACH_ABOUT: &str = "\
Prints the session as stream-json on stdout. Each line typed on stdin is a
prompt to it, queued while no host is online, except these:

  /approve REQUEST_ID        allow the tool call an approval.requested names
  /allow-session REQUEST_ID  allow it, and calls like it for the session
  /deny REQUEST_ID           refuse it
  /interrupt                 stop the running turn
  /steer TEXT                add TEXT to the running turn without stopping it

Each approval.requested line carries its requestId, and a hint on stderr
names the command that answers it.";

const CLAIM_ABOUT: &str = "\
Moves an anonymous upload into this key's workspace, spending the claim token
it came back with: `krowk help push`.";

const DELETE_ABOUT: &str = "\
Taking an upload down removes the bytes at once and leaves the link reporting
that it was taken down. There is no undo and no confirmation — it is what to
reach for when something was published by accident. A key takes down anything
in its workspace; an upload that is still anonymous is taken down with the
claim token it came back with, passed after the slug.";

/// Your krowk account, which a model provider is not: that is `krowk
/// connect`, and the help says so where someone could confuse the two.
macro_rules! login_about {
    ($connect:literal) => {
        concat!(
            "\
Logging in goes through a browser. `krowk login` asks the registry to open an
authorization, prints a short code and opens the page that approves it;
approving mints a key and this command collects it, once. Over SSH or with no
display it prints the code and the page instead of opening anything, which is
what --no-browser asks for everywhere else. On CI it is refused outright, since
nothing there can approve it — that is what --token is for, and --token never
opens or waits for anything.

`krowk login` is your krowk account; a model provider — a Claude or ChatGPT
subscription, SuperGrok, an API key — is connected with ",
            $connect,
            ".\nA key belongs to one workspace: `krowk help workspaces`."
        )
    };
}
#[cfg(feature = "harness")]
const LOGIN_ABOUT: &str = login_about!("`krowk connect`");
#[cfg(not(feature = "harness"))]
const LOGIN_ABOUT: &str = login_about!("the full build's\n`krowk connect`");

const WORKSPACES_ABOUT: &str = "\
The stored keys, one per workspace. A key belongs to one workspace, and the
credentials file holds one key per workspace: logging in against a second
workspace adds a key rather than replacing the first. Which key a command uses
is decided in order by --workspace, KROWK_WORKSPACE, the repository's
.krowk/config.json, the global config, and finally whichever key logged in
last. `krowk config set workspace <name>` pins a repository to a workspace, so
every command run inside it — by anyone, agent or person — lands there
without saying so. Where the files live: `krowk help environment`.";

const BUDGET_ABOUT: &str = "\
Checks by what the provider metered: the session's cost and generated tokens
(output and reasoning), its subagents' included. Over a limit it exits 4; a
Claude Code hook blocks only on exit 2, so block on a trip alone:

  krowk sessions budget \"$ID\" --max-usd 5; [ $? -ne 4 ] || exit 2

— any other failure then warns without stopping the agent.";

fn run_flag(usage: &str) -> Flag {
    flag("run", STRING, format!("{usage}. Its slug, or a link carrying it"))
}

fn upload_flags() -> Vec<Flag> {
    let mut flags = vec![
        run_flag("Attach to an existing run instead of opening one"),
        repeatable(
            "caption",
            "What this file shows, recorded on the artifact as `krowk.caption` and used wherever it is pasted. Repeat to caption several files, in the order they are given",
        ),
        flag(
            "destination",
            STRING,
            "Print what this tool wants pasted into it: the krowk block for github, linear and the like, the bare link for the ones that unfurl it themselves, like slack. A tool krowk has not been told about gets the block",
        ),
        flag(
            "private",
            BOOL,
            "Upload where only this workspace can read it. The image still embeds — the byte URL is the capability — but the card opens only for a signed-in member, reads as not found to everyone else, and unfurls nowhere. Needs an API key",
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
            "Link this work is about — the issue, the spec, the discussion. An absolute http(s) URL, repeatable up to 20, recorded on the run as `krowk.links`; label or classify each with --link-title and --link-rel",
        ),
        repeatable("link-title", "What to call the --link before it, instead of its URL. One line"),
        repeatable(
            "link-rel",
            format!("What the --link before it is: {} — or a word of your own", crate::runctx::LINK_RELS.join(", ")),
        ),
        repeatable("reference", "Related identifier that is not a URL, e.g. a ticket key — repeat for more than one. A URL is a --link"),
        flag("session", STRING, "Override the detected agent session ID"),
        flag(
            "title",
            STRING,
            "Title for the work this push belongs to, recorded on the run. What a pasted link says is the artifact's --caption",
        ),
        flag("repo", STRING, "Override the detected repository"),
        flag("commit", STRING, "Override the detected commit"),
        flag("agent", STRING, "Override the detected agent"),
        repeatable(
            "metadata",
            "Extra key=value metadata, repeatable: on push it lands on each artifact, on `runs start` on the run. Your value wins over a detected one. Metadata is public",
        ),
    ]
}

fn page_flags() -> Vec<Flag> {
    vec![
        with_default(flag("limit", INT, "Rows per page (1–100)"), "50"),
        flag("before", STRING, "Start after this row — the `next` of the last page"),
    ]
}

fn login_flags() -> Vec<Flag> {
    vec![
        flag("token", STRING, "Check and store this key instead of asking the browser — how CI logs in, and it opens nothing. E.g. krowk_sk_..."),
        flag("no-browser", BOOL, "Print the code and the page instead of opening a browser — the default over SSH, or with no display"),
    ]
}

fn global_flag() -> Flag {
    flag("global", BOOL, "Write the machine-wide config instead of the repository's")
}

#[cfg(feature = "harness")]
const SUMMARY: &str = "a coding agent, and permalinks for its output";
#[cfg(not(feature = "harness"))]
const SUMMARY: &str = "permalinks for agent output";

pub fn catalog(version: &str) -> Catalog {
    let file = Arg { repeated: true, ..arg("file", "Path to upload", true) };
    #[allow(unused_mut)]
    let mut c = Catalog {
        name: "krowk",
        version: version.into(),
        summary: SUMMARY,
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
                        ..cmd("create", "krowk uploads create <file...> [flags]", "Upload files: the long form of `krowk push`")
                    },
                    Command {
                        flags: [page_flags(), vec![run_flag("Narrow it to what one run produced")]].concat(),
                        ..cmd("list", "krowk uploads list [flags]", "List uploads, newest first: a run's, or the workspace's")
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
                ..cmd("uploads", "", "List, show, attach or delete uploads")
            },
            Command {
                subcommands: vec![
                    Command { flags: metadata_flags(), ..cmd("start", "krowk runs start [flags]", "Open a run to group uploads under") },
                    Command { flags: page_flags(), ..cmd("list", "krowk runs list [--limit --before]", "List the workspace's runs, newest first") },
                    Command { args: vec![arg("run", RUN_ARG, true)], ..cmd("show", "krowk runs show <run>", "Read one run back, with its metadata") },
                    Command { args: vec![arg("run", RUN_ARG, true)], ..cmd("finish", "krowk runs finish <run>", "Close a run") },
                ],
                ..cmd("runs", "", "Group uploads under a run")
            },
            Command {
                args: vec![arg("artifact", ARTIFACT_ARG, true), arg("claim-token", "The token the anonymous upload came back with", true)],
                flags: vec![run_flag("The run to group it under while claiming — a claimed upload has none otherwise")],
                ..cmd("claim", "krowk claim <artifact> <token> [--run]", "Keep an anonymous upload past expiry")
            },
            Command {
                flags: login_flags(),
                ..cmd("login", "krowk login [--token <token>] [--no-browser]", "Sign in to your krowk account")
            },
            cmd("logout", "krowk logout", "Remove this machine's key"),
            cmd("whoami", "krowk whoami", "Show the key and its workspace"),
            Command {
                subcommands: vec![
                    Command {
                        flags: login_flags(),
                        ..cmd("login", "krowk auth login [--token <token>] [--no-browser]", "The long form of `krowk login`")
                    },
                    cmd("logout", "krowk auth logout", "The long form of `krowk logout`"),
                    Command { no_json: true, ..cmd("token", "krowk auth token", "Print the stored token") },
                    cmd("verify", "krowk auth verify", "The long form of `krowk whoami`"),
                ],
                ..cmd("auth", "", "Long forms of login, logout and whoami")
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
                ..cmd("workspaces", "", "Switch between stored keys")
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
                ..cmd("config", "", "Show or set configuration")
            },
            cmd("doctor", "krowk doctor", "Check this machine's setup"),
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
                            "Import this machine's agent transcripts",
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
                            flag(
                                "max-tokens",
                                STRING,
                                "Trip when the session's generated tokens (output and reasoning — the session's and its subagents') are over this many",
                            ),
                        ],
                        ..cmd("budget", "krowk sessions budget <id> [--max-usd N] [--max-tokens N]", "Check a session against a spend limit")
                    },
                    Command {
                        flags: vec![flag("no-network", BOOL, "Skip the models.dev price refresh")],
                        ..cmd("sync", "krowk sessions sync [--no-network]", "Import what changed since the last import, refresh prices")
                    },
                ],
                ..cmd(
                    "sessions",
                    "krowk sessions [--harness <name>] [--worktree <path>] [--limit N] [--all]",
                    "List and read agent sessions on this machine",
                )
            },
            Command {
                subcommands: vec![cmd("refresh", "krowk pricing refresh", "Refresh the models.dev price cache")],
                ..cmd("pricing", "", "Refresh model prices")
            },
            cmd("upgrade", "krowk upgrade", "Upgrade krowk"),
            Command {
                args: vec![arg("command", "The command or topic to describe, e.g. `uploads attach` or `exit-codes`", false)],
                flags: vec![flag("all", BOOL, "List every command and subcommand")],
                ..cmd("help", "krowk help [command|topic] [--all]", "This, a command's help, or a topic")
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
            EnvVar { name: "KROWK_HOME", usage: "Where krowk keeps its files, an absolute path", default: "~/.krowk" },
            #[cfg(feature = "harness")]
            EnvVar { name: "KROWK_TUI_HOST", usage: "local — the TUI runs its sessions in its own process, not the host daemon", default: "" },
        ],
    };
    #[cfg(feature = "harness")]
    c.commands.extend(connect_commands());
    #[cfg(feature = "harness")]
    c.commands.push(providers_command());
    #[cfg(feature = "harness")]
    c.commands.push(cmd("status", "krowk status", "What's connected, and whether each is ready"));
    #[cfg(all(feature = "harness", unix))]
    c.commands.push(host_command());
    #[cfg(all(feature = "harness", unix))]
    c.commands.push(relay_command());
    #[cfg(all(feature = "harness", unix))]
    c.commands.push(cmd("hosts", "krowk hosts", "The tailnet's machines tagged tag:krowk-host, from Tailscale"));
    #[cfg(feature = "harness")]
    c.commands.push(sync_command());
    #[cfg(feature = "harness")]
    c.commands.push(devices_command());
    c
}

/// `krowk sync`: this machine's end-to-end keys (R-E2E-3, R-E2E-4).
#[cfg(feature = "harness")]
fn sync_command() -> Command {
    Command {
        subcommands: vec![
            cmd("init", "krowk sync init", "Set up: a device key, an account key, its recovery phrase"),
            cmd("recover", "krowk sync recover", "Restore the account key here from its recovery phrase"),
            Command {
                flags: vec![flag("name", STRING, "What the workspace's device list calls this machine; its host name when absent (also KROWK_DEVICE_NAME)")],
                ..cmd("join", "krowk sync join [ACCOUNT_KEY_ID]", "Add this machine, approved from one that already syncs")
            },
            cmd("register", "krowk sync register [--name NAME]", "Tell the workspace this machine holds its account key"),
            #[cfg(unix)]
            cmd("sessions", "krowk sync sessions", "The synced sessions this machine can open"),
            #[cfg(unix)]
            cmd("host", "krowk sync host SESSION", "Run a session here and sync it until interrupted"),
            #[cfg(unix)]
            cmd("attach", "krowk sync attach SESSION", "Follow a synced session here; stdin lines are prompts or /commands"),
        ],
        ..cmd("sync", "", "End-to-end encryption keys for syncing sessions")
    }
}

/// `krowk relay`: the reference relay (R-RELAY-1), the contract of Canon's
/// engineering/relay.md in Rust — the hermetic stand-in and a self-hosting
/// path.
#[cfg(all(feature = "harness", unix))]
fn relay_command() -> Command {
    Command {
        subcommands: vec![Command {
            flags: vec![
                flag("addr", STRING, "Where to listen; loopback unless you name another address (default 127.0.0.1:7790)"),
                flag("ticket-keys", STRING, "The JSON file of the registry's ticket-signing public keys (relay.md → Tickets)"),
                flag("origin", STRING, "The origin devices dial and sign, as ws://host:port or wss://host; ws:// and the request's Host when absent"),
                flag("state", STRING, "The directory each channel's fence is kept in across restarts; required off loopback"),
            ],
            ..cmd("serve", "krowk relay serve --ticket-keys FILE [--addr HOST:PORT] [--origin URL] [--state DIR]", "Carry sealed sessions between their host and viewers")
        }],
        ..cmd("relay", "", "The relay other devices reach a session through")
    }
}

/// `krowk devices`: the machines that sync the workspace (R-E2E-3).
#[cfg(feature = "harness")]
fn devices_command() -> Command {
    Command {
        subcommands: vec![
            cmd("list", "krowk devices list", "The workspace's devices, and the account key this one holds"),
            cmd("approve", "krowk devices approve [CODE]", "Approve a new device's `krowk sync join`"),
        ],
        ..cmd("devices", "", "The machines that sync this workspace's sessions")
    }
}

/// `krowk host`: the per-user daemon sessions run in (R-HOST-1, R-HOST-2).
/// `serve` is what the first `krowk` that needs the daemon starts, and what
/// the service runs; it is not listed.
#[cfg(all(feature = "harness", unix))]
fn host_command() -> Command {
    Command {
        subcommands: vec![
            cmd("status", "krowk host status", "Whether the daemon runs: socket, pid, uptime, sessions"),
            Command {
                args: vec![arg("session", "The session id a result names", true)],
                ..cmd("attach", "krowk host attach <session>", "Follow a session in the daemon, live, as stream-json")
            },
            Command {
                flags: vec![flag("force", BOOL, "Stop it though other clients are connected; each reconnects to the next daemon")],
                ..cmd("stop", "krowk host stop [--force]", "Stop the daemon, once no turn runs in it")
            },
            cmd("enable", "krowk host enable", "Run the daemon as a systemd or launchd user service"),
            cmd("disable", "krowk host disable", "Stop the service and remove it"),
        ],
        ..cmd("host", "", "The daemon sessions run in, which outlives the terminal")
    }
}

/// Every flag the parser takes outside a command: the ones every command
/// takes, and in the full build the agent's.
pub fn global_flags() -> Vec<Flag> {
    let flags = core_flags();
    #[cfg(feature = "harness")]
    let flags = [flags, prompt_flags()].concat();
    flags
}

/// How many of `global_flags` every command takes; the agent's follow them.
pub const CORE_FLAGS: usize = 8;

/// The flags every command takes.
fn core_flags() -> Vec<Flag> {
    vec![
        flag("workspace", STRING, "Use this workspace's stored key for this one command — outranks KROWK_WORKSPACE and every config file"),
        flag("dev", BOOL, format!("Talk to a local registry at {}", krowk_api::DEV_BASE_URL)),
        flag(
            "format",
            STRING,
            "human | json | markdown | url (default: human on a TTY, json when piped). markdown and url describe an upload; other commands fall back to json",
        ),
        flag("json", BOOL, "Shorthand for --format json"),
        flag("quiet", BOOL, "Raw JSON, no envelope"),
        flag(
            "jq",
            STRING,
            "Filter the JSON with a jq expression — built in, no jq binary needed. Implies --format json, and reads the bare record under --quiet",
        ),
        Flag { aliases: vec!["h"], ..flag("help", BOOL, "Show the help") },
        Flag { aliases: vec!["v"], ..flag("version", BOOL, "Print the version") },
    ]
}

/// `krowk connect` and `krowk disconnect`: a model source, by vendor and
/// method. The harness build's only.
#[cfg(feature = "harness")]
fn connect_commands() -> Vec<Command> {
    vec![
        Command {
            args: vec![arg(
                "vendor",
                "anthropic, openai, xai, openrouter or openai-compatible — or an instance to reconnect, e.g. claude:work. A person at a terminal may omit it and pick",
                false,
            )],
            flags: vec![
                flag("method", STRING, "subscription (Claude, ChatGPT or SuperGrok, signed in by the vendor's own login), device (the same with a code typed into any browser: ChatGPT, SuperGrok) or api-key. Required without a terminal when the vendor has more than one"),
                flag("name", STRING, "Name a second account <provider>:<name> — claude:work, anthropic:work; the default-named one (claude, anthropic) when absent"),
                flag("default", BOOL, "Make it the default model even when config.json names one already; the first connection is the default by itself"),
                flag("api-key-env", STRING, "The environment variable holding the key — krowk stores its name, never the key. Default: the conventional one, or <PROVIDER>_<NAME>_API_KEY for a named account"),
                flag("key-stdin", BOOL, "An API key: store the key piped to stdin in krowk's provider credentials file (0600). Never a --key argument, which the shell's history would keep"),
                flag("key-ref", STRING, "An API key: store a reference instead — '$VAR' (read from that variable) or '!command' (its output, e.g. '!pass show anthropic', run once per krowk process). At a terminal, pasting at the prompt does either"),
                flag("base-url", STRING, "Where the API is, for a gateway, a router or a local server — asked for openai-compatible"),
                flag("client-id", STRING, "SuperGrok: the OAuth client id to sign in as, when xAI's server offers no registration"),
                flag("no-browser", BOOL, "SuperGrok: print the sign-in link instead of opening a browser"),
                flag("binary", STRING, "A subscription: the claude or codex binary to run; the one on PATH when absent"),
                flag("config-dir", STRING, "A subscription: the CLAUDE_CONFIG_DIR or CODEX_HOME the account signs in and keeps its sessions in; a new one in ~/.krowk/accounts/ for a named account"),
            ],
            ..cmd(
                "connect",
                "krowk connect [vendor] [--method subscription|api-key|device] [--name N]",
                "Connect a model: Claude, ChatGPT, SuperGrok or an API key",
            )
        },
        Command {
            args: vec![arg("instance", "The instance to sign out, e.g. claude:work — a person at a terminal may omit it and pick", false)],
            flags: vec![
                flag("remove", BOOL, "Remove its definition too; without it the instance stays, not signed in"),
                flag("sign-out-vendor", BOOL, "The built-in claude or codex: yes, sign me out of Claude Code or Codex itself (~/.claude, ~/.codex), for every tool. Asked at a terminal; required without one"),
            ],
            ..cmd("disconnect", "krowk disconnect [instance] [--remove]", "Sign a connected model out")
        },
    ]
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
                    flag("config-dir", STRING, "claude, codex: the CLAUDE_CONFIG_DIR or CODEX_HOME this instance signs in and keeps its sessions in; a new one in ~/.krowk/accounts/ for a named instance"),
                ],
                ..cmd(
                    "add",
                    "krowk providers add <provider> [--name N] [--api-key-env VAR] [--base-url URL] [--device] [--binary PATH] [--config-dir DIR]",
                    "Add an instance, or sign one in",
                )
            },
            cmd("list", "krowk providers list", "List every instance, and whether it is ready"),
            Command {
                args: vec![arg("instance", "The instance to remove, e.g. openai:work", true)],
                ..cmd("remove", "krowk providers remove <instance>", "Remove an instance's definition, and forget its login")
            },
            Command {
                args: vec![
                    arg("instance", "The instance to rename, e.g. claude:work — a person at a terminal may omit it and pick", false),
                    arg("new-name", "Its whole new name, e.g. claude:personal — the same <provider>: prefix; a server's has none. Asked at a terminal when absent", false),
                ],
                ..cmd("rename", "krowk providers rename [instance] [new-name]", "Rename an instance; its login and sessions follow")
            },
        ],
        ..cmd("providers", "", "Low-level instance config (add, list, rename, remove)")
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
                "With -p and the agent: default, acceptEdits, plan or bypassPermissions, as in Claude Code, or unhinged, which asks about nothing: no krowk deny rule, ask rule or protected directory holds. Without it, the settings' permissions.defaultMode; in every other mode a deny rule holds",
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
        flag(
            "daemon",
            BOOL,
            "With -p: run the turn in the host daemon (started when none runs) instead of this process, so it goes on if this process ends — `krowk host attach <session>` follows it",
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

/// What `krowk help <command>` says under the usage: everything the one-line
/// summary has no room for, by the command's whole name. Prose for a person,
/// so not in `--json`, whose shape stays the parser's.
pub fn about(name: &str) -> &'static str {
    match name {
        "push" | "uploads create" => PUSH_ABOUT,
        "uploads delete" => DELETE_ABOUT,
        "claim" => CLAIM_ABOUT,
        "login" | "auth login" => LOGIN_ABOUT,
        "logout" => "Takes the key that resolves here (what `krowk whoami` shows) off this machine.",
        "auth" => "Manage the API key — your krowk account.",
        "workspaces" => WORKSPACES_ABOUT,
        "config" => "Pins a repository, or the machine, to a workspace: `krowk help workspaces`.",
        "doctor" => "Checks the local setup, and that the registry answers.",
        "sessions budget" => BUDGET_ABOUT,
        "sessions" => "Lists every agent thread on this machine, newest first.",
        "upgrade" => "Upgrades krowk to the latest release.",
        #[cfg(feature = "harness")]
        "status" => "Also where its key or login comes from, and what fixes it. Exits 3 if none is.",
        #[cfg(feature = "harness")]
        "connect" => "A subscription signs in by the vendor's own login. Connecting again renews it.",
        #[cfg(feature = "harness")]
        "disconnect" => "\
Signs an instance out: SuperGrok's tokens are deleted, a subscription's own
logout is run, and an API key's variable is named for you to unset.",
        #[cfg(feature = "harness")]
        "sync attach" => SYNC_ATTACH_ABOUT,
        #[cfg(feature = "harness")]
        "providers add" => "\
Also signs in to SuperGrok, or adds a Claude Code or Codex account (signed in
with `claude auth login` or `codex login`).",
        #[cfg(feature = "harness")]
        "providers list" => "Where each runs, too. The same check as `krowk status`.",
        #[cfg(feature = "harness")]
        "providers" => "Below `krowk connect`: API keys, logins, Claude Code and Codex accounts.",
        #[cfg(feature = "harness")]
        "sync" => "\
Sessions are encrypted on this machine before they leave it. `init` shows the
account key as 24 words once, with its key id; `recover` takes them on a new
machine and shows the id it restored. Type them at its prompt, or pipe them
from a file (`krowk sync recover < phrase.txt`) — never `echo`, which keeps
them in your shell history. Or skip the words: `join` shows a code, and
`krowk devices approve` on a machine that already syncs answers it; read the
account key id off that machine, never from an error or a web page. The keys
are kept in krowk's home, 0600; with a key to a Pro workspace the device is
registered there too.",
        #[cfg(feature = "harness")]
        "devices" => "\
Adding a machine: run `krowk sync join` on it, then `krowk devices approve`
here and type the code it shows. Type back the account key id `approve`
shows on the new machine, or pass it to `join`. Comparing both is what keeps
a registry from slipping its own keys in. Needs a Pro workspace.",
        #[cfg(feature = "harness")]
        "host" => "\
The first krowk that needs it starts the daemon, and it exits after ten idle
minutes (host.idleMinutes in config.json, or KROWK_HOST_IDLE seconds).
`krowk host enable` keeps it running instead, for an always-on machine.",
        _ => "",
    }
}

/// A heading in the human help, and the commands under it in reading order.
/// `krowk help` lists each command; `krowk help --all` each with everything
/// under it. Every command the build has sits under exactly one heading.
pub const GROUPS: &[(&str, &[&str])] = &[
    // The harness build's own commands, and the sessions they run.
    #[cfg(feature = "harness")]
    ("AGENT", &["connect", "disconnect", "status", "sessions"]),
    ("PUBLISH", &["push", "runs", "uploads", "claim"]),
    ("ACCOUNT", &["login", "logout", "whoami", "workspaces", "auth"]),
    (
        "OTHER",
        &[
            #[cfg(feature = "harness")]
            "providers",
            #[cfg(all(feature = "harness", unix))]
            "host",
            #[cfg(all(feature = "harness", unix))]
            "hosts",
            #[cfg(all(feature = "harness", unix))]
            "relay",
            #[cfg(feature = "harness")]
            "sync",
            #[cfg(feature = "harness")]
            "devices",
            #[cfg(all(feature = "sessions", not(feature = "harness")))]
            "sessions",
            "config",
            "doctor",
            #[cfg(feature = "sessions")]
            "pricing",
            "upgrade",
            "help",
        ],
    ),
];

/// Listed by `krowk help --all` alone: the long forms of what the overview
/// already lists, help itself, and the relay, which a person runs only to
/// host one.
pub const ALL_ONLY: &[&str] = &[
    "auth",
    "help",
    #[cfg(all(feature = "harness", unix))]
    "relay",
    // Only for a tailnet whose machines are tagged for krowk.
    #[cfg(all(feature = "harness", unix))]
    "hosts",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_in_the_build_sits_under_exactly_one_heading() {
        let c = catalog("dev");
        let mut listed: Vec<&str> = GROUPS.iter().flat_map(|(_, names)| names.iter().copied()).collect();
        // The agent build's catalog still names what it leaves out, so a
        // command from another build is refused as not in this one.
        let mut commands: Vec<&str> = c
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| cfg!(feature = "sessions") || !matches!(*n, "sessions" | "pricing"))
            .collect();
        listed.sort_unstable();
        commands.sort_unstable();
        assert_eq!(listed, commands);
        assert!(ALL_ONLY.iter().all(|n| listed.contains(n)));
    }

    #[test]
    fn the_core_flags_come_first() {
        assert_eq!(core_flags().len(), CORE_FLAGS);
        assert_eq!(catalog("dev").global_flags[CORE_FLAGS - 1].name, "version");
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
