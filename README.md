<a href="https://krowk.com"><img src=".github/logo.svg" alt="Gruntas" width="56" /></a>

# Gruntas

Plumbing for independent agents: they travel between your machines to use compute, get tooling that makes them better at the work, and nobody has to watch them.

<a href="https://github.com/gruntas/gruntas/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/gruntas/gruntas?color=1a1a19"></a>
<a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-1a1a19"></a>

---

Gruntas is plumbing for agents running in other harnesses, built for power users who run coding agents all day. It has no interface of its own: a command line, a background daemon and the agent engines. It drives Claude Code and Codex, on your existing subscription or an API key (an Anthropic key or an Anthropic-compatible gateway runs in Claude Code; an OpenAI, xAI, OpenRouter or OpenAI-compatible key in Codex), and reads your Claude Code permissions, skills and hooks as they are.

Sessions follow you across machines, end-to-end encrypted, and your machines share the work. Agents publish screenshots, diffs and logs as links that unfurl in GitHub, Slack, Linear and Basecamp.

## Install

```bash
curl -fsSL https://krowk.com/install | bash
npx @gruntas/cli
cargo install --locked --git https://github.com/gruntas/gruntas --features harness gruntas
```

Builds are published for Linux and macOS (amd64/arm64) and Windows (amd64). On a workstation the installer gives you the full build, with the agent. In CI and containers it gives you the lean build, which only publishes. `--full` and `--lean` choose; see [the install docs](https://krowk.com/docs#install).

**Coming from krowk?** Install gruntas fresh (a krowk binary can't upgrade itself into it). The first `gruntas` run copies `~/.krowk` into `~/.gruntas` once, so you stay signed in with your sessions and accounts. Old `krowk@krowk` plugin installs reinstall as `gruntas@gruntas`. [CHANGELOG.md](CHANGELOG.md) has the full list.

## Three moments

**Your first machine.** Run `gruntas` in a terminal. It asks you to sign in in the browser, then to connect a model: a Claude Code or Codex account, on a Claude or ChatGPT subscription, an API key, or your own Claude Code or Codex login. Then it asks whether to keep the machine available while you're away, and offers your recovery kit. That is the whole setup; after it, `gruntas` prints an overview of the commands and anything that needs fixing.

```bash
gruntas                          # set this machine up, then an overview
gruntas -p "fix the build"       # run one prompt headless (--output-format json)
gruntas -p --resume <id> "go on" # continue an earlier session
```

`gruntas -p` asks before it runs a repository's own hooks and MCP servers, and asks again when one of them changes; `gruntas trust` answers for good.

**Another machine.** Install gruntas and run `gruntas`. Sign in there and type the code `gruntas machines` shows on a machine you already have. The new machine joins your account, and your sessions are there.

**Your phone.** The phone app is not part of this release.

## More

- **Sessions.** The full build keeps a local store of every agent session on the machine, whether it came from gruntas, Claude Code, Cursor or opencode, and lists the ones on your other machines too. `gruntas sessions` lists them and `gruntas -p --resume` continues one. [Sessions](https://krowk.com/docs#sessions)
- **Machines.** `gruntas machines` shows each machine's load, free memory and slots, and whether it accepts work from your others. `always-on` keeps one available and `recovery` guards your recovery kit. [Machines](https://krowk.com/docs#machines)
- **Worktrees.** Agents running at once each work in a git worktree of their own, so they never overwrite each other. `gruntas worktrees` lists, applies and removes them. [Worktrees](https://krowk.com/docs#worktrees)
- **Publishing.** `gruntas push screenshot.png` uploads a file and prints a link whose card shows the repo, commit, branch, PR and session. It works without an account for a day. In CI, this repository is also a GitHub Action (`uses: gruntas/gruntas@v0`). [Publishing](https://krowk.com/docs#first-push)
- **MCP.** Claude Code and Codex get, through gruntas's bridge, the MCP servers you set up for Claude Code. `gruntas mcp add`, `login` and `list` manage them, and `gruntas-mcp` serves gruntas itself to other MCP clients. [MCP servers](https://krowk.com/docs#mcp), [signing in to OAuth servers](https://krowk.com/docs#mcp-sign-in)
- **Accounts.** gruntas no longer runs models itself, and the SuperGrok sign-in is gone: every session runs in Claude Code or Codex. API keys and gateways keep working as Claude Code or Codex accounts, which `gruntas connect` makes of them, and `gruntas providers` lists and renames them.
- **Configuration.** Everything lives in `~/.gruntas/`, and a repository can commit `.gruntas/config.json`. `gruntas config` reads and sets it, environment variables are `GRUNTAS_*`, `gruntas status` and `gruntas doctor` check health and `gruntas upgrade` updates. [Configuration](https://krowk.com/docs#configuration)

`gruntas help` lists the commands, `gruntas help --all` every one, and `--json` gives them as data. [`skills/gruntas/SKILL.md`](skills/gruntas/SKILL.md) teaches another agent to use gruntas, and in Claude Code `/plugin marketplace add gruntas/gruntas` and `/plugin install gruntas@gruntas` add the skill and the MCP server.

## Development

Build, test and benchmark instructions are in [the development docs](https://krowk.com/docs#development). See [CHANGELOG.md](CHANGELOG.md) for what changed in each release.

## Who uses Gruntas?

- [Primevise](https://primevise.com)
- [Rinkta](https://rinkta.com)

## License

MIT
