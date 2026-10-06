<a href="https://krowk.com"><img src=".github/logo.svg" alt="Krowk" width="56" /></a>

# Krowk

A coding agent harness: one session, on any machine, model or agent.

<a href="https://github.com/krowkcom/krowk/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/krowkcom/krowk?color=1a1a19"></a>
<a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-1a1a19"></a>

---

Krowk is an agent harness built for power users who run coding agents all day. It runs Claude, GPT, Grok or any OpenAI-compatible model, using your existing subscription or an API key, or drives Claude Code and Codex. A session can move to another model or agent mid-conversation, and syncs to your other machines end-to-end encrypted. It follows Claude Code's permissions, skills and hooks, keeps track of every agent session on your machine, and publishes screenshots, diffs and logs as links that unfurl in GitHub, Slack, Linear and Basecamp. It assumes you're comfortable in a terminal, with git and with configuration files.

```bash
krowk connect            # sign in with a Claude, ChatGPT or SuperGrok subscription, or an API key
krowk                    # open the agent
krowk -p "fix the build" # run one prompt headless (--output-format json)
krowk --resume           # continue an earlier session
```

## Install

```bash
curl -fsSL https://krowk.com/install | bash
npx @krowk/cli
cargo install --locked --git https://github.com/krowkcom/krowk --features harness krowk
```

Builds are published for Linux and macOS (amd64/arm64) and Windows (amd64). On a workstation the installer gives you the **full build**, which includes the agent. In CI and containers it gives you the **lean build**, which only publishes. Pass `--full` or `--lean` to choose (`curl … | bash -s -- --full`).

## The agent

- **Inline, not full-screen.** The agent draws at the bottom of your terminal and leaves everything else in your normal scrollback, so output can be scrolled, copied and searched, including over SSH and in tmux.
- **Tools.** It can read, write, edit, run bash, grep, glob and publish. It also keeps a todo list, runs subagents in parallel, each with its own context and a cheaper model, and asks you with options to pick from when a choice is yours.
- **Compatible with Claude Code.** It uses the same permission modes, `allow`/`ask`/`deny` rules and settings files. It reads `AGENTS.md`, `CLAUDE.md` and `.cursor/rules`, and loads Claude-format skills, hooks and `.claude/agents` without changes. It adds an `unhinged` mode that never asks.
- **Multiple accounts.** You can connect several accounts per provider and rename them in `/connect`. A second account is added with `--name work`.
- **Cost limits.** `--max-usd` and `--max-tokens` stop a session before it goes over a limit, subagents included.

In the prompt, `/` lists the commands and `?` shows the keys.

## Sessions

The full build keeps a local store of your agent sessions, whether they came from krowk, Claude Code, Cursor or opencode.

| Command | What it does |
| --- | --- |
| `krowk sessions` | List every session on this machine, newest first |
| `krowk sessions show <id>` | Read a session back, turn by turn |
| `krowk sessions import` / `sync` | Import transcripts and reconcile usage |
| `krowk sessions budget <id> --max-usd 5` | Exit 4 when a session costs more than the limit, for use in a hook |

## Sync

Sessions sync between your machines end-to-end encrypted: neither krowk's servers nor its relay can read them. Run a session on one machine, then follow it and prompt it from another.

| Command | What it does |
| --- | --- |
| `krowk sync init` | Set up sync on this machine, with a recovery kit |
| `krowk devices add` / `krowk sync join` | Show a code on one machine, enter it on the new one |
| `krowk sync host <session>` | Run a session here and sync it |
| `krowk sync sessions` | The synced sessions this machine can open |
| `krowk sync attach <session>` | Follow a synced session in the TUI |

## Publishing

```bash
krowk push screenshot.png --pull-request="https://github.com/acme/storefront/pull/412"
```

```
✓ Uploaded screenshot.png → https://krowk.com/a/art_2e1d
  412 KB · run run_8Kd2wq
```

Each link shows a card with the repo, commit, branch, PR, session and agent, all detected from git and CI. You can push without a key: anonymous uploads last 24 hours and can be claimed into a workspace with `krowk claim`.

| Command | What it does |
| --- | --- |
| `krowk push <file...>` | Upload files and print a link for each |
| `krowk runs` / `uploads` | Group, list, attach and delete uploads |
| `krowk login` / `whoami` | Sign in to a workspace (`--token` for CI) |
| `krowk config set workspace <ws>` | Pin a repository to a workspace |

Useful `push` flags include `--private`, `--caption`, `--link`, `--metadata key=value`, and `--destination github|slack|linear|…`, which prints the paste format that tool expects. Metadata is public, so never put a secret in it.

In CI, the repository doubles as a GitHub Action:

```yaml
- uses: krowkcom/krowk@v0
  id: krowk
  with:
    files: screenshots/**/*.png
    token: ${{ secrets.KROWK_TOKEN }}
```

It outputs `urls`, `markdown` (ready to post as a PR comment), `run-slug` and `json`, and adds the links to the job summary.

## Using krowk from other agents

- [`skills/krowk/SKILL.md`](skills/krowk/SKILL.md) teaches another agent how to use krowk. The installer adds it to `~/.claude/skills`.
- In Claude Code, `/plugin marketplace add krowkcom/krowk` and then `/plugin install krowk@krowk` add the skill and the MCP server together. When krowk drives Claude Code itself, the plugin's server stays off and krowk's own tools are used.
- Output is JSON when piped. `krowk help --json` describes every command, and `--jq` filters the output without needing jq installed: `URL=$(krowk push shot.png --jq '.data.artifacts[0].url')`.
- `krowk-mcp` serves the same features over MCP stdio, for any other MCP client, or for Claude Code without the plugin: `claude mcp add krowk -- krowk-mcp`.
- Exit codes: `0` ok · `1` bad command · `2` not found · `3` needs credentials · `4` refused · `5` rate limited · `6` transfer failed · `7` server error · `8` gone.

## Configuration

Everything krowk keeps is in `~/.krowk/` (move it with `KROWK_HOME`): settings in `config.json`, keys in `credentials.json`, and sessions in `sessions/`. A repository can commit `.krowk/config.json` to pin a workspace or add permission rules.

| Variable | Purpose |
| --- | --- |
| `KROWK_TOKEN` | API token (takes precedence over stored credentials) |
| `KROWK_WORKSPACE` | Which stored workspace key to use |
| `KROWK_API_URL` | Point at a self-hosted registry |
| `KROWK_NO_UPDATE_CHECK` | `1` disables release checks |

### MCP servers

krowk's own agent uses the MCP servers you already set up for Claude Code: `mcpServers` in `~/.claude.json` (user and per-project) and in Claude's `settings.json`, plus `mcpServers` in `~/.krowk/config.json`. A repository's `.mcp.json` is used only after you trust the repository, and your own servers of the same name win over it. Each server takes Claude Code's shape, with `${VAR}` and `${VAR:-default}` expanded:

```json
{
  "mcpServers": {
    "github": { "command": "github-mcp-server", "args": ["stdio"], "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" } },
    "docs":   { "type": "http", "url": "https://mcp.example.com/mcp", "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } }
  }
}
```

| Key | Purpose |
| --- | --- |
| `command`, `args`, `env` | A stdio server: the program krowk starts, and its arguments and environment |
| `type: "http"`, `url`, `headers` | A streamable-HTTP server |

The model sees two tools, `mcp_search` and `mcp_call`, however many MCP tools there are, so they cost no context until one is used. No server starts until the first search. Permission rules name a tool as `Mcp(server:tool)` or `mcp__server__tool`, and `Mcp(server)` names all of a server's tools; a server denied whole is never started.

## Development

```bash
make check   # clippy, unit tests and golden tests
make dev     # fast build, linked into ~/.cargo/bin
make mock    # local registry; run any command with --dev
```

See [CHANGELOG.md](CHANGELOG.md) for what changed in each release, and [`crates/krowk-bench`](crates/krowk-bench/README.md) for the performance and size budgets.

## Who uses Krowk?

- [Primevise](https://primevise.com)
- [Rinkta](https://rinkta.com)

## License

MIT
