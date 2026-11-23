# Hook payloads

What `gruntas hook <harness> <event>` reads on stdin, one file per event.

- `claude/`: recorded from Claude Code 2.1.330 (`claude -p` with a hook that saved its stdin), home paths replaced with `/home/user`. `WorktreeRemove.json` is an exception: written to the input Claude Code 2.1.330 documents for it (`worktree_path`, absolute) on the recorded `WorktreeCreate`'s common fields. So are `PermissionRequest.json` and `PermissionDenied.json` (SL04), written to Claude Code's documented hook inputs on the recorded common fields; `gruntas hook` reads only their session.
- `codex/`: written to the hook input schemas Codex 0.171.0 ships (`*.command.input`), since no Codex login was at hand to record them. KT08 checks them against a live session.

`usage.json` is the shape the Claude plugin's mod sends (`gruntas hook claude usage`).
