<a href="https://krowk.com"><img src=".github/logo.svg" alt="Krowk" width="56" /></a>

# Krowk

Plumbing for independent agents: they travel between your machines to use compute, get tooling that makes them better at the work, and nobody has to watch them.

<a href="https://github.com/krowkcom/krowk/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/krowkcom/krowk?color=1a1a19"></a>
<a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-1a1a19"></a>

---

Krowk is plumbing for agents running in other harnesses, built for power users who run coding agents all day. It has no interface of its own: a command line, a background daemon and the agent engines. It drives Claude Code and Codex, on your existing subscription or an API key (an Anthropic key or an Anthropic-compatible gateway runs in Claude Code; an OpenAI, xAI, OpenRouter or OpenAI-compatible key in Codex), and reads your Claude Code permissions, skills and hooks as they are.

Sessions follow you across machines, end-to-end encrypted, and your machines share the work. Agents publish screenshots, diffs and logs as links that unfurl in GitHub, Slack, Linear and Basecamp.

## Install

```bash
curl -fsSL https://krowk.com/install | bash
npx @krowk/cli
cargo install --locked --git https://github.com/krowkcom/krowk --features harness krowk
```

Builds are published for Linux and macOS (amd64/arm64) and Windows (amd64). On a workstation the installer gives you the full build, with the agent. In CI and containers it gives you the lean build, which only publishes. `--full` and `--lean` choose; see [the install docs](https://krowk.com/docs#install).

## Three moments

**Your first machine.** Run `krowk` in a terminal. It asks you to sign in in the browser, then to connect a model: a Claude Code or Codex account, on a Claude or ChatGPT subscription, an API key, or your own Claude Code or Codex login. Then it asks whether to keep the machine available while you're away, and offers your recovery kit. That is the whole setup; after it, `krowk` prints an overview of the commands and anything that needs fixing.

```bash
krowk                          # set this machine up, then an overview
krowk -p "fix the build"       # run one prompt headless (--output-format json)
krowk -p --resume <id> "go on" # continue an earlier session
```

`krowk -p` asks before it runs a repository's own hooks and MCP servers, and asks again when one of them changes; `krowk trust` answers for good.

**Another machine.** Install krowk and run `krowk`. Sign in there and type the code `krowk machines` shows on a machine you already have. The new machine joins your account, and your sessions are there.

**Your phone.** The phone app is not part of this release.

## More

- **Sessions.** The full build keeps a local store of every agent session on the machine, whether it came from krowk, Claude Code, Cursor or opencode, and lists the ones on your other machines too. `krowk sessions` lists them and `krowk -p --resume` continues one. [Sessions](https://krowk.com/docs#sessions)
- **Machines.** `krowk machines` shows each machine's load, free memory and slots, and whether it accepts work from your others. `always-on` keeps one available and `recovery` guards your recovery kit. [Machines](https://krowk.com/docs#machines)
- **Worktrees.** Agents running at once each work in a git worktree of their own, so they never overwrite each other. `krowk worktrees` lists, applies and removes them. [Worktrees](https://krowk.com/docs#worktrees)
- **Publishing.** `krowk push screenshot.png` uploads a file and prints a link whose card shows the repo, commit, branch, PR and session. It works without an account for a day. In CI, this repository is also a GitHub Action (`uses: krowkcom/krowk@v0`). [Publishing](https://krowk.com/docs#first-push)
- **MCP.** Claude Code and Codex get, through krowk's bridge, the MCP servers you set up for Claude Code. `krowk mcp add`, `login` and `list` manage them, and `krowk-mcp` serves krowk itself to other MCP clients. [MCP servers](https://krowk.com/docs#mcp), [signing in to OAuth servers](https://krowk.com/docs#mcp-sign-in)
- **Accounts.** krowk no longer runs models itself, and the SuperGrok sign-in is gone: every session runs in Claude Code or Codex. API keys and gateways keep working as Claude Code or Codex accounts, which `krowk connect` makes of them, and `krowk providers` lists and renames them.
- **Configuration.** Everything lives in `~/.krowk/`, and a repository can commit `.krowk/config.json`. `krowk config` reads and sets it; `krowk status` and `krowk doctor` check health and `krowk upgrade` updates. [Configuration](https://krowk.com/docs#configuration)

`krowk help` lists the commands, `krowk help --all` every one, and `--json` gives them as data. [`skills/krowk/SKILL.md`](skills/krowk/SKILL.md) teaches another agent to use krowk, and in Claude Code `/plugin marketplace add krowkcom/krowk` adds the skill and the MCP server.

## Development

Build, test and benchmark instructions are in [the development docs](https://krowk.com/docs#development). See [CHANGELOG.md](CHANGELOG.md) for what changed in each release.

## Who uses Krowk?

- [Primevise](https://primevise.com)
- [Rinkta](https://rinkta.com)

## License

MIT
