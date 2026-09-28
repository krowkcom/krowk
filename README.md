<a href="https://krowk.com"><img src=".github/logo.svg" alt="Krowk" width="56" /></a>

# Krowk

Permalinks for agent output. Push a screenshot, diff or log and get a URL that unfurls in GitHub, Slack, Linear and Basecamp, with the run metadata attached. The CLI also includes a terminal coding agent and a local store of every agent session on your machine.

<a href="https://github.com/krowkcom/krowk/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/krowkcom/krowk?color=1a1a19"></a>
<a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-1a1a19"></a>

---

```bash
krowk push screenshot.png --pull-request="https://github.com/acme/storefront/pull/412"
```

```
✓ Uploaded screenshot.png → https://krowk.com/a/art_2e1d
  412 KB · run run_8Kd2wq
```

- **Built for agents.** Output is JSON when piped, `krowk help --json` describes every command, and each result includes the follow-up commands to run next.
- **No setup needed.** You can push without a key. Anonymous uploads last 24 hours and can be claimed into a workspace.
- **Context attached.** The repo, commit, branch, PR, session and agent are detected from git and CI.
- **Small and fast.** A single Rust binary, with a lean build for CI and agent containers.

## Install

```bash
curl -fsSL https://krowk.com/install | bash   # also installs the agent skill
npx @krowk/cli push screenshot.png            # npm
cargo install --locked --git https://github.com/krowkcom/krowk --features harness krowk
```

Builds are published for Linux and macOS (amd64/arm64) and Windows (amd64). The installer gives a workstation the **full build**, which includes the agent and the session store. CI and containers get the **lean build**, which only handles push, runs and uploads. Pass `--full` or `--lean` to choose (`curl … | bash -s -- --full`).

## Usage

| Command | What it does |
| --- | --- |
| `krowk push <file...>` | Upload files and print a link for each |
| `krowk runs start` / `finish` / `list` / `show` | Group uploads under a run |
| `krowk uploads list` / `show` / `attach` / `delete` | Manage uploads |
| `krowk claim <artifact> <token>` | Keep an anonymous upload past its 24h expiry |
| `krowk login` / `logout` / `whoami` | Sign in to your workspace (`--token` for CI) |
| `krowk config set workspace <ws>` | Pin a repository (or `--global`, the machine) to a workspace |
| `krowk sessions` | Browse, import and sync Claude Code, Cursor and opencode sessions |
| `krowk sessions budget <id> --max-usd 5` | Exit 4 when a session costs more than the limit, for use in hooks |
| `krowk doctor` / `upgrade` | Check your setup, or upgrade to the latest release |

Useful `push` flags include `--run`, `--title`, `--caption`, `--link`, `--private` (visible only to your workspace), `--metadata key=value`, and `--destination github|slack|linear|…`, which prints the paste format that tool expects. Run `krowk help` for the full list.

## The agent

In the full build, running `krowk` with no arguments opens an inline coding agent in your terminal. It has no alternate screen, so its output stays in your normal scrollback.

```bash
krowk connect            # Claude or ChatGPT subscription, SuperGrok, or an API key
krowk                    # open the agent
krowk -p "fix the build" # one prompt, headless (--output-format json)
krowk --resume           # continue a session
```

- Tools for reading, writing, editing, bash, grep and glob, plus `publish`, which pushes an artifact to krowk. It also keeps a todo list and can run parallel subagents.
- Permissions work the same way as Claude Code's: the same modes, the same `allow`/`ask`/`deny` rules and the same settings files. There is also an `unhinged` mode that asks about nothing.
- It reads `AGENTS.md`, `CLAUDE.md`, `.cursor/rules`, Claude-format skills, hooks and `.claude/agents`.
- Type `/` in the prompt to list commands and `?` to see the keys.

## GitHub Action

```yaml
- uses: krowkcom/krowk@v0
  id: krowk
  with:
    files: screenshots/**/*.png
    token: ${{ secrets.KROWK_TOKEN }}
```

The action provides `urls`, `markdown` (ready to post as a PR comment), `run-slug` and `json` as outputs. The links also appear in the job summary.

## For AI agents

- [`skills/krowk/SKILL.md`](skills/krowk/SKILL.md) teaches an agent how to use krowk. The installer adds it to `~/.claude/skills`.
- `--json` returns a single envelope shape for every command, and `--jq '<expr>'` filters it without needing jq installed: `URL=$(krowk push shot.png --jq '.data.artifacts[0].url')`.
- Exit codes: `0` ok · `1` bad command · `2` not found · `3` needs credentials · `4` refused · `5` rate limited · `6` transfer failed · `7` server error · `8` gone.
- A claim token is a one-shot secret. Never post it anywhere public.

For agents that can't run shell commands, `krowk-mcp` is included in the same install and serves the same client over MCP stdio:

```bash
claude mcp add krowk -- krowk-mcp
```

## Configuration

Everything krowk keeps is in `~/.krowk/` (move it with `KROWK_HOME`). Settings go in `config.json` and keys in `credentials.json`. A repository can commit `.krowk/config.json` to pin a workspace.

| Variable | Purpose |
| --- | --- |
| `KROWK_TOKEN` | API token (takes precedence over stored credentials) |
| `KROWK_WORKSPACE` | Which stored workspace key to use |
| `KROWK_API_URL` | Point at a self-hosted registry |
| `KROWK_AGENT` / `KROWK_MODEL` | Override the detected agent or model |
| `KROWK_NO_UPDATE_CHECK` | `1` disables release checks |

Metadata attached to an upload is public, so never put a secret in it.

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
