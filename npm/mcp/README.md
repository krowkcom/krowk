# @gruntas/mcp

`@gruntas/mcp` is how an agent in any MCP client (Claude Code, Codex, Cursor,
Gemini CLI, Claude Desktop) reaches what Gruntas, the coding agent harness, gives
you. It publishes what the agent produced (screenshots, recordings, diffs, logs)
as artifacts with permalinks, returned paste-ready: a markdown embed that renders
inline in GitHub, Linear and Notion, and a bare URL that unfurls into a card in
Slack and Basecamp. Publishing works on every platform with no account; anonymous
uploads expire in 24 hours, and an API key makes them permanent and adds runs and
listings. On macOS and Linux, where you run `gruntas` with sync set up, the same
agent can also see what every session on all your machines is doing, message or
interrupt one, and hand a task to a machine with room, then get the changes
applied back to its working tree.

The tools, over stdio:

- **Publishing**, everywhere: `gruntas_push`, `gruntas_list_artifacts`,
  `gruntas_get_artifact`, `gruntas_claim_artifact`, `gruntas_get_run` and
  `gruntas_verify_key`.
- **Your agents**, on macOS and Linux with gruntas running: `gruntas_status`,
  `gruntas_changes`, `gruntas_read` and `gruntas_ping` to see them;
  `gruntas_message` and `gruntas_interrupt` to steer one, as each machine's
  `steer` setting allows; `gruntas_delegate`, `gruntas_collect` and
  `gruntas_machines` to hand work to a machine with room; `gruntas_report` and
  `gruntas_session_info` for the agent's own session.

```json
{
  "mcpServers": {
    "gruntas": {
      "command": "npx",
      "args": ["-y", "@gruntas/mcp"],
      "env": { "GRUNTAS_TOKEN": "krowk_sk_..." }
    }
  }
}
```

Uploads are confined to a root directory — the working directory by default,
`--root` or `GRUNTAS_MCP_ROOT` to say otherwise. The model picks the paths, so the
boundary is what keeps an instruction hidden in a repository file from
publishing something else entirely.

## What this package is

A launcher. The server itself is a single static Rust binary, `gruntas-mcp`.
Installing this package pulls one more package — `@gruntas/cli-linux-x64` or
whichever matches your machine — through the same npm registry as everything
else. The platform packages carry both binaries, which is why the one this
server needs is named for the CLI. There
is no postinstall script and no download from a second host, so it works behind
a proxy, under `npm ci --ignore-scripts`, and off a private mirror.

If Node is not already in the picture, point the client at the binary instead:

```bash
cargo install --locked --git https://github.com/gruntas/gruntas gruntas --bin gruntas-mcp
# or grab an archive from https://github.com/gruntas/gruntas/releases/latest
```

```json
{ "mcpServers": { "gruntas": { "command": "gruntas-mcp" } } }
```

## Documentation

Tools, arguments, and the errors they return: <https://github.com/gruntas/gruntas>.

MIT.
