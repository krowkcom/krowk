# Hook payloads

What `gruntas hook <harness> <event>` reads on stdin, one file per event.

- `claude/`: recorded from Claude Code 2.1.330 (`claude -p` with a hook that saved its stdin), home paths replaced with `/home/user`. `WorktreeRemove.json` is the exception: written to the input Claude Code 2.1.330 documents for it (`worktree_path`, absolute) on the recorded `WorktreeCreate`'s common fields.
- `codex/`: written to the hook input schemas Codex 0.171.0 ships (`*.command.input`), since no Codex login was at hand to record them. KT08 checks them against a live session.

`usage.json` is the shape the Claude plugin's mod sends (`gruntas hook claude usage`).
