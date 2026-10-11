# Recorded Claude Code sessions

Live recordings of `claude` 2.1.289 (Claude Code), for what the fake
`claude` beside them can't settle: what Claude Code itself does. Each line is
`>> ` and what was written to its stdin, or `<< ` and what it printed, in the
order they happened. In the steering recordings (`steer_*.txt`) the control
protocol (`initialize`, the `krowk` MCP server's handshake, `can_use_tool`
answered allow) is left out, and so are the `init` line's skills, plugins
and memory path; the session id is `{{SESSION}}` and the working directory
`{{CWD}}`, as in the scenarios.

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

## Agents (`agents_*.txt`)

Recorded with the same `claude` 2.1.289 and the same command, on
`claude-haiku-4-5-20251001`, with `forwardSubagentText: true` in the
`initialize` request so an agent's own lines reach the stream. Unlike the
steering recordings, every control line is kept, in both directions:
`initialize`, the `krowk` MCP server's handshake (answered with no tools),
the paste guard's `hook_callback` (answered `{}`), `can_use_tool` (answered
allow) and `stop_task`. Trimmed: the `initialize` answer keeps only
`models`, `capabilities`, `current_permission_mode` and `hooks_applied`
(the account, commands and agents are gone); `init` loses its skills,
plugins, slash commands, memory path and socket path; `rate_limit_event`
keeps only its `status`; thinking signatures are `{{SIGNATURE}}`, a hook's
`transcript_path` `{{TRANSCRIPT}}` and an agent's `output_file` directory
`{{TASKS}}`. The working directory held `notes.txt`, naming a secret word.

- `agents_parallel.txt`: two `Agent` calls in one message, run in the
  foreground (`is_backgrounded: false`): "read notes" reads `notes.txt`,
  "sleeper" runs `sleep 30 && echo slept > slept.txt`. Shows
  `task_started` (with `prompt` and `tool_use_id`) before any line of that
  agent; each agent's prompt, then its finished `assistant` and `user`
  lines, carrying `parent_tool_use_id` (the `Agent` call), `subagent_type`
  and `task_description`, never a task id and never a `stream_event`; one
  `task_progress` per agent (`last_tool_name`, `usage`); the sleeper's
  `Bash` asked as `can_use_tool` with `agent_id` (its task id) and
  `tool_use_id` (its own call); and, 5 s after its `task_progress`, a
  `stop_task` for the sleeper: `task_updated` `killed` and
  `task_notification` `stopped` for it, the `success` answer, then
  `task_notification` `stopped` for its running `Bash` (a `local_bash`
  task `owned_by_subagent`). The parent's `Agent`
  call gets an error `tool_result` and its turn goes on to one `result`.
- `agents_background.txt`: one `Agent` call with `run_in_background: true`
  running `sleep 15 && echo bg-done > bg.txt`, and a prompt that ends the
  turn at once. The agent's lines, its `task_progress` and its
  `can_use_tool` all come after that turn's `result`; its prompt is not
  forwarded as a line (only `task_started.prompt` has it). When it ends
  (`background_tasks_changed` empty, `task_updated` and
  `task_notification` `completed`), Claude Code begins a turn by itself,
  whose `result` carries `origin.kind` `task-notification`.
