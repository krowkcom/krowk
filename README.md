<a href="https://krowk.com"><img src=".github/logo.svg" alt="Krowk" width="56" /></a>

# Krowk

Permalinks for agent output. Push a screenshot, get a URL that unfurls in GitHub, Slack, Basecamp and Linear — with the run metadata attached.

<a href="https://github.com/krowkcom/krowk/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/krowkcom/krowk?color=1a1a19"></a>
<a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-1a1a19"></a>

---

```bash
krowk push screenshot.png \
  --pull-request="https://github.com/acme/storefront/pull/412" \
  --session="3fe6808d-088d-4a6f-a04c-cc9690bcf852"
```

```
✓ Uploaded screenshot.png → https://krowk.com/a/art_2e1d
  412 KB · run run_8Kd2wq
```

### Features

- **Built for agents** — JSON output when piped, a machine-readable command surface, ready-to-run follow-up commands in every result
- **Zero setup** — push without a key; the upload works instantly and can be claimed into your workspace later
- **Context attached** — repo, commit, branch, PR and agent are detected from git and CI, so links carry their provenance
- **One static binary** — Go; no runtime to install in an agent container

## Installation

```bash
# The installer — picks your platform, verifies checksums, installs the agent skill
curl -fsSL https://krowk.com/install | bash

# From source (the full build)
cargo install --locked --git https://github.com/krowkcom/krowk --features harness krowk

# npm
npx @krowk/cli push screenshot.png
```

Linux and macOS (amd64/arm64), Windows (amd64). Every release ships `checksums.txt`.

Every release has two builds of `krowk`. The **full build** carries krowk's own agent (bare `krowk` opens it), the session store and everything else; the **lean build** is the few-megabyte agent-container build — push, runs, uploads, no SQLite. The installer gives a workstation the full build and CI or a container (it reads `CI`, `GITHUB_ACTIONS` and the like, and `/.dockerenv`, `/run/.containerenv`, `$container`) the lean one. Ask for either with `bash -s -- --lean` / `--full`, or `KROWK_LEAN=1` / `0`. `krowk upgrade` stays on the build it is; the GitHub Action installs the lean build. A shell with no terminal (a Dockerfile `RUN`, a provisioning script) counts as a container too, and so do toolbox and distrobox: there, pass `--full` for the agent.

## Usage

| Command | What it does |
| --- | --- |
| `krowk push <file...>` | Upload files, get a link for each |
| `krowk runs start` / `finish` | Open and close a run to group uploads under |
| `krowk runs list` / `show <run>` | Browse runs and everything recorded on them |
| `krowk uploads list` / `show <artifact>` | Browse uploads — the workspace's, or one run's with `--run` |
| `krowk uploads attach <artifact> --run <run>` | Put an upload under a run after the fact |
| `krowk uploads delete <artifact>` | Take an upload down — immediate and unrecoverable |
| `krowk claim <artifact> <token>` | Keep an anonymous upload past its 24h expiry |
| `krowk auth login` | Approve this machine in a browser (`--token` for CI) — one stored key per workspace |
| `krowk workspaces list` / `use ws_9hj3kd8a` | List the stored keys, or make one the machine-wide default — `use` with no name picks from a list |
| `krowk config set workspace ws_9hj3kd8a` | Pin this repository to a workspace (`--global` for the machine) |
| `krowk config show` / `unset <key>` | The effective configuration and which layer set it, or remove a value |
| `krowk doctor` | Report version, connectivity, auth and detected run context |
| `krowk sessions` | List every agent thread on this machine, newest first (`--harness`, `--worktree`, `--limit N`, `--all`) — picks from a list on a terminal |
| `krowk sessions show <id>` | Read one session back, with its turns, messages and parts (`--thinking`) |
| `krowk sessions budget <id> --max-usd 5` | Exit 4 when a session's metered cost, or its generated tokens (`--max-tokens N`), subagents included, are over a limit — for a hook or wrapper to stop the run (a Claude Code hook blocks on exit 2: `…; [ $? -ne 4 ] \|\| exit 2` blocks on a trip alone) |
| `krowk sessions import --from all` | Read Claude, Cursor and opencode transcripts on this machine into the local store, and reconcile provider usage ledgers against them (`--dry-run`, `--limit N`) |
| `krowk sessions sync` | Import only what changed since the last import, and refresh prices if they are a day old (`--no-network`) |
| `krowk sessions rebuild` | Delete the local store and re-import every transcript — the fix when the store's schema version does not match (`--yes` off a terminal) |
| `krowk pricing refresh` | Refresh the models.dev price cache (conditional GET, silent on failure) |
| `krowk upgrade` | Upgrade krowk to the latest release |

### The agent

In the full build, bare `krowk` on a terminal opens krowk's own agent: an inline prompt at the bottom of your terminal, with everything finished going into the terminal's normal scrollback — no alternate screen, so it scrolls, copies and searches like any other output, over SSH, in tmux and on a phone. With stdout or stdin not a terminal, bare `krowk` prints exactly what it always did.

| | |
| --- | --- |
| `krowk` | Open the agent (`--model <instance>/<model>`, `--permission-mode`) |
| `krowk --resume` / `--resume <id>` | Continue a krowk session — picked from a list, or named |
| `krowk -p "…"` | One prompt, headless (`--output-format text\|json\|stream-json`, `--resume`, `--model`) |
| `krowk -p "…" --max-usd 0.50` | Stop the session before the model call that would take it, subagents included, past a metered limit (`--max-tokens N`; the TUI takes both) — exits 4 like `krowk sessions budget` |

The agent's tools are read, write, an edit tool in its model's format, bash, grep, glob and `publish`, which pushes a screenshot, diff or log as a krowk artifact with `krowk_push`'s own rules (inside the working directory, no credential files, no hard links). With an API key, a session's first publish opens a krowk run recording the session, and every artifact is tagged `krowk.session` and grouped under it.

It keeps a todo list (`todo_write`, Ctrl-T in the TUI), and hands work to subagents (`subagent`): child sessions with a fresh context and a cheaper model by default, several at once, each one line in the TUI (Ctrl-G to expand one or interrupt it alone), of which only the final summary comes back. Agent definitions — krowk's `.krowk/agents/*.md` or Claude Code's `.claude/agents/*.md`, as they are — give a subagent its instructions, model and tool allowlist.

What you see: `❯` before what you asked; the answer as it streams, in light markdown (headings, `•` lists, `code` and fenced blocks coloured), two columns in from both edges and wrapped by krowk inside that padding (drawn by moving the cursor, not with spaces, as Claude Code does); each tool call once, with its outcome — `◆ Read README.md (3 lines)`, `◆ Run cargo test` with the head and tail of its output, `◆ Edit src/main.rs +3/-1` with the removed and added lines on red and green bands, a red `◆` when it failed; thinking collapsed to `◆ Thought for 4.2s`; and `Worked for 12s · 6.2k tokens` when the turn is done. While a turn runs, one line says what it is doing (`⠋ Thinking… 3.2s │ esc to interrupt`). The visual language follows xAI's Grok Build (see THIRD-PARTY-NOTICES).

**Permissions follow Claude Code's.** The modes are `default` (asks before edits and commands), `acceptEdits`, `plan` (changes nothing) and `bypassPermissions`, plus krowk's own `unhinged`, which runs everything with no exceptions, and the rules are Claude Code's — `permissions.allow`, `ask` and `deny` with `Bash(git:*)`, `Read(./secrets/**)`, `Edit(src/**)`, `WebFetch(domain:…)`, `Mcp(server:tool)` — read from `.claude/settings.json`, `.claude/settings.local.json`, `~/.claude/settings.json`, and the `permissions` key of `~/.config/krowk/config.json` or a repository's `.krowk/config.json`. A deny rule wins in every mode, `bypassPermissions` included — every mode but `unhinged`, which no rule holds and only you can choose (`--permission-mode unhinged`, or `defaultMode` in your own settings). A repository's own allow rules, directories and hooks count once you trust it. When a call needs your say, the TUI shows it over the prompt — `y` once, `s` for the session, `p` for this project, `n` no; `krowk -p` refuses it instead of waiting, and tells the model what would allow it. `AGENTS.md`, `CLAUDE.md` and `.cursor/rules` are read from the repository root down, deeper files winning; Claude-format skills and hooks load as they are. Canon's `engineering/harness.md` (Permissions) has the evaluation order.

Enter sends; Alt-Enter, Ctrl-J or a trailing `\` starts a new line, and ↑/↓ walk the prompt history. Esc or Ctrl-C interrupts the running turn, keeping what arrived; typing while it runs steers it — the model reads it before its next step. `?` on an empty prompt shows the keys, Ctrl-O the session's details (tokens, log path), Ctrl-Y copies the last answer to the clipboard as the model wrote it — no padding, no wrapping, Ctrl-D or `/exit` quits. When the model's API cannot be reached, a persistent **no network connectivity** notice says so within two seconds, and clears when the API answers again; nothing hangs waiting for it.

Wherever a command takes `<artifact>`, `<run>` or `--run`, it takes the link as readily as the slug: paste `https://krowk.com/a/art_…` or the CDN URL under it, and the slug is read out of it.

Push flags: `--run`, `--pull-request`, `--link <url>` (repeatable, with `--link-title` / `--link-rel` describing the `--link` before them), `--reference` (repeatable, for identifiers that are not URLs), `--session`, `--title`, `--caption` (repeatable), `--destination <tool>`, `--private`, `--metadata key=value` (repeatable), plus `--repo` / `--commit` / `--agent` to override detection. Without a key, uploads land anonymously, expire in 24 hours and return a one-shot claim token.

### Private uploads

`krowk push shot.png --private` uploads where only your workspace can read it. The image still embeds anywhere — a private artifact's bytes sit on the CDN under a key whose secret segment is the whole of the authorization, which is what lets an unfurl bot, carrying nobody's session, render it in a PR comment at all. What changes is the card: `krowk.com/a/{slug}` opens only for a signed-in workspace member and answers everyone else exactly as it answers a slug that was never minted, so nothing unfurls it. The API read is gated the same way.

It needs an API key — a keyless upload lands in the shared anonymous workspace, which nobody is a member of, so there is nothing for it to be private to — and it is refused rather than published without one. Every artifact reports its own `visibility`, and krowk's paste labels stop promising a preview for a card no destination can fetch.

Switching an artifact's visibility later — from the dashboard, or `PUT /v1/artifacts/:slug/visibility`; krowk has no command for it yet — re-keys its bytes and withdraws the URL they were under. The slug never changes, so the card link that was pasted is always the card link. The byte URL is not symmetric: a private key is a fresh random secret each time, so an embed built on one dies for good when the artifact moves, while a public key is derived from the workspace and the slug rather than drawn — republishing an artifact that has not changed hands puts the bytes back at exactly the URL privatizing took away.

### Metadata

Key names follow the canon vocabulary: OpenTelemetry's where OTel has a word, `krowk.`-namespaced where it does not. Metadata is **public** — an artifact's card page is keyless, so never record a secret in it.

| Key | Source |
| --- | --- |
| `vcs.repository.name` | `--repo`, else `GITHUB_REPOSITORY`, else the `origin` remote |
| `vcs.repository.url.full` | The `origin` remote when it names a URL; dropped when it disagrees with the repository name |
| `vcs.ref.head.revision` | `--commit`, else `GITHUB_SHA`, else `git rev-parse HEAD` |
| `vcs.ref.head.name` | `git rev-parse --abbrev-ref HEAD` |
| `krowk.vcs.dirty` | Whether `git status --porcelain` names anything; omitted outside a checkout |
| `krowk.harness` | `--agent`, else `KROWK_AGENT`, else detection (`claude-code`, `cursor`, `github-actions`) |
| `gen_ai.request.model` / `gen_ai.system` | `KROWK_MODEL`, else `ANTHROPIC_MODEL`; the provider follows the model's family (or the harness), never a guess |
| `krowk.change.url` / `vcs.change.id` | `--pull-request` (or `GITHUB_REF` in a PR build); the id is derived from the URL |
| `krowk.session` | `--session`, else `KROWK_SESSION`, `CLAUDE_CODE_SESSION_ID`, `CURSOR_TRACE_ID`, `GITHUB_RUN_ID` |
| `krowk.links` | `--link` (always a list, up to 20) — each entry `{url, title?, rel?}`, labelled by `--link-title` and classified by `--link-rel` (`tracks`, `fixes`, `spec`, `discussion`, `source`, `supersedes`, or your own word) |
| `krowk.references` / `vcs.change.title` | `--reference` (always a list) for identifiers that are not URLs, `--title` — the work's title, not the paste's label |
| `krowk.caption` | `--caption` — on the artifact, not the run: what that one file shows |
| `krowk.client` | krowk itself: `krowk-cli/…` or `krowk-mcp/…` |
| anything else | `--metadata key=value` — your value wins over a detected one, standard keys included |

Every write detects at its own moment. Facts about the work — the change, the session, the links and references — live on the **run**; every push also stamps each **artifact** with the state it finds then (commit, branch, dirty, harness, client), so a file's production record travels with it wherever it is later claimed or attached. Runs recorded before this vocabulary carry flat keys (`repo`, `commit`, …); readers look for the standard key first and fall back.

### Sharing the link

Name where it is going and paste what comes out:

```
krowk push shot.png --caption "Cart before the fix" --destination github
```

`--destination github` (or `gitlab`, `linear`, `notion`, …) prints the krowk block — the image, the caption, the link through to the card. `--destination slack` (or `basecamp`, `asana`) prints the bare URL those tools unfurl into a preview card of their own. A tool krowk has not been told about gets the block, which reads as text wherever it does not render.

Which tool wants which form is the registry's table, served with the artifact and readable at `paste.destinations` in the JSON envelope — so a tool proving out reaches installs that predate it. `--format markdown` and `--format url` remain as explicit overrides.

Without a destination, ordinary human output ends with the block anyway, so the last thing on screen is the thing worth copying. Every form is computed by the registry and passed through untouched — krowk assembles no paste of its own, which is what lets the look of a krowk reference change in one deploy, for installs that already exist.

## GitHub Action

The repository doubles as an action, so CI can push what a test run produced and put the links where a reviewer will see them:

```yaml
- uses: krowkcom/krowk@v0 # or a release tag, e.g. @v0.8.0, to freeze the binary too
  id: krowk
  with:
    files: |
      screenshots/**/*.png
      recordings/*.webm
    token: ${{ secrets.KROWK_TOKEN }}

- uses: actions/github-script@v7
  if: github.event_name == 'pull_request'
  with:
    script: |
      await github.rest.issues.createComment({
        ...context.repo,
        issue_number: context.issue.number,
        body: ${{ toJSON(steps.krowk.outputs.markdown) }},
      })
```

`files` is the only required input — whitespace-separated paths or globs, so a path with a space in it has to go through a glob. The pull request, repo, commit and branch are detected from the runner's environment, exactly as they are on a laptop. `token` keeps the uploads past the keyless 24-hour expiry when the key belongs to a paid workspace; `version` pins a CLI release; `run-slug` and `title` name or open the run they group under. Linux and macOS runners.

`@v0` moves with each 0.x release and installs the latest one. `@v0.8.0` pins the action and the binary together — the installer ships inside the action, so a pinned tag has nothing left to fetch and go stale. An explicit `version` wins over either.

Outputs: `urls` (one artifact URL per line), `markdown` (the registry's paste block, ready for a PR comment), `run-slug`, and `json` (the envelope, claim tokens and breadcrumbs stripped, for anything else). The links also land in the job's step summary, clickable without any comment step.

## For AI agents

The agent skill at [`skills/krowk/SKILL.md`](skills/krowk/SKILL.md) teaches an agent the whole tool — the installer drops it into `~/.claude/skills` automatically. The essentials:

- **Output is JSON when piped** (or with `--json`): one envelope for every command — `ok`, `data`, `paste` (both forms as the registry computed them, plus the `destinations` table saying which tool wants which), `summary` and `breadcrumbs`.
- **Breadcrumbs are ready-to-run commands**, with this result's own slugs and tokens filled in. Substitute any `<placeholder>` before running — never paste one into a shell verbatim.
- **`krowk help --json`** returns the entire command surface — commands, flags, types, defaults, environment variables — so an agent discovers krowk without parsing prose. It is generated from the same catalog that routes commands, so it cannot drift.
- **`--jq '<expr>'` filters that JSON in-process** — jq compiled in, no jq binary and no pipe. It implies `--json` and reads what the command rendered: the envelope, or the bare record under `--quiet`. A string result prints unquoted, so `URL=$(krowk push shot.png --jq '.data.artifacts[0].url')` is the whole ceremony. A bad expression fails as `bad_jq` before anything is sent; one that compiles and then does not fit the result fails as `jq_failed` afterwards, saying that the command itself succeeded. `doctor` and `upgrade` answer with a bare record rather than an envelope, and `auth token` and `--version` answer with no JSON at all and refuse the flag — `krowk help --json` marks those `no_json`.
- **A claim token is a one-shot secret.** It keeps an anonymous upload; never put one in a PR comment or anywhere public.
- **Exit codes classify the failure**: `0` ok · `1` bad command · `2` not found · `3` needs credentials · `4` refused, retrying won't help · `5` rate limited · `6` transfer failed, retry · `7` server error, retry · `8` gone, don't retry.

### MCP server

`krowk-mcp` ships in the same install — the same client over MCP stdio, for agents that cannot shell out:

```jsonc
// Claude Code: .mcp.json — or `claude mcp add krowk -- krowk-mcp`
{
  "mcpServers": {
    "krowk": { "command": "krowk-mcp", "env": { "KROWK_TOKEN": "krowk_sk_..." } }
  }
}
```

Tools: `krowk_push`, `krowk_list_artifacts`, `krowk_get_artifact`, `krowk_claim_artifact`, `krowk_get_run`, `krowk_verify_key`. Every result carries both paste forms, labelled by destination — and labelled honestly for a private artifact, whose image embeds but whose card unfurls nowhere. `krowk_push` takes `private: true` for the same upload `--private` makes. `krowk_push` is confined to a root directory and refuses credential files (`.env*`, keys, `.ssh` and friends) wherever they sit, so a prompt-injected path cannot turn it into an exfiltration channel.

## Configuration

| Variable | Purpose |
| --- | --- |
| `KROWK_TOKEN` | API token — wins over the credentials file |
| `KROWK_WORKSPACE` | Workspace whose stored key to use, as if by `--workspace` |
| `KROWK_API_URL` | Point at a self-hosted registry |
| `KROWK_AGENT` | Override the detected agent name |
| `KROWK_MODEL` | Name the model doing the work (`gen_ai.request.model`) — harness-agnostic; `ANTHROPIC_MODEL` is also read |
| `KROWK_NO_UPDATE_CHECK` | `1`/`true` — never check for or mention new releases |

The agent's status line, under the prompt, reads
`elvinas/primevise-arch-1 | anthropic/claude-opus-5-5 | $21.47 | [4 tasks] | [3 subagents] | ? help`
and is configured in `~/.config/krowk/config.json`, under `tui`:

```json
{ "tui": { "statusBar": true, "statusItems": ["device", "model", "cost", "tasks", "subagents", "help"] } }
```

| Key | Purpose |
| --- | --- |
| `tui.statusBar` | `false` hides the status line. The no-network notice shows regardless |
| `tui.statusItems` | Which items the line shows, in order (all six by default): `device` (`<user>/<host>`), `model` (the instance and model, with the instance's rate limit once it is near it: `claude:work/haiku (78% of 7-day)`), `cost` (the session's, priced from models.dev), `tasks` (open todos, only while there are any), `subagents` (only while they run) and `help` (`? help`, always last). `offline` is added before the help while the API cannot be reached. The old names still read: `todos` is `tasks`, `instance` is `model`, and `connectivity` and `session` are ignored |

On a narrow terminal the items give way one at a time — the device first, then the subagents, the tasks and the cost — and then the model is cut short; `? help` stays.

A key or item the TUI does not know is named above the first prompt, and the rest still applies. Prompt history is kept beside the session logs, in `~/.local/share/krowk/tui-history.jsonl`.

Credentials from `krowk auth login` live in `~/.config/krowk/credentials.json` (0600), one key per workspace. Which key a command uses resolves in order: `--workspace` → `KROWK_WORKSPACE` → `.krowk/config.json` at the git root → `~/.config/krowk/config.json` → whichever key logged in last. Commit the repo file and everyone who clones the repository — person or agent — uploads to the right workspace without naming it; the file selects among keys already on the machine and never carries one itself.

## Development

```bash
make check          # clippy, the unit tests and every golden case
make build          # → target/release/krowk (the full build) and krowk-mcp
make dev            # a fast build of the same, linked into ~/.cargo/bin; rerun after an edit
make mock           # a local stand-in registry — then run any command with --dev
make golden-update  # re-record tests/golden/cases after an intended output change
make bench          # hold the release builds to the performance and size budgets
```

Rust (a cargo workspace under `crates/`). The repository ships the registry it develops against as a crate of its own (`crates/krowk-devregistry`, run by `make mock`), so trying krowk out needs neither the network nor a key. It is not part of any released binary.

Every performance and size number krowk promises is in [`crates/krowk-bench/budgets.toml`](crates/krowk-bench/budgets.toml); [its README](crates/krowk-bench/README.md) explains how the numbers are measured and how a pending budget is turned on.

## Who uses Krowk?

- [Primevise](https://primevise.com)
- [Rinkta](https://rinkta.com)

## License

MIT — the CLI for [krowk.com](https://krowk.com).
