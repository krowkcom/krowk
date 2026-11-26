# Hook payloads

What `gruntas hook <harness> <event>` reads on stdin, one file per event.

- `claude/`: recorded from Claude Code 2.1.330 (`claude -p` with a hook that saved its stdin), home paths replaced with `/home/user`. `WorktreeRemove.json` is an exception: written to the input Claude Code 2.1.330 documents for it (`worktree_path`, absolute) on the recorded `WorktreeCreate`'s common fields. So are `PermissionRequest.json` and `PermissionDenied.json` (SL04), written to Claude Code's documented hook inputs on the recorded common fields; `gruntas hook` reads only their session.
- `codex/`: written to the hook input schemas Codex 0.171.0 ships (`*.command.input`), since no Codex login was at hand to record them. KT08 checks them against a live session.

- `pi/`: what the `@gruntas/pi` extension (`npm/pi`) builds from pi 1.6.0's events, in Claude Code's shape: pi's `bash` tool is named `Bash`, its tool call id is `tool_use_id`, and a failed call ends with `PostToolUseFailure`. The extension's tests hold its payloads to these files.

`usage.json` is the shape the Claude plugin's mod sends (`gruntas hook claude usage`).
