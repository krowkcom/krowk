<a href="https://gruntas.com"><img src=".github/logo.svg" alt="Gruntas" width="56" /></a>

# Gruntas

Run Claude Code and Codex across every machine and account you have, and keep them working while you're away.

<a href="https://github.com/gruntas/gruntas/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/gruntas/gruntas?color=1a1a19"></a>
<a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-1a1a19"></a>

---

Gruntas sits underneath the agents you already run. Keep your harness, subscription, permissions, skills and hooks; gruntas adds machines, accounts and a way to show the work.

- **Every machine.** Pair your machines and they share the work. Sessions follow you between them, end-to-end encrypted.
- **Every account.** When one account hits its limit, the session continues on the next, on Claude Code or Codex, without losing its place.
- **Every agent, in view.** One tree of every agent on every machine. Any agent can read, message or interrupt the others, within limits you set.
- **Evidence, not screen-watching.** Agents publish screenshots, diffs and logs as links that unfurl in GitHub, Slack, Linear and Basecamp.

## Install

```bash
curl -fsSL https://gruntas.com/install | bash
```

Or `npx @gruntas/cli`, or `cargo install --locked --git https://github.com/gruntas/gruntas --features harness gruntas`. Builds ship for Linux and macOS (amd64, arm64) and Windows (amd64). [Install docs](https://gruntas.com/docs#install)

Then add the plugin, which registers each session with gruntas:

```text
# Claude Code
/plugin marketplace add gruntas/gruntas
/plugin install gruntas@gruntas

# Codex (then approve its hooks once in /hooks)
codex plugin marketplace add gruntas/gruntas
codex plugin add gruntas@gruntas
```

## Get started

```bash
gruntas                              # set up this machine: sign in, connect an account
gruntas run claude --account auto    # Claude Code on the account with the most room
gruntas run claude --unattended "fix the flaky test"   # a background worker
gruntas status                       # every agent on every machine
gruntas push screenshot.png          # a link to paste where people will read it
```

On another machine, install gruntas, run `gruntas`, and type the code `gruntas machines` shows on one you already have. `gruntas run` and `gruntas status` need Linux or macOS.

## Learn more

- **[Sessions](https://gruntas.com/docs#sessions)** — every agent session on the machine, from gruntas, Claude Code, Cursor or opencode, and the ones on your other machines.
- **[Machines](https://gruntas.com/docs#machines)** — load, memory and free slots, and which machines take work from the others.
- **[Worktrees](https://gruntas.com/docs#worktrees)** — agents running at once each get their own, so they never overwrite each other.
- **[Publishing](https://gruntas.com/docs#first-push)** — links that carry the repo, commit, branch, PR and session. Works without an account for a day, and in CI as a GitHub Action (`uses: gruntas/gruntas@v0`).
- **[MCP](https://gruntas.com/docs#mcp)** — the MCP servers you set up for Claude Code, available to Codex too.
- **[Configuration](https://gruntas.com/docs#configuration)** — `~/.gruntas/`, `.gruntas/config.json` per repository, `GRUNTAS_*` variables.

`gruntas help --all` lists every command. [`skills/gruntas/SKILL.md`](skills/gruntas/SKILL.md) teaches an agent to use gruntas.

## Versions

Calendar versions, `YYYY.M.N`. Breaking changes are announced in [CHANGELOG.md](CHANGELOG.md) at least one release ahead, and each release is supported for a year. `uses: gruntas/gruntas@v0` follows every stable release; pin a tag to stay on one.

## License

MIT
