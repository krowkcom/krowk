# @gruntas/cli

A coding agent harness: one session, on any machine, model or agent. It runs
Claude, GPT, Grok or any OpenAI-compatible model on your existing subscription
or an API key, or drives Claude Code and Codex; syncs sessions between your
machines end-to-end encrypted; follows Claude Code's permissions, skills and
hooks; and publishes screenshots, diffs and logs as links that unfurl in
GitHub, Slack, Linear and Basecamp.

```bash
npx @gruntas/cli connect     # sign in to a provider
npx @gruntas/cli             # open the agent here
npx @gruntas/cli -p "fix the build"
```

Publishing works on its own too, with no account:

```bash
npx @gruntas/cli push screenshot.png \
  --pull-request="https://github.com/acme/storefront/pull/412"
```

```
✓ Uploaded screenshot.png → https://krowk.com/a/art_2e1d
  412 KB · expires tomorrow
```

## What this package is

A launcher. gruntas itself is a single static Rust binary with no runtime and no
dependencies — this package carries the full build, the agent included; this package exists because the website says `npx @gruntas/cli push`
and some people are already in Node.

Installing it pulls one more package — `@gruntas/cli-linux-x64` or whichever
matches your machine — through the same npm registry as everything else. There
is no postinstall script and no download from a second host, so it works behind
a proxy, under `npm ci --ignore-scripts`, and off a private mirror.

If Node is not already in the picture, skip it. The binary is the primary
channel:

```bash
cargo install --locked --git https://github.com/gruntas/gruntas --features harness gruntas
# or grab an archive from https://github.com/gruntas/gruntas/releases/latest
```

## Documentation

The agent, commands, flags, output formats, MCP, and what the CLI refuses to
upload and why: <https://github.com/gruntas/gruntas>.

MIT.
