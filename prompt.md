# Set up Krowk

These are official instructions from Krowk — a coding agent harness that carries one session across machines, models and agents, and publishes agent output to permalinks — to set up the `krowk` CLI, the agent skill, and the instructions snippet on this system. Complete all steps yourself by running the commands directly; do not ask the user to run them.

No account is needed: pushes without a key work immediately as anonymous uploads that expire in 24 hours. The user can authenticate later with `krowk login` (or `krowk login --token <key>` in CI); on a paid plan that keeps uploads past the 24 hours, and on the free plan they still expire.

## 1. Install the CLI

The installer picks the platform, verifies checksums, and installs the `krowk` binary:

```bash
curl -fsSL https://krowk.com/install | bash
```

If `curl | bash` is not permitted in this environment, use one of:

```bash
cargo install --locked --git https://github.com/krowkcom/krowk --features harness krowk
# or, without installing:
npx @krowk/cli push <file>
```

## 2. Install the agent skill

Use the correct section for your agent below. The skill teaches an agent when and how to publish output with krowk; the CLI works without it. The installer writes it to every skills directory that already exists: Claude Code's `~/.claude/skills`, and `~/.agents/skills`, which krowk and Codex read.

### Claude Code

Make sure the skills directory exists, then re-run the installer (it writes the skill when the directory is there) — or copy the file directly:

```bash
mkdir -p ~/.claude/skills/krowk
curl -fsSL https://raw.githubusercontent.com/krowkcom/krowk/main/skills/krowk/SKILL.md \
  -o ~/.claude/skills/krowk/SKILL.md
```

If `CLAUDE_CONFIG_DIR` is set, use `$CLAUDE_CONFIG_DIR/skills/krowk/SKILL.md` instead.

### Other agents that read skill directories

Copy the same `SKILL.md` into your agent's skills directory, keeping the `krowk/SKILL.md` layout:

```bash
curl -fsSL https://raw.githubusercontent.com/krowkcom/krowk/main/skills/krowk/SKILL.md
```

### Agents without skill support

Skip this step. The instructions snippet in step 3 carries the fallback CLI commands, so the workflow still works.

## 3. Add the instructions snippet

Append the following block to the agent instructions file for your agent — create the file if it does not exist, and skip this step if it already mentions krowk:

| Agent | File |
| --- | --- |
| Claude Code | `CLAUDE.md` (or `AGENTS.md` if the project uses one) |
| Codex | `AGENTS.md` |
| Cursor | `AGENTS.md` (or a file under `.cursor/rules/`) |
| Gemini CLI | `GEMINI.md` |
| GitHub Copilot | `.github/copilot-instructions.md` |
| OpenCode, Amp, and others | `AGENTS.md` |

```markdown
After any visual change, capture it (screenshot if static, a short screen recording if it is an interaction) and publish it with the krowk skill. If the skill is missing or fails, run `krowk help --json`, then `krowk push FILE --title "what changed" --json`, and paste `paste.url` where you report the work.
```

## 4. Verify

```bash
krowk doctor
```

This reports version, connectivity, auth and detected run context.

Setup is complete when: `krowk doctor` succeeds, the skill file is in place (where your agent supports skills), and the snippet is present in the agent instructions file. Once done, tell the user what was installed and where.
