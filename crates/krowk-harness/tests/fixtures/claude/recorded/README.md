# Recorded Claude Code sessions

Live recordings of `claude` 2.1.289 (Claude Code), for what the fake
`claude` beside them can't settle: what Claude Code itself does. Each line is
`>> ` and what was written to its stdin, or `<< ` and what it printed, in the
order they happened. The control protocol (`initialize`, the `krowk` MCP
server's handshake, `can_use_tool` answered allow) is left out, and so are the
`init` line's skills, plugins and memory path; the session id is
`{{SESSION}}` and the working directory `{{CWD}}`, as in the scenarios.

The command, krowk's flags plus `--replay-user-messages`:

```text
claude -p --input-format stream-json --output-format stream-json --verbose
       --include-partial-messages --replay-user-messages --permission-prompt-tool stdio
       --mcp-config '{"mcpServers":{"krowk":{"type":"sdk","name":"krowk"}}}' --strict-mcp-config
       --settings '{"attribution":{"commit":"","pr":""},"includeCoAuthoredBy":false}'
       --model claude-haiku-4-5-20251001 --permission-mode default
```

- `steer_mid_turn.txt`: a second `user` line written 5 s into a 20 s `Bash`
  call is replayed (`"isReplay": true`) right after that call's `tool_result`,
  and the model reads it in the same turn, which ends with one `result`.
- `steer_late.txt`: a second `user` line written after the last tool call,
  while the model writes its final answer, is not read in that turn; after
  its `result` Claude Code begins another turn by itself (`init`, the line
  replayed, the answer, a second `result` with no `origin`).
- `steer_interrupt.txt`: lines written with a `uuid`, and a second one 5 s
  into the `Bash` call, then at 8 s an `interrupt` with `cancel_queued: true`
  (Claude Code announces `interrupt_cancel_queued_v1`): its answer lists the
  unread line under `cancelled`, and no turn follows for it. Without
  `cancel_queued` the line survives the interrupt and Claude Code begins a
  turn by itself to read it. Its control lines for the interrupt are kept.

The prompt is replayed too, as the first line after `init`. A line written
with a `uuid` gets `command_lifecycle` lines naming it (`queued`, `started`,
`cancelled`).
