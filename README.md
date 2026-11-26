<a href="https://gruntas.com"><img src=".github/logo.svg" alt="Gruntas" width="56" /></a>

# Gruntas

Run Claude Code and Codex across every machine and account you have, and keep them working while you're away.

<a href="https://github.com/gruntas/gruntas/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/gruntas/gruntas?color=1a1a19"></a>
<a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-1a1a19"></a>

---

Gruntas sits underneath the agents you already run. Keep your harness, subscription, permissions, skills and hooks; gruntas adds machines, accounts and links to the work.

- **Every machine.** Pair your machines and they share the work. Sessions follow you between them, end-to-end encrypted.
- **Every account.** When an account hits its limit, a session started with `gruntas run` moves to the next one, on Claude Code or Codex, and carries on.
- **Every agent, in view.** One tree of every agent on every machine. Agents can read, message or interrupt each other, as far as each machine's `steer` setting allows.
- **Links to the work.** Agents publish screenshots, diffs and logs as links to paste into GitHub, Slack, Linear or Basecamp.

## Install

```bash
curl -fsSL https://gruntas.com/install | bash
```

Or `npx @gruntas/cli`, or `cargo install --locked --git https://github.com/gruntas/gruntas --features harness gruntas`. Builds ship for Linux and macOS (amd64, arm64) and Windows (amd64). CI and containers get the lean build, which only publishes. [Install docs](https://gruntas.com/docs#install)

Then add the plugin, which registers each session with gruntas. It needs the full `gruntas` on your PATH.

```text
# Claude Code
/plugin marketplace add gruntas/gruntas
/plugin install gruntas@gruntas

# Codex (then approve its hooks once in /hooks)
codex plugin marketplace add gruntas/gruntas
codex plugin add gruntas@gruntas

# pi
pi install npm:@gruntas/pi
```

## Get started

```bash
gruntas                              # set up this machine: sign in, connect an account
gruntas -p "fix the build"           # one prompt, headless
gruntas run claude --account auto    # Claude Code on the account with the most room
gruntas run claude --unattended "fix the flaky test"   # a background worker
gruntas run pi --account work        # pi with gruntas in it, nothing to install; Claude turns on work
gruntas status                       # every agent on every machine
gruntas push screenshot.png          # a link to paste where people will read it
```

Accounts are Claude or ChatGPT subscriptions, or API keys: Anthropic-compatible keys run in Claude Code, OpenAI-compatible ones (OpenAI, xAI, OpenRouter) in Codex.

To add a machine, install gruntas there and run `gruntas`. Once you've signed in, enter the code `gruntas machines` shows on a machine you already have.

A repository's own hooks and MCP servers run only once you trust it (`gruntas trust`). `gruntas run`, `status` and `machines` need Linux or macOS.

## Learn more

- **[Sessions](https://gruntas.com/docs#sessions)** — every agent session on the machine, from gruntas, Claude Code, Cursor or opencode, and the ones on your other machines.
- **[Machines](https://gruntas.com/docs#machines)** — load, memory and free slots, and which machines take work from the others.
- **[Worktrees](https://gruntas.com/docs#worktrees)** — Claude Code sessions with the plugin each get their own, so they never overwrite each other.
- **[Publishing](https://gruntas.com/docs#first-push)** — links that carry the repo, commit, branch, PR and session. Anyone with the link can open it. Without an account it lasts 24 hours. In CI, use the GitHub Action (`uses: gruntas/gruntas@v0`).
- **[MCP](https://gruntas.com/docs#mcp)** — the MCP servers you set up for Claude Code, available to Codex too.
- **[Configuration](https://gruntas.com/docs#configuration)** — `~/.gruntas/`, `.gruntas/config.json` per repository, `GRUNTAS_*` variables.

`gruntas help --all` lists every command. [`skills/gruntas/SKILL.md`](skills/gruntas/SKILL.md) teaches an agent to use gruntas.

## Versions

Calendar versions, `YYYY.M.N`. Breaking changes are announced in [CHANGELOG.md](CHANGELOG.md) at least one release ahead, and each release is supported for a year. `uses: gruntas/gruntas@v0` follows every stable release; pin a tag to stay on one.

Building from source: [development docs](https://gruntas.com/docs#development).

## License

MIT
