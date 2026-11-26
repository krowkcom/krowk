# @gruntas/pi

Gruntas for [pi](https://pi.dev). Each pi event the extension handles runs
`gruntas hook pi <event>`, the entry point gruntas's Claude Code and Codex
plugins run, so a pi session is treated as theirs are.

The session also gets the gruntas MCP server, which pi starts as
`gruntas-mcp` (its tools are `mcp__gruntas__<tool>`), and the gruntas skill.

```bash
pi install npm:@gruntas/pi
```

A pi session then registers with the gruntas daemon as it starts and shows in
`gruntas agents` as harness `pi` until it ends; its tool calls count as its
activity, and a run that did work without a `gruntas_report` is reminded once
to report.

The package is a small loader: the extension itself is carried by the
`gruntas` on the PATH (`gruntas run pi` starts pi with the same one), and the
package asks gruntas for it as pi loads it, so pi always runs the extension
of the installed gruntas, at its version, and upgrading gruntas upgrades it
with nothing to do in pi. The gruntas skill comes from gruntas too, unless pi
already finds the installer's identical copy in `~/.agents/skills/gruntas`.

It needs `gruntas` on the PATH: without it, a session's start says how to
install it and the package does nothing else. On Windows it does nothing,
and under `gruntas run pi`, which loads the extension itself, it stands
aside.

## Claude on your gruntas accounts

The extension adds a `gruntas` provider to pi's `/model`: `gruntas/<model>`,
such as `gruntas/claude-sonnet-5-5`. Each turn runs through Claude Code via
the Claude Agent SDK, on one of your gruntas accounts (a Claude subscription,
or an API-key or gateway account gruntas runs in Claude Code), started by
`gruntas run claude --account <name> --no-plugin`. The login or key stays in
gruntas: nothing to log in to inside pi, and none of it reaches pi. Claude
Code runs with its own system prompt and its built-in tools off; pi's tools
are bridged in, and pi runs them.

A session starts on the account it used last (kept in pi's config home,
`gruntas/account.json`), else on the ready account rollover ranks first.
`/account` picks another: it lists every gruntas account with its kind,
readiness and usage left per window, and the next request runs on the one
you choose. `/account <name>` moves directly, also where pi has no UI. The
footer shows `gruntas: <account>` while a `gruntas/` model is in use. Codex
accounts are listed but not usable from pi yet.

The provider is adapted from the MIT-licensed
[pi-claude-subscription](https://github.com/jkeskikangas/pi-claude-subscription);
see THIRD-PARTY-NOTICES in the gruntas repository, which also covers what the
carried extension bundles.

Supported pi: 1.6 and later 1.x (`engines.pi`).

MIT.
