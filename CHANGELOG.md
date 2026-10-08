# Changelog

What changed in each release of the krowk CLI, for the people upgrading rather
than for the people who wrote it. Newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the versions are the `v*` tags a release is cut from. Entries land under
`[Unreleased]` as the work merges, and move under a version when it is tagged.

## [Unreleased]

### Changed

- **The TUI is fullscreen, and the prompt stays at the bottom.** krowk now
  takes the terminal's alternate screen. The prompt and status line stay on
  the bottom rows while you scroll the conversation with the mouse wheel or
  PgUp/PgDn, so you can read back without losing the prompt. Sending a
  prompt jumps back to the bottom, and a resize rewraps the conversation.
  Dragging over the conversation selects it and copies it when you let go,
  a wrapped line as one line. When you leave, the conversation is printed
  into your normal scrollback as before. Inside Zellij krowk stays inline.
  To keep the old inline mode everywhere, which draws at the bottom of the
  normal screen and streams into scrollback, choose Screen: inline in
  `/settings`, or set `"tui": {"screen": "inline"}` in
  `~/.krowk/config.json` (`"fullscreen"` forces fullscreen, and `"auto"` is
  the default). The choice applies from the next session.
- **An upgrade reaches the host daemon by itself.** The daemon that runs
  your sessions used to stay on the old version until you ran `krowk host
  stop`. Now a newer `krowk` replaces an older daemon when it starts, or
  before your next prompt, once nothing runs there: no turn, no background
  agent, and no turn a backend started by itself.
  Other open `krowk`s reconnect to the new one. A newer daemon is never
  replaced by an older `krowk`, and a daemon run as a service (`krowk host
  enable`) is left for you to restart.

### Fixed

- **What you said is on a band across the width again**, instead of a
  background behind each row's text.
- **A prompt with images is no longer sent to an older host daemon that
  drops them.** A daemon from before 0.13.0 sent the model `[Image #N]`
  and no image. Now the prompt comes back to the editor, images and all,
  to send again once the daemon is replaced.
- **"copied … (N lines)" shows as soon as you copy.** After Ctrl-Y picked
  something to copy, the confirmation only appeared after the next key.

## [0.13.0] - 2026-10-07

### Added

- **A subagent can work in a git worktree of its own.** Give the
  `subagent` tool `isolation: "worktree"`, or put `isolation: worktree` in
  an agent definition's frontmatter as in Claude Code's agent files (the
  call's value wins), and the subagent runs in a new worktree of your
  repository, on a branch `krowk/<8 hex>`, under
  `~/.local/share/krowk/worktrees` (or `$XDG_DATA_HOME/krowk/worktrees`).
  Subagents started together no longer overwrite each other's files. The
  worktree starts from your files as they are, uncommitted changes
  included: modified, new and deleted files, and what you staged (ignored
  files only if you force-added them, nested repositories never), are put
  in one commit, `krowk: working state for <hex>`, on top of your `HEAD`, and
  the branch starts there; with nothing uncommitted it starts at `HEAD`.
  Your index, `HEAD` and files are not touched. When
  it finishes having changed nothing, the worktree and its branch are
  removed. When it changed something, its changes are applied to your
  working tree (see below). Outside a git repository the call fails
  with `isolation: worktree needs a git repository`. Without the field, or
  with `"none"`, a subagent runs in your directory as before. Your
  repository's git hooks do not run when the worktree is made.

- **A subagent's work in its worktree comes back to your working tree.**
  When a subagent with `isolation: "worktree"` finishes having changed
  something, krowk applies its commits and uncommitted changes, new,
  deleted and binary files included, to the working tree the parent
  agent works in, as uncommitted changes: your index and `HEAD` are not
  touched. The worktree and its branch are then removed, its final state
  kept as `refs/krowk/snapshots/<hex>` for 30 days, and the summary the
  parent gets ends with `Changes applied to your working tree: <files>`.
  Subagents finishing at once apply one after another. When a file
  doesn't apply cleanly (you, or a sibling, changed the same lines),
  nothing of it is applied, the worktree and branch are kept, and the
  summary ends with `Changes not applied (conflicts in <files>).
  Worktree: <path>, branch krowk/<hex>`. A subagent that changed a
  submodule is kept the same way, as krowk doesn't apply submodule
  changes. So is one that changed files inside a `.git`, `.claude`,
  `.codex` or `.krowk` directory, which the file tools only change with
  your say: its summary ends with `Changes not applied (they touch
  protected files: <files>)`, the worktree and branch, and the
  `krowk worktrees apply <hex>` you run to apply them.
  `krowk worktrees apply <hex|path> [--to <dir>]` does the same for any
  kept worktree, protected files included, into your repository's main
  checkout by default: it is how you bring a `--worktree` session's work
  home. It refuses (exit 4) while that session is still running, or when
  a file conflicts, and refuses (exit 1) a `--to` that is a checkout of
  another repository.

- **A subagent's worktree has your submodules checked out.** Before the
  subagent starts, krowk initialises every submodule of its worktree,
  submodules inside submodules too, at the commits the worktree records.
  One you have initialised in your main checkout is copied from there,
  with no download; its `origin` stays its own URL. One you haven't is
  cloned from its URL, with no terminal to prompt on and a 2-minute limit.
  A submodule that can't be initialised stays empty, and the subagent's
  first prompt says which and what git said. URLs from `.gitmodules` that
  would run a command (`ext::`) or copy a repository from a local path are
  refused. Your repository's config is not changed, so a submodule you
  deinitialised stays that way in your checkout. The agent can edit files
  in a submodule but not commit in it. An edit or a new file in any
  submodule, at any depth, keeps the worktree; an unchanged one is removed
  with its submodules' git data.

- **A subagent's worktree starts with your build output.** On a file
  system that can clone files without copying their blocks (btrfs, XFS
  with reflink, APFS on macOS), krowk clones your main checkout's `target`
  and `node_modules` into a new worktree before the subagent starts, so its
  first `cargo build` finds nothing to compile and the clone takes almost
  no disk. `target` is skipped while a build is running in it, and
  `node_modules` unless the worktree's lockfile (`package-lock.json`,
  `pnpm-lock.yaml`, `yarn.lock`, `bun.lock` or `bun.lockb`) is the same as
  your checkout's. Anywhere else, and on Windows, nothing is copied. Choose
  the directories with `worktrees.seed` in `config.json`, for example
  `{"worktrees": {"seed": ["target", ".venv"]}}`; `[]` turns it off.
  What was skipped and why is in `seed.log`, beside the repository's
  worktrees.

- **Worktrees appear almost instantly on btrfs.** When krowk's worktrees
  directory is on btrfs, on the same file system as your repository, and
  the `btrfs` program is installed, a new worktree for a subagent or a
  `krowk --worktree` session is a snapshot of a copy of your source tree
  that krowk keeps up to date, with your build output already in it:
  about a tenth of a second whatever the repository's size, and its first
  `cargo build` compiles only what differs from your last build. Twenty
  agents starting at once in one repository each get theirs in under half
  a second, rather than waiting on each other. Anywhere else, or if a
  snapshot fails, the worktree is checked out as before.

- **A subagent's worktree gets the ignored files you list in
  `.worktreeinclude`.** As in Claude Code, put a `.worktreeinclude` at the
  top of your main checkout, in `.gitignore` syntax (`.env*`,
  `config/local.yml`), and every file it matches that git ignores is
  copied into a new worktree before the subagent starts, with its mode.
  They are copies, never links, so the agent can't change your real
  `.env`; on a file system that clones files they cost no disk. Tracked
  files are never copied over the worktree's, nothing is read through a
  symlink or written over a file the worktree has, and the
  `worktrees.seed` directories are left to seeding. A file that can't be
  copied is skipped and noted in `seed.log`. Without a
  `.worktreeinclude`, nothing is copied.

- **A subagent's worktree can run your project's setup command first.**
  Set `worktrees.setup` to a shell command (`npm ci`, a code generator,
  a database script) and krowk runs it in each new worktree before the
  subagent starts, so the agent doesn't burn turns finding out the
  project isn't installed. Put it in `~/.krowk/config.json` for yourself,
  or in a repository's `.krowk/config.json`, where it runs only once you
  have trusted the repository (the same question that turns its hooks
  on); a trusted repository's command is the one that runs. It runs in
  the sandbox: network on, the worktree writable, your main checkout and
  your home not, so a cloned repository's install can't touch anything
  else. On a machine with no sandbox it runs only for a trusted
  repository. It gets `KROWK_PROJECT_ROOT` (your main checkout),
  `KROWK_WORKTREE_PATH` and `KROWK_PORT_BASE`: each live worktree holds
  its own ten ports from 20000 up (20000, 20010, …), and the subagent's
  own commands see `KROWK_PORT_BASE` too, so dev servers in two worktrees
  don't collide. A build or install command waits for a build slot as the
  agent's would. Its output is in `krowk-setup.log` in the worktree's git
  directory. When it fails, or runs past `worktrees.setupTimeout` (600
  seconds by default), the subagent starts anyway and its first prompt
  says so, with the exit code or "timed out" and the last 50 lines of
  output.

- **`krowk worktrees` lists, removes and prunes the worktrees krowk
  kept.** `krowk worktrees` shows every one, across repositories: its
  path, repository, branch, base commit, commits ahead of the base,
  whether it has uncommitted changes, the session that made it, its age,
  and whether that session is still running (`--json` for scripts).
  `krowk worktrees remove <hex|path>` removes one, and keeps its branch.
  It refuses while a running session uses it, and refuses one with
  uncommitted changes or commits ahead of its base unless you add
  `--force`. A forced removal first saves the uncommitted changes, new
  files included, as `refs/krowk/snapshots/<hex>`, and a `HEAD` its
  branch doesn't hold (the agent detached it, or switched branch, and
  committed) as `refs/krowk/snapshots/<hex>-head`, and prints the commands
  that bring the worktree back. Ignored files, such as build output and
  the copies `.worktreeinclude` made, don't count as changes and are
  deleted with it. `krowk worktrees prune` clears git's record of krowk's
  worktrees whose directory you deleted (only krowk's: your own
  worktrees' records are left alone, even when their drive isn't
  mounted), deletes worktree directories git no longer knows when their
  files match their branch (one with changes is left in place and
  named), and deletes snapshots older than 30 days. A repository that has
  moved, or whose drive isn't mounted, is skipped and named. Prune also
  runs by itself in the background, at most once a day, when a session
  starts. Worktrees made by an earlier krowk are
  listed too, with their base shown as unknown, so removing one of them
  needs `--force`.

- **`krowk --worktree` and `krowk -p --worktree` start a session in a git
  worktree of its own.** Sessions you start in separate terminals no
  longer share one checkout. krowk makes the worktree from the repository
  you are in, your uncommitted changes included, on a branch
  `krowk/<8 hex>`, readies it as it does a subagent's (submodules, build
  output, `.worktreeinclude`, `worktrees.setup`), and starts the session
  there; the TUI's header shows the branch, and the agent's commands get
  `KROWK_PORT_BASE`. When the session ends having changed nothing, the
  worktree and its branch are removed. When it changed something, both
  are kept and krowk says `Worktree kept: <path> (branch krowk/<hex>)` on
  stderr, with the `krowk worktrees apply <hex>` that brings its changes
  into your checkout; `-p --output-format json` or `stream-json` names it in the
  result instead, as `worktree: {path, branch, commits,
  uncommittedChanges}`. Resuming such a session runs it in its worktree
  again; if that directory is gone, krowk says so (exit 2) and points to
  `krowk worktrees`. Outside a git repository `--worktree` is refused
  (exit 1). It can't be combined with `--resume`, or with `-p --daemon`:
  the session runs in krowk's own process, which holds the worktree and
  finishes it on the way out, and the TUI does the same rather than using
  the host daemon. A worktree is trusted as the repository it was made
  from. `krowk sessions --worktree <path>` filters the listing as before.

- **An agent can commit inside a worktree krowk made for it.** When a
  sandboxed session runs in a worktree under
  `~/.local/share/krowk/worktrees` (or `$XDG_DATA_HOME/krowk/worktrees`) on
  a `krowk/…` branch, `git add` and `git commit` now work there, and the
  commit shows up in your main checkout under your own name and email:
  krowk reads `user.name` and `user.email` for that worktree and passes
  them in, without exposing your `.gitconfig`. The agent can move only
  its own `krowk/…` branches. Your other branches, tags, replace refs and
  `packed-refs` stay read-only, and so do the repository's config, hooks
  and `HEAD`, and your other worktrees. So `git tag` fails in there. A
  symlink the agent leaves among those branches, their reflogs or the
  worktree's git files is removed after the command, and the command
  fails and says so.
  Every other repository, your own linked worktrees included, keeps
  `.git` read-only as before.

- **Builds and test runs take turns across every krowk on the machine.**
  Agents spend most of their time waiting on the model, so one machine
  can run many of them, until several start `cargo test` or
  `npm install` at once and it runs out of memory. Now, when the agent's
  bash command runs a build or test program (`cargo`, `make`, `npm`,
  `pnpm`, `yarn`, `go`, `pytest`, `gradle`, `mvn`, `dotnet`, `swift` and
  the like, wherever the command line itself runs one, but not inside a
  script it calls or a `bash -c "…"`), it first waits for one of a fixed
  number of build slots that every krowk session and subagent on the
  machine shares, and holds it until the command ends. Other commands run
  straight away. While it waits, the command shows
  `waiting for a build slot (N in use)`, and its result says how long it
  waited when that was over a second. Interrupting the turn stops the
  wait. A krowk that is killed or crashes frees its slot at once. A build
  the command starts in the background (`cargo build &`) is not held to a
  slot once the command returns. There are a quarter as many slots as
  cores by default (at least one); set `"builds": {"slots": 2}` in
  `~/.krowk/config.json` to change that. A build also gets
  `CARGO_BUILD_JOBS` set to the cores divided by the slots, so even a
  build running alone gets only that share; set `CARGO_BUILD_JOBS`
  yourself, or raise `builds.slots`, to change it. Your own value is
  passed on in the sandbox too. This applies to krowk's own agent;
  Claude Code and Codex run their own shells.

- **Agents take turns across every krowk on the machine, and worktrees
  wait for disk.** `subagents.maxParallel` limits one turn, so ten
  sessions each starting four subagents started forty at once. Now every
  subagent, and every `--worktree` session, holds one of a fixed number of
  agent slots that every krowk on the machine shares, for as long as it
  runs. When all are held, a subagent shows
  `waiting for an agent slot (N in use)` and starts when one frees up;
  interrupting the turn stops the wait. A `--worktree` session prints the
  same line before it starts, and Ctrl-C stops it. A krowk that is killed
  or crashes frees its slot at once. Sessions without `--worktree` never
  wait, and a `--worktree` session's subagents run on its slot rather
  than taking more, so a session holding the last slot never waits on
  its own subagents. The default is twice your cores or twice your memory in GiB,
  whichever is fewer, between 4 and 64; set
  `"subagents": {"maxHost": 8}` in `~/.krowk/config.json` to change it (0 is
  refused). Before making a worktree, krowk also checks that its disk will
  have 4 GiB free beyond twice the size the worktree is expected to be
  (the last one krowk made of that repository, else your tracked files).
  When it would not, nothing is made, and the session or subagent fails
  with `not enough disk for a worktree: <free> free, <needed> needed`,
  pointing at `krowk worktrees prune` and `krowk worktrees remove`.
- **The README covers worktrees, and `scripts/bench-worktrees` measures
  them.** The README's new Worktrees section sums up `isolation`, applying
  back, `--worktree`, `.worktreeinclude`, `worktrees.setup`, `builds.slots`,
  `subagents.maxHost` and `krowk worktrees`. In a checkout of krowk,
  `scripts/bench-worktrees` makes 20 worktrees of a scratch clone at once,
  runs `cargo check` in all of them behind the build slots, applies three
  subagents' changes back (one a conflict), removes everything and prints
  creation p50/p95, check time, peak memory, slot wait and leftovers. It
  calls no model.
- **A host says why a viewer stays on the relay.** When a viewer offered
  the direct path never reaches it — a firewall on the host, or a tailnet
  access policy that doesn't allow the port, drops the connection before
  krowk sees it — `krowk sync host` now says so once per device, 30 seconds
  after the viewer joined:
  `krowk: session …: device 0123abcd stays on the relay — nothing it sent
  reached this machine's direct addresses (100.64.0.1:51915, …); if it is on
  this tailnet, a firewall here or the tailnet's access policy may be
  refusing the port`. A viewer that reached the direct path once and fell
  back is never named. The session goes on by the relay, as before.
- **A bare krowk link no longer lands in a pull request or an issue.** On
  GitHub a bare `krowk.com/a/…` link doesn't unfurl. It shows up as a blue
  link that says nothing about the file. When an agent running in krowk
  calls `gh pr create|edit|comment` or `gh issue create|edit|comment` with
  one in the body, krowk refuses that one call and tells the agent to paste
  the krowk block instead, and where to get it (`paste.markdown` from
  `krowk uploads show <link> --json`). The agent rewrites the body and posts
  again without you stepping in. The body is checked whether it is given
  inline, through a heredoc, or with `--body-file` when the file can be
  read. A card link inside a krowk block is fine, and nothing else is
  touched: other `gh` commands, bodies without a card link, and posts to
  Slack or Basecamp, where a bare link unfurls. This isn't a permission
  rule, so it holds in every mode, `bypassPermissions` and `unhinged`
  included, on krowk's own models, Claude Code and Codex alike. On Codex,
  krowk passes the check as a hook, and the first time, has Codex record it
  as trusted in its `config.toml` (a `hooks.state` entry for that one
  command), because Codex runs only trusted hooks. krowk's Codex accounts
  share your `~/.codex/config.toml`, so that is where the entry lands. On
  Windows, Codex sessions don't have the check yet. A code span or fenced
  block that quotes a card link is left alone.
- **The agent can ask you which way to go.** When a choice is yours, the
  agent asks up to four questions with options to pick from, and you answer
  over the prompt: a number or Enter picks, Space toggles where several may
  be picked, Tab moves between questions, and typing gives your own answer
  instead. Esc declines and the agent carries on with its best judgment.
  The native agent gets an `ask_user` tool for this. Claude Code's
  `AskUserQuestion` and Codex's questions, which krowk used to refuse, now
  reach you the same way, in plan mode too. Questions are only asked where
  someone can answer: `krowk -p` doesn't offer the tool, and a backend that
  asks anyway is told to decide and say what it assumed. For protocol
  clients, an `approval.requested` frame can carry `questions`, and
  `approve` answers them with `answers`.
- **Paste a screenshot into the prompt.** Ctrl-V (or Alt-V, where the
  terminal keeps Ctrl-V for itself) puts the clipboard's image in the prompt
  as `[Image #1]`, and the model sees it with the text. Dragging an image
  file onto the terminal, or pasting its path, does the same, several at
  once included. A file is read the moment it's dropped, so a macOS
  screenshot dragged from its thumbnail still works. `[Image #N]` is one
  unit to the caret: Backspace takes it whole. Typing goes on while a large
  screenshot is read, and the image lands where you pasted it. PNG, JPEG,
  GIF and WebP are taken, scaled down to 2,000 pixels a side and under the
  providers' size limits as they're pasted, and kept beside the session's
  log in `~/.krowk/sessions/<id>/images/`. Every provider and backend gets them
  (Anthropic, OpenAI, xAI and other Chat Completions servers, Claude Code
  and Codex), and a model the catalog says can't read images is refused
  with the prompt handed back. The clipboard is read with `wl-paste` or
  `xclip` on Linux (krowk names the one to install if neither is there),
  `osascript` on macOS, and PowerShell on Windows and under WSL. Ctrl-V with
  only text on the clipboard pastes the text.
- **On Linux, `krowk -p` now makes edits without asking, inside a sandbox.**
  A `-p` run with no `--permission-mode` and no `defaultMode` used to refuse
  every edit; where bubblewrap works it now runs in `acceptEdits`, with its
  commands inside the `workspace` sandbox below, and inside a container in
  `acceptEdits` with the file tools held to the same fences. Pass
  `--permission-mode default` to keep the old behaviour. `--sandbox
  workspace|read-only|strict|off` puts the `bash` tool inside bubblewrap.
  The sandbox allows rather than lists what it hides: your home is replaced
  by an empty one, and only the workspace and the Rust toolchain's homes
  come back, so nothing else in it — `~/.ssh`, `.git-credentials`, `.netrc`,
  agents' logins, `~/Documents` — is in reach, and programs installed under
  the home (mise's shims, `~/.local/bin`) are not on the sandbox's `PATH`. `workspace` writes
  the working directory and its added directories and keeps the network;
  `read-only` writes nothing and has no network, not even DNS; `strict`
  writes the workspace and has no network. Every `.git` in
  the workspace, nested repositories and gitdir files included, and every
  hooks directory a repository's `core.hooksPath` names there — found by
  searching the workspace once a turn and re-checking only what changed —
  stays
  read-only, as do `.claude`, `.codex` and `.krowk`; a `.git` a command
  creates is removed after the call. The file tools hold the same lines
  under every permission mode, and open exactly the path they checked, so a
  symlink swapped in meanwhile fails the call. A sandboxed command gets
  only `PATH`, `TERM`, the locale, `USER`, a private `HOME` and `TMPDIR` —
  no provider key, token or agent socket — no inherited file descriptor, and
  a session of its own. `RUSTUP_HOME` and `CARGO_HOME` are bound read-only
  with cargo's `credentials.toml` hidden, so `cargo build` of what is
  already fetched works and fetching a new dependency does not. `PreToolUse`
  hooks are your own and run outside the sandbox. A sandbox that cannot be
  enforced refuses the run with a fix instead of running unsandboxed:
  bubblewrap missing or blocked, a workspace too large to search for
  repositories, a Claude Code or Codex backend, `--daemon`, macOS (Seatbelt
  is not built yet) and Windows.

- **krowk's own agent uses your MCP servers.** It reads the `mcpServers`
  you set up for Claude Code (`~/.claude.json`, its `settings.json`), those
  in `~/.krowk/config.json`, and a repository's `.mcp.json` once you trust
  the repository, since a `.mcp.json` names programs to run. Stdio and
  streamable-HTTP servers work. The model gets two tools, `mcp_search` and
  `mcp_call`, instead of every server's tools, so fifty MCP tools cost each
  turn about 140 tokens. No server starts until the model searches.
  `Mcp(server:tool)` permission rules allow, ask about or deny each call,
  and search leaves out tools a deny rule covers. A repository's servers
  are asked about again when its `.mcp.json` changes after you trusted it,
  and no server inherits your provider keys unless its config sets them.

- **Idle sessions move off the machine as weekly vintages.** `krowk sessions
  archive` takes every native session idle for more than 14 days
  (`--older-than DAYS`, or `KROWK_ARCHIVE_AFTER_DAYS`) and stores each ISO
  week's sessions in the registry as one vintage: zstd-compressed JSONL,
  sealed under the account key, so the registry only ever holds ciphertext.
  An existing vintage for the week is merged, never overwritten. `--weekly`
  runs only when a week has passed since the last run, so it can be put on
  a schedule. An archived session keeps its title, summary, directory,
  dates, models and cost on the machine, so `krowk sessions` still lists
  it, and `krowk sessions show` or `krowk -p --resume` fetches its week and
  restores it into krowk.db first. `krowk sessions restore <id>` does that
  on its own. `krowk sessions pin <id>` keeps a session from ever being
  archived, and `unpin` undoes it. It needs `krowk sync` set up on the
  machine.

- **The Claude Code plugin brings the MCP server.** Installing `krowk@krowk`
  in Claude Code now adds the krowk MCP server (`npx -y @krowk/mcp`) beside
  the skill, so publishing works without a separate `claude mcp add`; if you
  added `krowk-mcp` that way, run `claude mcp remove krowk` in the project
  where you added it, or the model sees every krowk tool twice there. When
  krowk drives Claude Code as a backend the plugin's server is not started,
  and krowk's own MCP server is the only one. The plugin's version now
  moves with every release: 0.12.0 and 0.12.1 shipped without moving it, so
  Claude Code offered no update.

### Changed

- **krowk's own git calls now run with repository hooks, fsmonitor and
  commit signing turned off.** The git krowk runs itself — the branch on
  the status line, the commit a synced session is at, the files a search
  lists — looks for hooks in an empty directory in krowk's home
  (`~/.krowk/no-hooks`), runs no `core.fsmonitor` command and signs
  nothing. Filters such as git-lfs still run, as a checkout needs them.
  Git the agent runs through its bash tool is unchanged.
- **Uploads are now `krowk artifacts`, matching the API and the JSON.**
  `krowk artifacts create | list | show | attach | delete | claim` is the
  full set, named as `/v1/artifacts` and `data.artifacts` already were.
  `krowk push` is the short form of `artifacts create`, and `krowk claim`
  of `artifacts claim`; both stay and work as before. `krowk uploads …`
  still runs every command it did, and `krowk help uploads` shows the
  artifacts help, but help no longer lists it. Breadcrumbs and error fixes
  now name `krowk artifacts …`, so an agent that copies them gets the new
  spelling.
- **krowk's messages read like sentences, and failures say what to try.**
  An error leads with what went wrong in bold, puts the command that fixes
  it on a `Try:` line of its own, and ends with its code. It no longer drops
  the alternative a fix offers ("…, or upload anonymously"). A mistyped
  command, subcommand, flag or help topic gets a "did you mean". Flag errors
  are plain English (`unknown flag --nope`, `--limit takes a whole number`)
  rather than Go's `flag provided but not defined: -nope`, and point at the
  command's own help. Bare `krowk runs` names the subcommands it takes
  instead of calling `runs` unknown. Successes, warnings (a yellow `!`),
  labels and empty lists are capitalised, and `krowk doctor` shows a
  checklist instead of raw JSON. Colour follows `NO_COLOR` and `TERM=dumb`,
  and an error is coloured only when stderr is a terminal. The JSON
  envelopes are unchanged except for the `fix` wording.
- **`krowk --help` is the readable page even when piped.** An agent running
  `krowk --help` used to get the whole command catalog as about 50 KB of JSON.
  It now gets the overview, which has an Examples section and points to the
  new `krowk help agents` page. That page explains the JSON envelope,
  breadcrumbs, exit codes and which command answers which task.
  `krowk help --json`, or `--jq`, still prints the catalog. Help headings
  are in title case and bold on a terminal.
- **`krowk hosts` lists your machines that host, with no Tailscale tag.**
  It used to list only tailnet machines tagged `tag:krowk-host`, which took
  an edit to the tailnet policy in the Tailscale admin console. Now it lists
  every machine on your device list that hosts a synced session — from the
  registry's sessions, so it needs this machine's sync keys — with whether
  it hosts one now and whether this machine's Tailscale reaches it
  directly. A host names its own tailnet node, sealed, in each session it
  hosts, so a session hosted by an older krowk shows its host as reachable
  only once a newer one hosts it again. Machines that aren't yours, and
  yours that host nothing, aren't listed. `tag:krowk-host` is no longer
  read. In `krowk hosts --json`, `name` is now the machine's name on your
  device list rather than Tailscale's (`dnsName` is still Tailscale's), and
  rows also carry `device`, `os`, `thisMachine`, `hosting`, `direct` and
  `sessions`; `online` still means online on the tailnet.
- **The same-user check refuses to run on a tagged machine.** Tailscale
  names every tagged machine of a tailnet as one shared user,
  `tagged-devices`, so on a tagged host `KROWK_TAILSCALE_SAME_USER=1` would
  have let in any tagged machine and turned away your own. It now says so
  and offers no direct path; untag the machine, or leave the check off.
- **A synced session goes direct over Tailscale with no setup.** With
  Tailscale running on both machines, `krowk sync host` offers the direct
  path by itself: it fetches the registry's relay ticket public keys
  (`GET /v1/relay/ticket_keys`) at every hosting, so a viewer's session
  moves off the relay with no `KROWK_RELAY_TICKET_KEYS` file to write, and
  a key the registry rotates in reaches the next session without a
  reinstall. It keeps them in `~/.krowk/cache/` for a day, used only when
  the registry can't be reached. Admission is unchanged: only the
  registry's ticket and the device's signed challenge get in, and being on
  the tailnet admits nobody. When there is no direct path, the host says
  why in one line on stderr, such as `krowk: no direct path (Tailscale
  isn't running: it is Stopped); the session goes by the relay`, and the
  session goes by the relay. `KROWK_RELAY_TICKET_KEYS` still names the keys
  by hand, and wins over the fetched ones, for a stand-in registry.
- **The Gemini extension, the MCP registry entry and `@krowk/mcp` describe
  Krowk as a coding agent harness.** They used to describe a permalink
  uploader. Each now says that it's Krowk's publishing, for Gemini CLI or
  for any MCP client, and what it turns into links. With
  the Claude Code plugin's, they no longer say links unfurl in GitHub and
  Linear, which don't unfurl links. Those links unfurl in Slack and Basecamp;
  in GitHub and Linear, images show inline. The `server.json` here was over the
  MCP registry's 100-character description limit and didn't validate against
  its schema; it's within it now. `make release-check` and the release
  workflow now hold it to that limit, and the other descriptions to one line.
- **What you say in a session is shown in markdown, as answers are.**
  `` `code` `` is in the code colour, `**bold**` is bold, and links, lists,
  quotes and fenced blocks look as they do in an answer, all on the band of
  your prompt. Ctrl-Y still copies the prompt as you typed it.
- **Reading your synced sessions is signed by this machine's device key.**
  `krowk sync sessions`, attaching, `--resume` and the host now sign the
  calls that list a session, show it and list its chunks, as they already
  signed every write. Once the registry requires it, a copy of your API key
  without the device's key reads none of your sessions, not even their
  sizes and times. Two reads signed in the same millisecond on one machine
  are told apart by signing the second again.
- **The new-release notice speaks up only when the release is worth it.**
  A major release (before 1.0, a new middle number, like 0.12 → 0.13) is
  mentioned once and once more a week later, a minor release once, and a
  patch release never — unless a release you skipped has a security fix,
  which is mentioned once a day until you upgrade. In the agent it's one dim
  `Update:` row under the session header, never a banner, and Ctrl-O's
  session details always say whether a newer release is out. The agent
  never waits on the check: it uses the last answer and asks for the next
  one in the background. After other commands the line on stderr follows
  the same rules. `KROWK_NO_UPDATE_CHECK=1` still turns it all off.

### Fixed

- **A Mac with the Tailscale app can host over the direct path.** The app
  has no `tailscaled` socket: its LocalAPI is on 127.0.0.1 behind a token.
  On macOS, `krowk sync host` and `krowk hosts` now try the open-source
  `tailscaled`'s sockets, then the App Store app's `sameuserproof` file in
  its group container, then the standalone app's in `/Library/Tailscale`.
  `KROWK_TAILSCALE_LOCALAPI` names the app's LocalAPI by hand
  (`http://127.0.0.1:<port>` with `KROWK_TAILSCALE_LOCALAPI_TOKEN`, or the
  `sameuserproof` file's path); only loopback is taken.
  `KROWK_TAILSCALE_SOCKET` still names a socket, and wins over it.
- **A synced host no longer hangs on a relay link that stopped working
  without saying so.** If the relay kept answering heartbeats but acked none
  of what the host sent, viewers stopped receiving updates while the host
  kept writing into the void. The host now drops such a link once eight
  batches have gone unacked for ten seconds, joins the relay again, and
  resends what was missed.
- **`krowk devices add` no longer says "nothing was added" when the new
  device may have your key.** If the answer to the step that sends the key
  was lost and checking on it failed too, the pairing used to end as if
  nothing had happened, although the new device might hold your key
  without being on your list. Now the laptop goes on: it waits for the new
  device to confirm and adds it, or, if it never confirms, still lists it so
  `krowk devices remove` can take it off.
- **`krowk sync host` notices a device removed while it runs.** It used to
  check your device list only when it started, so a long-running host went
  on as before after you removed a device. It now reads the list again every
  minute. If another device was removed, it hosts the session again from
  the top, under the new list; if this device was removed, or the list was
  started over, it stops.
- **The installer says when another krowk would run instead.** If a krowk
  earlier on your `PATH` (an older install, or a build from source) would
  answer before the one just installed, the installer now names it, says
  its version, and how to fix it. Before, `krowk --version` quietly showed the
  old one.
- **Pairing refuses a malformed key exchange outright.** If the other side
  of `krowk devices add` / `krowk sync join` sends a degenerate SPAKE2 point
  (one of small order, or one spelled non-canonically), the pairing ends at
  once instead of carrying on. An honest device never sends one; this only
  closes the door on a misbehaving peer.
- **Running `krowk sync init --start-over` again says what it did.** While
  the old keys from a start-over are kept, running it again carries on that
  start-over (sealing any sessions left under the new list) and never begins
  a new one. It now says so, and that a new start-over needs
  `krowk sync recovery discard-old` first.
- **A new device can't take a name that only looks like one already on your
  list.** Adding or pairing a device is refused when its name reads like
  another device's: a Cyrillic `а` for a Latin `a`, full-width letters, or
  `1` or `I` for `l`. Each name in a removal or recovery review now names
  exactly one device.
- **A synced session moves to a new key when you host it again after
  removing a device.** A session published before you removed a device used
  to go on under the session key that device held, however often it was
  hosted again. Now the host first moves it to a new key, sealed under your
  current user key, which the removed device never had; a start-over does
  the same for the sessions it brings along. Everything written before
  still reads on all your devices. A host already running when you remove
  the device keeps its key until the session is next hosted. A session that
  was never moved is stored exactly as before and still opens on older
  krowk versions; one that was needs this version, and a viewer that
  opened it before the move has to open it again.

- **Copying from the TUI no longer drags the layout along.** Text in the
  transcript starts at the first column: a mouse selection of your prompt,
  an answer, a list or a code block comes without the two-column margin,
  the `▎` bar before your prompts, the padding inside code blocks or the
  spaces that filled a band to the edge. Code blocks are left for the
  terminal to wrap, so a long line of code copies as one line, the block's
  language sits on the row above the code, and tabs in code are four
  spaces rather than one. Prose still wraps between words, so a selection
  across a wrapped paragraph has a line break where each row ended:
  Ctrl-Y now offers the last answer, each code block in it and your last
  prompt, and copies the one you choose exactly as written, tabs included.
  In Ghostty, and in herdr, which is built on it, text from the first
  screen of a session used to copy with a break at every row the terminal
  had wrapped, because the prompt was kept at the bottom by moving the
  rows above it; the prompt now sits at the bottom from the start, the
  conversation filling the screen down to it, and nothing printed is ever
  moved.

- **Two quick Ctrl-Cs right after a prompt still print the resume line.**
  Leaving that fast could beat the session's start to the TUI, so krowk
  exited 130 without `krowk --resume <id>` although the host had already
  started the session. The TUI now reads what the host already sent before it leaves.

- **The prompt stays on the bottom row when the window grows taller**, in
  Ghostty and other terminals that add the new rows at the bottom.
- **What you said survives a narrower window.** Each row is banded only as
  wide as its text, so the band no longer wraps onto rows of its own; a row
  wider than the new window still wraps, as any text does.

## [0.12.1] - 2026-10-04

### Changed

- **krowk says what it is: a coding agent harness.** `krowk help`, the
  installer and the package descriptions lead with one session on any
  machine, model or agent; publishing to a permalink is one of the things it
  does. `sync` and `devices` move up to the `AGENT` heading in `krowk help`.
  The lean build, which only publishes, still calls itself that.
- **The installer writes the agent skill for every agent that reads one.**
  Besides Claude Code's `~/.claude/skills`, it writes `~/.agents/skills`,
  which krowk and Codex read, when that directory exists, and names every
  place it wrote on one line. Its next steps lead with `krowk`, `krowk login`
  and `krowk sync init`, then `krowk push`.

## [0.12.0] - 2026-10-04

### Added

- **`krowk sync attach` opens the TUI.** On a terminal, a session another
  machine runs is drawn as your own are: its history, then live. Prompts go
  from the prompt box, approvals are answered in the usual dialog, and Esc
  interrupts. The status line says whether the host is there (prompts wait
  for it when it is not) and whether the session comes by the relay or
  directly. `krowk --resume <id>` of a synced session opens it the same way.
  The host runs prompts from a viewer in the session's default mode, so
  `/mode`, `/model`, `/new` and `/sessions` say they are the host's, and a
  viewer allows a call once or for the session, never for the host's
  project. With `--json`, or stdout not a terminal, it prints stream-json as
  before.
- **`krowk sync init` sets up your device list, with a 12-word recovery
  kit.** It asks you to sign in again in the browser, then makes the list's
  first entry: this device and the recovery device the kit derives, with your
  user key wrapped to both. The kit's words are shown once on stderr ([Enter]
  when written down, [s] to skip) or written to `--save FILE`, 0600, and are
  never typed back. `--start-over` makes a new list; every other device on
  the old one stops syncing and asks to be paired again. Run from a device
  that holds your key, it keeps the old keys aside and seals the sessions it
  can open again under the new list; running it again (under another
  workspace's key, for that workspace's) goes through what is left, and
  `krowk sync recovery discard-old` drops the old keys when you say so. The 24-word recovery phrase is gone, and so is
  `krowk sync register`: a device is registered by being on the list.
- **`krowk devices remove NAME`** takes a device off your list and rotates
  your key away from it. It names every device the new key goes to before it
  asks, needs a fresh sign-in, and removes any device revoked on the dashboard
  in the same post, so no new key reaches one. `krowk sync status` offers to
  finish a dashboard Revoke. Your other devices take the new key at their next
  sync.
- **`krowk sync recover`** gets back in on a new machine from the kit's
  words, typed at a prompt that doesn't echo them or piped in. It verifies
  the list from its first entry, goes through every device on it with you to
  keep or remove, and only then wraps your key, to what you kept.
- **`krowk sync recovery new`** replaces the kit at once, with the old kit's
  words, or from any device when you have none. **`krowk sync recovery
  check`** tests the words against the list, locally. **`krowk sync status`**
  verifies the list against the head this device pinned, and says when there
  is no kit; so does the TUI's status line.

- **Pairing a device by a short code, in the library.** `krowk_client::pairing`
  holds both sides of `krowk devices add` as sans-IO state machines: an
  eight-character code (Crockford base32 less `0` and `1`, shown `XXXX-XXXX`,
  typed in any case with spaces and dashes ignored), SPAKE2 in asymmetric mode
  bound to the peer kind, the person and both device ids, and key confirmation
  both ways before the new device's name is shown or anything is posted. One
  failed step ends the pairing, and the new device's code is consumed by the
  attempt, so a hostile registry gets one guess against each side. The SPAKE2 crate is held to magic-wormhole's vectors. Nothing
  calls it yet.
- **The client crypto for devices you own**, not yet wired to any command:
  a user key per person with generations, each wrapping the one before; a
  12-word recovery kit that derives a recovery device; and a signed,
  chained device list that every client verifies against the head it last
  saw, refusing an older or forked list and any removal of the recovery
  device. Only the current kit can replace the kit, or any device when there
  is none.
- **`krowk devices add` and `krowk sync join` pair a machine by a short
  code.** `add` shows `XXXX-XXXX`, valid ten minutes and once; `join` on the
  new machine takes it at a prompt, never as an argument. Both machines check
  the code, the paired one asks `Add '<name>' (<os>) to your devices? [Y/n]`,
  and the new machine keeps the user key only once the chain it was sent
  adds exactly its keys. A wrong code, or any failure on the new machine, a
  dropped connection included, ends the pairing and asks for a new code. A
  machine that was sent the key but never confirmed it is still listed, so
  `krowk devices remove` can take it off; ^C on `add` ends the pairing.
- **`krowk sync host` seals under the device list as the registry has it
  now,** extended from this machine's pin, and refuses when the registry
  cannot be asked, rather than sealing under a list a removal left behind.
- **The stand-in registry holds devices you own.** `krowk-devregistry` serves
  a person's signed device list (verified on every post by krowk-client's own
  verifier), the user key wrapped to each device, keys bound to one device,
  a fresh sign-in stamp, and the pairing mailbox: one live pairing per
  person, ten minutes, ended by the first step out of turn. The device
  approval endpoints answer `410 sync_reset`.
- **Releases are signed, come with an SBOM, and are proven reproducible.**
  Each release carries `checksums.txt.sigstore.json`, a keyless Sigstore
  signature over the checksums of every archive, bound to the release
  workflow at that tag, and a CycloneDX SBOM of the full and lean builds,
  each signed the same way. Before anything is published, the release builds
  Linux x86-64 a second time on another runner from another path and stops
  unless every binary matches byte for byte. The npm packages carry npm
  provenance. `SECURITY.md` says how to verify a download and rebuild one.

- **`SECURITY.md`**: where to report a vulnerability (security@krowk.com),
  what happens then, what is in scope, and every network call krowk makes
  without being asked. krowk has no telemetry and no crash reporting.

- **`cargo deny` on every pull request and at every tag.** A dependency
  with a RustSec advisory, a licence outside `deny.toml`'s list, or a
  source other than crates.io fails CI and stops a release.

- **`krowk sync attach` answers approvals and steers the turn.** Besides
  prompts, a line typed on its stdin can be `/approve REQUEST_ID`,
  `/allow-session REQUEST_ID` or `/deny REQUEST_ID`, answering the tool call
  an `approval.requested` line names, `/interrupt`, which stops the running
  turn, or `/steer TEXT`, which adds to it. Each `approval.requested` line
  carries its `requestId`, and stderr says which command answers it. Any
  other line starting with `/` goes as a prompt, so skills and paths still
  work, and `//TEXT` sends `/TEXT`. `krowk help sync attach` lists them.

- **A synced session goes direct over Tailscale when it can.** `krowk sync
  host` reads this machine's tailnet address, MagicDNS name and LAN address
  from the local `tailscaled`, listens there, and names the addresses to
  each viewer inside the sealed channel. A viewer tries them all beside the
  relay and moves onto the first that passes the same ticket, challenge and
  handshake the relay does, mid-session and without losing a frame; if the
  direct path goes, or its welcome is more than three seconds late, it
  falls back to the relay by itself and tries again later, less often each
  time nothing is found (10 seconds, doubling to 5 minutes). `krowk sync attach` prints the path in use (`sync.path`: `relay`,
  `direct over Tailscale` or `direct over LAN`). Being on the tailnet lets
  nobody in: direct paths are on only with the registry's ticket keys in
  `KROWK_RELAY_TICKET_KEYS`, and `KROWK_TAILSCALE_SAME_USER=1` also turns
  away a connection from another tailnet user. The LAN address is offered
  only with `KROWK_DIRECT_LAN=1`, and never with the same-user check.
  `krowk hosts` lists the
  tailnet's machines tagged `tag:krowk-host`.
- **A file a session publishes now says what made it.** Besides the session
  (`krowk.session`), each artifact `publish` pushes records the engine that
  ran the turn (`krowk.engine`: `krowk`, `claude-code` or
  `codex-app-server`), the model (`gen_ai.request.model`) and its provider
  (`gen_ai.system`), so its card on krowk.com can link back to the session
  with a `krowk --resume` command. A subagent's file names the subagent's
  model. Nothing from the session's log is sent; the session id, engine,
  model and provider are, in the clear, even for a synced session.
- **A session running on one machine can be watched and steered from
  another.** The host keeps the session's lease, renewing it every 20
  seconds, streams it over the relay sealed end to end, and writes its log
  to the registry as sealed chunks with a checkpoint to attach from.
  Another of your machines attaches from the latest checkpoint in well
  under half a second, follows the session live, and can prompt it, steer
  or stop it, and answer its approvals. When the host is away the other
  machine is read-only, and a prompt typed there waits and runs when the
  host is back. A dropped connection resumes by itself with nothing lost.
  Neither the relay nor the registry ever sees the session's content.
  `krowk sync host <session>` syncs a session of this machine, `krowk sync
  sessions` lists what another machine can open, and `krowk sync attach
  <session>` — or `krowk --resume <session>` on a machine without the
  session's log — follows it there. The relay is `KROWK_RELAY_URL`, or
  `krowk relay serve` on this machine.

- **Sync calls that act as this machine are signed by its own key.**
  Registering or approving a device, a lease call, writing a session or
  its log, and asking for a relay ticket now carry `X-Krowk-Device`,
  `X-Krowk-Timestamp` and `X-Krowk-Signature`, an Ed25519 signature by
  this machine's signing key. The registry refuses them unsigned, signed by
  another key, more than five minutes off its clock, or sent twice, so a
  workspace API key alone can no longer act as one of its devices. A
  clock more than five minutes out makes these calls fail with
  `signature_stale`.
- **A file a session publishes now says what made it.** Besides the session
  (`krowk.session`), each artifact `publish` pushes records the engine that
  ran the turn (`krowk.engine`: `krowk`, `claude-code` or
  `codex-app-server`), the model (`gen_ai.request.model`) and its provider
  (`gen_ai.system`), so its card on krowk.com can link back to the session
  with a `krowk --resume` command. A subagent's file names the subagent's
  model. Nothing from the session's log is sent; the session id, engine,
  model and provider are, in the clear, even for a synced session.
- **Setting up sync now registers this machine's relay signing key.**
  `krowk sync init`, `recover` and `register`, and `krowk devices approve`,
  send the public half of the signing key beside the device key, and `krowk
  sync join` sends it with the approval request, so the approval registers
  it. krowk's hosted relay uses it to tell this machine's connections from
  anyone else's. The key is set once: a different one for the same device
  is refused (`signing_key_mismatch`).
- **The code `krowk sync join` shows to approve a machine changed.** It
  now covers both of the new machine's keys, so a request that copied its
  device key with someone else's signing key shows a different code, and
  `krowk devices approve` approves only the one request whose code is
  exactly the one you typed. Only one approval request per machine may
  wait at a time.
- **Relays admit devices on tickets the registry signs.** A device joins
  a relay with a short-lived ticket, which comes with the session's lease
  for the host and on request for a viewer. `krowk relay serve` now takes
  `--ticket-keys FILE`, the registry's ticket-signing public keys, instead
  of `--roster`. The ticket travels in the `X-Krowk-Ticket` header of the
  WebSocket upgrade, and a connection without a good one is turned away
  before it takes any room on the channel, so a flood of connections
  cannot keep a session's devices off it.
- **`krowk relay serve --state DIR` keeps each channel's fence across
  restarts.** It is required when `--addr` is reachable from the network,
  like `--origin`, so a restart never lets a machine that lost the lease
  host again. On loopback the relay may keep it in memory.
- **A relay keeps development sessions apart from production ones.**
  A relay join now says which it is, `env` "production" or "development",
  and `krowk relay serve` gives each its own channel of a session, so a
  developer's session never shares a relay buffer or a viewer with a
  user's. krowk says "development" for a debug build, for
  `KROWK_ENV=development`, or for any registry but `api.krowk.com`.
- **`krowk relay serve` runs a relay of your own.** It carries a synced
  session between the machine running it and the devices watching it, and
  only ever sealed bytes: it refuses anything not encrypted, and never
  stores it. A device joins by signing the relay's challenge with a new
  per-device signing key (`signing.json` in krowk's home, made the first
  time it is needed); only the machine holding the session's lease may
  host it. A device that reconnects picks up where it left off from the
  relay's short buffer, and the relay answers heartbeats itself, so a busy
  or quiet host never looks gone. It listens on 127.0.0.1:7790 unless
  `--addr` says otherwise; an address reachable from the network also
  needs `--origin`, the URL devices dial it by, and the banner says it is
  open. Nothing connects to it on its own yet; the host daemon and the
  terminal will once syncing sessions lands. `krowk help --all` lists it.
- **A synced session's log can now be stored, encrypted, in the registry.**
  The machine holding a session's lease seals each piece of the log on its
  own side and uploads it straight to storage. Each piece is numbered,
  names the lease it was written under, and is chained to the piece before
  it, and the last one is marked. A device reading the log back refuses a
  piece that is repeated, out of order, missing, or spliced in from another
  machine's copy of the log, and can tell a finished log. A piece is
  refused unless it comes with the lease's current token, and one a
  previous lease holder left half-uploaded is replaced rather than blocking
  the log. Pieces are at most 64 MiB. The registry and storage only ever hold the sealed bytes.
  The pieces count on your workspace's storage meter, and never show up in
  `krowk uploads list`, on a card, or anywhere else a push's artifacts do.
  Nothing writes them yet on its own; the host daemon will once syncing
  sessions lands.
- **A device the workspace's owner revoked by resetting sync can come back.**
  Run `krowk sync register` on it after the reset. The fix line on a
  `device_revoked` refusal now says so, where it used to point at
  `krowk sync recover`, which could not help.

- **Add a machine to sync by approving it from one that already syncs,
  with no recovery phrase.** On the new machine, `krowk sync join` shows a
  32-character code and waits. On a machine that already syncs, `krowk
  devices approve` asks for that code, shows which machine it belongs to,
  and hands the account key over, encrypted to the new machine's key, once
  you say yes. The new machine then asks for the account key id `approve`
  printed (or takes it as `krowk sync join <id>`), asks you to confirm, and
  keeps nothing unless the ids match. Read that id off your other machine,
  never from an error message or a web page. Both commands need a person at
  a terminal: an agent told to run them is refused. `krowk devices list`
  shows the workspace's machines and the account key id this one holds.
  All of it needs a key to a Pro workspace; on a free one the commands
  refuse with a fix line. `krowk sync init` and `recover` still work with
  no account, on a free plan, or with the registry unreachable: the keys
  stay on this machine, and `krowk sync register` registers it later. With
  a Pro key they register this machine themselves, and `recover` then
  refuses a phrase that restored a different account key from the
  workspace's, which is how a mistyped word that happens to pass the
  checksum is caught. `--name` (or `KROWK_DEVICE_NAME`) sets what the device
  list calls this machine instead of its host name.
- **`krowk sync init` sets up the end-to-end encryption keys sync will
  use, and shows your recovery phrase.** It makes a key for this machine
  and an account key, prints the account key as 24 words, and keeps it
  only once you type the words back (they are not echoed). Write them
  down, with the key id shown beside them: krowk never stores the phrase
  and cannot show it again, and it is the only way back to your sessions
  if every device is lost. On another machine, `krowk sync recover` takes
  the 24 words — typed at its prompt, or piped from a file (`< phrase.txt`),
  never with `echo`, which keeps them in your shell history — restores the
  same account key there and shows its key id; if the id differs, a word
  was wrong, and running `recover` again with the right words replaces it. The keys
  stay in krowk's home, `device.json` and `account-key.json`, both
  `0600`, and the wrapped account key opens only with that machine's own
  device key.
- **The host daemon can serve its sessions over a WebSocket on this
  machine**, the transport other devices will reach it through once the
  relay lands. It is off unless `KROWK_HOST_WS` or `host.websocket` in
  `config.json` names a loopback address (`127.0.0.1:7788`; port `0` picks
  one, and `krowk host status` shows it); anything but loopback is
  refused. A client proves it is you with the token in `host.token` beside
  the daemon's socket, which is new each time the daemon starts. The
  frames are the unix socket's, sent in compressed batches.
- **The TUI's sessions now run in the host daemon, so closing the
  terminal no longer ends them.** Bare `krowk` starts the daemon if none is
  running and runs every turn there. When the window closes mid-reply, the
  turn keeps going. Reopen `krowk` in the same directory and it tells you
  which session is still running; `krowk --resume <id>` (or
  `/sessions <id>`) then shows what happened while you were away and
  follows the rest live. Several TUIs can follow one session and see the
  same reply. `/connect` updates the daemon's providers too. Set
  `KROWK_TUI_HOST=local` to keep the TUI's sessions in its own process, as
  before. `krowk host stop` stops the daemon once no turn is running.
- **A session can outlive its terminal: `krowk -p --daemon` runs the turn
  in a per-user host daemon.** The first `krowk` that needs the daemon
  starts it in the background; it listens on a private unix socket
  (`$XDG_RUNTIME_DIR/krowk/host.sock`, or under `$TMPDIR` on macOS) and
  exits after ten idle minutes with no session running — `host.idleMinutes`
  in `config.json`, or `KROWK_HOST_IDLE` in seconds, changes that. Kill the
  process or close the terminal mid-turn and the turn goes on; `krowk host
  attach <session>` shows it — what it has done so far, the reply being
  typed, then the rest live — and any number of clients can follow one
  session and see the same events. `krowk host status` says whether the
  daemon runs and what it holds. The daemon asks nobody whether to trust a
  repository, so a backend runs there only once it is trusted. The TUI
  still runs its sessions in its own process for now.
- **`krowk host enable` keeps the daemon running on an always-on machine**,
  as a systemd user service on Linux or a launchd agent on macOS, with no
  idle exit; `krowk host disable` stops and removes it. A service has no
  shell variables, so give it keys with `krowk connect` rather than an
  exported `ANTHROPIC_API_KEY`.

### Changed

- **A synced session opens on one line, not the splash.** `krowk sync
  attach` draws the session's history, then `⇄ Attached to "<title>" on
  <host>` (or `Attached to <host>` for an untitled session) — or that the
  host is away and prompts wait — with the host named as the device list
  names it. The status line says the host in
  a glyph and its name: `● <host>` there, `○ <host>` away, `◌ <host>`
  connecting.
- **`krowk sync attach --json` ends when its stdin does,** once every
  command sent has been answered. Before, it ran until interrupted.

- **Nothing asks you to sign in again in the browser any more.** Starting
  over, `krowk sync recovery new`, `krowk devices remove` and `krowk sync
  recover` use the key you're signed in with; a browser login opens only
  when there is none, and still says what it is for.
- **Sync's device and pairing calls are under `/v1/users/:user_id`.** The
  device list, its append and start-over (`…/devices`, `…/devices/reset`),
  a device's wrapped user key (`…/devices/:id/key`) and the person's one
  pairing (`…/pairing`), for the user the key names. The payloads are the
  same; the stand-in registry answers the new routes.

- **Synced sessions are sealed under your user key.** `krowk sync host`
  seals a new session's key under the newest user key generation this machine
  holds, and records the generation in the wrapped key, which stays 74 bytes.
  `sync attach`, `sync sessions` and `--resume` open any older generation down
  the chain of wraps. A machine holding only an older generation, such as one
  removed before a rotation, can't open a newer session, and is told which
  generation it would need. The user keys a machine holds live in
  `user-keys.json` (`0600`, wrapped to its device key, replaced by rename),
  and a save never drops an older generation's wrap it already holds.
  Each session's record is signed by the machine that published it. Every
  machine that opens a session — `sync host`, `attach`, `sessions` and
  `--resume` — checks that signature against your verified device list
  (`device-list.json`), so any machine of yours can take a session up again,
  and a record no device of yours signed opens nowhere. A session published
  by a machine since removed from your devices still opens to read, but no
  machine hosts it again: start a new one.
  Sessions sealed under the account key no longer open: clean break. No
  command puts a user key or a device list on a machine yet, so these
  commands say it holds none until adding a device does.
- **The client protocol's types live in `krowk-client`.** The commands,
  events and log lines, and the daemon's 28-byte frame header, are
  declared in the crate the desktop app and the phones will link, which
  pulls in no engine, tokio or reqwest; `krowk_harness::protocol`
  still names the same types. The generated JSON Schema is unchanged.
- Leaving the TUI takes two presses, as in Claude Code, so one stray key no longer ends a session. On an empty prompt, Ctrl-C or Ctrl-D shows "Press Ctrl-C again to exit" (or Ctrl-D) under the prompt, and the same key again within 1.5 seconds quits.
- The TUI's prompt sits on the same band as your messages in the chat, edge to edge across the terminal with an empty row either side, instead of between two rules. The arrow and text stay where they were. The status line lines up with the arrow, with an empty row under it, and the working line reads "12s · esc to interrupt".
- Keys the TUI suggests stand out: in hints, the help menu, the status line, approvals and questions, each key (`esc`, `enter`, `y`, `ctrl-g`, `?`) is white instead of grey like the words around it.

### Removed

- **`krowk devices approve` and the 32-hex device code.** Pairing by a short
  code replaces them; `krowk sync join` takes no argument.

### Fixed

- **A krowk session has one id.** `krowk sessions` listed a krowk session
  under an id of krowk.db's own, which `krowk sync host` refused as no
  session. A session is now stored under its log's id, the one `sync host`
  and `--resume` take; `sync host` also takes the id an older store listed
  it under (`krowk sessions rebuild` relists those under their log's id).
- **`krowk sync init` warns about a skipped recovery kit once**, where you
  press [s], rather than again in the line after it.
- **Sync reaches the hosted relay.** `krowk sync host` and `krowk sync
  attach` could not dial a `wss://` relay, so with `KROWK_RELAY_URL` set to
  `wss://relay.krowk.com` the host was never on it: a viewer replayed the
  history but saw nothing live, and its prompts stayed queued. They now dial
  it over TLS with the same trust as every other connection krowk makes, and
  say on stderr why, once, when the relay cannot be joined.
- **`KROWK_RELAY_URL` is no longer needed for krowk.com.** Signed in to the
  production registry, `krowk sync host` and `krowk sync attach` dial
  `wss://relay.krowk.com` by default; a stand-in or custom `KROWK_API_URL`
  keeps the local relay, and `KROWK_RELAY_URL` still overrides both.
- **A host the relay let go joins again.** When the relay dropped `krowk
  sync host`'s link without the close reaching it, or the host was stopped
  or asleep longer than the relay keeps a silent link, the host stayed off
  the relay with no error while viewers showed it gone. It now joins again
  at once, and says why on stderr.
- **A prompt sent as the host went away runs when it is back.** A viewer
  that sent a command the host never received, because its link was lost
  just then, now sends it again when the host returns. The host runs it
  once.
- **`/name` loads the skill on every agent, not only krowk's own.** A skill
  picked from the TUI's slash menu reached Claude Code or Codex as a bare
  `/implement`, and a vendor that did not have that skill answered that there
  was no such skill. krowk now loads the skill's instructions itself and sends
  them ahead of your words, whichever agent runs the session.
- **Skills installed with `npx skills` are found.** krowk reads
  `~/.agents/skills` and `.agents/skills` from the repository's root down to
  the working directory, beside the `.claude/skills` it already read, so a
  skill in the shared directory is listed and can be used with `/name`.
- **A viewer that moves to the direct path mid-session keeps receiving
  the session.** The direct listener replays from where the viewer was when
  it started looking for the direct path. The viewer had already opened
  those batches through the relay, so it never acked them, and once the
  listener's window filled, no further event reached the viewer, though
  its own prompts and their acks still went through. An approval request
  waiting while the viewer moved is now shown once, not once per path. A
  command still running when the viewer moved is answered on the path the
  viewer is on now. A host that reconnects to the relay no longer drops the
  viewers on its direct path.
- **`krowk sync host` no longer hangs on a session this machine does not
  have.** It used to take the session's lease and then wait, silent, until
  interrupted; it now fails at once with `no_session` and how to start one,
  before writing anything to the registry. A bridge that stops by itself
  for any other reason also ends the command then, saying why.
- **A turn sent the moment the host daemon starts no longer stalls every
  other session.** The daemon waited on its thread for the TLS setup a
  turn's first request needs, and read a new directory's configuration
  there too; both now happen off it, so streams and heartbeats keep going.
  A TLS setup that fails once is tried again, rather than failing every
  turn until the daemon restarts.
- **A turn caught up after its client fell behind no longer loses its
  answer.** A terminal or phone that stopped reading, and was caught up
  from where it stood just as the turn ended, could get the typing and the
  end of the turn but not the finished answer. It now always gets it.
- **A terminal whose connection to the host daemon drops mid-turn picks the
  turn up where it left off.** It reconnects and follows on from the last
  thing it had, with nothing shown twice and nothing skipped, instead of
  reporting the turn lost. A model switch caught up by another terminal
  also no longer shows up twice.
- **`krowk host stop` straight after a turn keeps that turn on disk.** The
  daemon waits for the turn's log to be flushed before it exits, for up to
  ten seconds. Stopped by SIGTERM or Ctrl-C, it first interrupts the
  running turns, a turn still starting included, so each is logged as
  interrupted and flushed too. The waits add up: up to ten seconds for the
  turns to end, ten for each backend to close, and ten for the flush. A
  second SIGTERM or Ctrl-C exits at once. A prompt sent while it exits is
  refused rather than lost, an idle exit's included.

- **A terminal suspended while it follows a long session no longer grows
  the host daemon's memory.** The daemon keeps a few megabytes for each
  client that stops reading, then drops what it held and, once the client
  reads again, sends what it missed from where it stopped — nothing twice,
  nothing lost. `krowk host status --json` counts the bytes waiting for
  clients and how often one was caught up.
- **A slow disk no longer stalls every session in the host daemon.** Each
  event a turn logs, and its context record, is written off the thread all
  sessions and heartbeats share, and still before any client sees it.
- Quitting the TUI is instant. On a Claude Code model it took most of a second, waiting on Claude Code to flush its telemetry as it exited; krowk now starts Claude Code with telemetry off (an instance can turn it back on by setting `DISABLE_TELEMETRY` in its `env`). The screen is also handed back before anything else is tidied up, and the session is saved into `krowk sessions` after each turn instead of on the way out, so leaving has nothing left to wait on. A quit now takes about 30 ms, down from 0.9 s.
- A session resumed in the TUI shows what it has cost so far. On a model models.dev has no price for yet, such as a new Claude through Claude Code, the status bar showed `$—` until the next turn ran; it now counts what the backend reported for each past turn, the way the live figure does.

## [0.11.2] - 2026-09-29

Code in the TUI is in colour, and the chat is calmer: your messages stand
on a band of their own, and running calls no longer make it jump.

### Added

- Code in the TUI is in colour. A fenced block in an answer is highlighted by its language, on a faint band across the width with a row of it above and below; its ```` ``` ```` fences are no longer shown, and its language is named at the right of the band's top row. Long lines break at the width and keep their spacing. A file a call writes shows its first lines, highlighted, under the call, and, in a terminal with 24-bit colour, an edit's removed and added lines are highlighted on muted red and green bands (GitHub's dark-mode tints) that keep the code readable. The colours are the terminal's own, so they follow its theme.

### Changed

- What you say in the TUI is shown on a faint grey band across the width, with a thin blue bar down its left edge and an empty row of it above and below, in normal weight rather than bold after a `❯`. Your messages are easy to find when scrolling back, and long ones read as text rather than as a heading.
- A prompt sent in the TUI is shown the moment you press Enter. Before, it appeared only after krowk had chosen the model, read its key and started the backend, which could take a noticeable moment.
- In the TUI, a read, search or command that runs while earlier ones are counted on one line (`◆ Ran 3 commands, searched for 2 patterns`) stands under that line as a branch, with the bullet orange until it is back. The chat no longer jumps as each call finishes and is taken into the count.

## [0.11.1] - 2026-09-29

Answers in the TUI are easier to read: markdown tables are drawn as tables,
and lists hang under their text.

### Added

- The TUI draws markdown tables in an answer the way psql does: indented, with a blank line either side, a bold header, and dim lines between the columns and under the header, but no box. Each cell is in the answer's markdown and wraps inside its column, aligned as the delimiter row says; once a row wraps, rows get a rule between them. On a screen too narrow for the columns, each row is shown as a record of `Header │ value` lines. A table is held while it streams and drawn once it ends.
- Lists in an answer are easier to read in the TUI: a wrapped item's rows line up under its text rather than under its bullet, a list is indented two columns from the text around it, and a nested list is indented by its depth with its own bullet (`•`, `◦`, `▪`). Markers are washed so the text leads: numbered lists (`1.`, `1)`) keep their number, a task list's `[ ]` and `[x]` show as `☐` and `☒`, and a line that goes on under an item lines up with it. A quote's `│` runs down every row it wraps onto.

### Changed

- `@krowk/cli` on npm describes krowk as the coding agent it carries, with publishing as one of the things it does; `@krowk/mcp` says it publishes from any agent. The build-it-yourself line (npm launcher, `prompt.md`) installs the full build (`--features harness`) rather than `sessions`, which left the agent out. The Claude plugin's manifest version, stuck at 0.8.2, is 0.11.0.

### Fixed

- A blank line in an answer no longer goes missing in the TUI when the text streams in with a chunk ending right after it, which ran a heading or a label into the list or paragraph under it.
- A finished todo is `☒`, not `☑`: `☑` has an emoji form, so many terminals drew it larger than the `☐` beside it.

## [0.11.0] - 2026-09-28

krowk is a coding agent now. Bare `krowk` opens its TUI, `krowk -p` runs it
headless, and it runs on an API key or on the Claude and ChatGPT
subscriptions you already have.

### Upgrading

- **Everything krowk keeps moves to `~/.krowk/`**, on the first run after
  upgrading. Config, keys, logins, named accounts, sessions and the price
  cache were spread over `~/.config/krowk`, `~/.local/share/krowk` and
  `~/.cache/krowk`. krowk moves your config and keys in one step, says so
  on stderr, and then deletes the old key files so no secret is left where
  a dotfiles repository may track it. Only krowk's own files are deleted,
  by name. Anything it does not move (the old `krowk.db`, a dev build's
  accounts) is named once with what to do. `XDG_*` variables no longer
  move anything; set `KROWK_HOME` (an absolute path) to keep it all
  elsewhere.
- **Run `krowk sessions rebuild` once.** Costs are now priced per turn by
  the model each turn ran on, and Claude usage is no longer counted once
  per content block (it overstated tokens and cost by about 1.8×).
  `sync` does not re-read sessions already imported.
- **JSON changes for anyone reading costs**: `sessions show` turns drop
  `cost_unknown` and gain `cost_usd` (null when unknown), `cost_source`,
  `model`, `provider` and reasoning and cache token counts. A session's
  `priced_cost_usd` is now `cost_usd`. `sessions` rows gain `unpriced`,
  and the import report gains `unpriced_models`.
- **The release ships two builds, and the installer picks.** The full
  build (agent, TUI, session store) keeps the archive name every release
  has used, so npm and `krowk upgrade` get it. The lean build,
  `krowk-lean_<version>_…`, is installed in CI and containers, and by the
  GitHub Action. `--full` / `--lean` (or `KROWK_LEAN=1` / `0`) choose;
  toolbox and distrobox count as containers, so pass `--full` there.
- **`krowk --help` fits on one screen.** Everything it used to carry is in
  `krowk help <command>`, `krowk help topics` and `krowk help --all`.
  `krowk help --json` keeps its shape.
- With `HOME` unset, krowk no longer uses a `.krowk` beside you as its
  home; commands that need one refuse with `no_home`. On Windows the home
  is `%USERPROFILE%\.krowk`.

### Added

- **The TUI.** Bare `krowk` on a terminal opens an inline prompt with the
  conversation in your terminal's own scrollback, so it scrolls, copies
  and searches like any output, over SSH and in tmux. Answers stream in
  light markdown with clickable links; tool calls are one quiet line each,
  with an edit's lines added and removed and a command's last line of
  output. Typing while a turn runs steers it; Esc or Ctrl-C interrupts.
  Shift+Enter (or Alt+Enter, Ctrl-J) adds a line, Ctrl-Y copies the last
  answer, Ctrl-Z suspends.
  - Commands: `/model`, `/mode`, `/settings`, `/connect`, `/disconnect`,
    `/sessions` (or `/resume`), `/new` (or `/clear`), and your skills.
  - A two-row status line: model, device, task and subagent counts, then
    the branch, its pull request as a link, and the cost. The branch
    follows the agent into the worktree it works in.
  - `tui.contentWidth` sets the column the conversation reads in:
    `prose` (80, the default), `prose-wide` (120) or `full-width`.
  - The window title and herdr show whether it is waiting, working or
    needs your yes.
- **`krowk -p "…"` runs the agent headless**, with `--output-format
  text|json|stream-json`, `--resume <id>`, `--model <instance>/<model>`,
  `--effort` and `--permission-mode`. It never waits on a question: a call
  its rules do not allow is refused, and the model is told why.
- **Every major model, on a key or a subscription.** Native engines for
  Anthropic, OpenAI (Responses API), xAI, OpenRouter and any Chat
  Completions server, plus a SuperGrok subscription. `--model claude/…`
  drives your own Claude Code on your Claude subscription, and
  `--model codex/…` drives OpenAI's `codex app-server` on your ChatGPT
  subscription. krowk never reads either vendor's login; their own CLIs
  sign in. A bare `--model sonnet` runs on whichever instance is ready
  here, and when a key and a subscription could both run it, krowk asks
  you to pick (`ambiguous_model`) rather than guessing.
- **`krowk connect` and `krowk disconnect`** sign providers in and out by
  vendor and method, with as many accounts as you like (`--name work`),
  and the TUI does the same with `/connect`. The first connection becomes
  the default model. An API key can be stored in
  `~/.krowk/credentials.json`, or as a `$VAR` or `!command` reference.
  `krowk providers rename` renames an instance and everything that names
  it.
- **`krowk status`** says which providers can run a turn here and the one
  command that fixes each of the rest. `providers list` and `doctor` use
  the same check.
- **Switch model, instance or engine mid-session without losing the
  thread**, with `/model` or `-p --resume … --model …`. Between accounts
  of the same vendor, krowk moves the vendor's own transcript across.
- **Rate limits are detected on every engine**, and a limited turn ends
  with an offer to continue on another instance. `"rollover": "auto"`
  with a `rolloverOrder` does it by itself; it is off by default.
- **Claude Code's setup works as it is.** Permission rules
  (`permissions.allow`, `ask`, `deny` in Claude Code's syntax), `AGENTS.md`,
  `CLAUDE.md` and Cursor rules, `SKILL.md` skills, command hooks, and agent
  definitions in `.claude/agents/` are all read. A deny rule wins in every
  mode. A repository's own allow rules, hooks and agents count only once
  you trust it, and krowk asks before Claude Code or Codex runs in a
  repository you have not trusted.
- **Permission modes**: `default`, `acceptEdits`, `plan`,
  `bypassPermissions`, and `unhinged`, which runs everything with no
  approval. The TUI asks before a call its rules do not allow: once, for
  the session, or for the project.
- **Tools in the model's own edit format**: `read`, `write`, `grep`,
  `glob`, `bash`, and `str_replace`, `apply_patch` or `search_replace` by
  model family. The file tools stay inside the working directory and out
  of `.git`, `.claude`, `.codex` and `~/.krowk` unless you allow it.
- **Subagents**, several at once on a cheaper model by default, each a
  session of its own under its parent. Ctrl-G lists them and `x`
  interrupts one. Claude Code's background agents are followed too.
- **`todo_write`**, a todo list the agent keeps, counted in the status
  line and shown with Ctrl-T.
- **`publish`**: the agent can push screenshots, diffs and logs as krowk
  artifacts, attached to a krowk run for the session.
- **Budgets**: `--max-usd` and `--max-tokens` stop a session before the
  call that would pass the limit, subagents included (exit 4,
  `budget_exceeded`). `krowk sessions budget` checks any imported session
  the same way, for use in a hook.
- **Native sessions are logs you own**, JSONL under `~/.krowk/sessions/`,
  listed in `krowk sessions` beside imported ones.
- **Provider usage ledgers**: drop a provider's per-request usage export
  in `~/.krowk/ledger/` and imports reconcile it against your transcripts,
  so a request your agent gave up on but was still billed for shows up.
- `krowk login`, `logout` and `whoami`, short for your krowk account.
- `krowk doctor` reports how old the model prices are.

### Changed

- A missing price reads `—`, never $0, and every listing says where the
  rates came from. Costs under a cent print to three significant digits.
- Claude turns keep thinking tokens apart from output, and opencode
  subagents are linked to their parent session.
- `sessions` lists in about 25 ms with a full price cache (was about
  80 ms).

## [0.10.0] - 2026-09-24

krowk is written in Rust now, and it can read your agents' sessions back.

### Changed

- **krowk is written in Rust.** The commands, flags, JSON, error codes and
  exit codes are the ones 0.9.0 had: every recorded case of the Go build
  passed against the Rust one before the Go build was removed. What you
  notice is the size and the speed. `krowk` is 4–5 MB depending on the
  platform (was ~14 MB) and `krowk-mcp` about 2 MB (was ~7 MB). On 707 real
  sessions, `sessions import` takes ~10 s where the Go build took ~14 s, and
  `sessions` lists in 12 ms where it took 23 ms.
- Linux builds are static (musl), so they run in a container with any libc
  or none. macOS and Windows builds are native, as before.
- **The repository is `krowkcom/krowk`** (was `krowkcom/cli`). The old URLs
  redirect, so an installed krowk keeps upgrading, `install.sh` keeps working
  from its old address, and `uses: krowkcom/cli@v0` still resolves. Point new
  workflows at `uses: krowkcom/krowk@v0`.
- From source it is `cargo install --locked --git https://github.com/krowkcom/krowk --features sessions krowk`
  rather than `go install`.
- The GitHub Action installs the release of the repository it was taken
  from, so a fork's action installs the fork's releases.
- Where the two builds differ, it is wording rather than contract:
  human-readable text, key order and escaping in JSON (`<` is written as
  `<`, not `\u003c`), and `doctor`'s `runtime` reads `rust <os>/<arch>`.

### Added

- **`krowk sessions`**: your agents' transcripts, on this machine, in one
  local store at `~/.local/share/krowk/krowk.db`. The files are private to
  you (0700/0600), and nothing leaves the machine.
  - `krowk sessions import --from <claude|cursor|opencode|all>` reads Claude
    Code, Cursor and opencode transcripts into the store. Running it again
    inserts nothing new, so re-running is how you keep the store current.
    `--dry-run` counts without writing, and `--limit N` caps how many
    transcripts each source reads.
  - `krowk sessions sync` is the version for a schedule. It reads only the
    transcripts that changed since the last run, and refreshes the model
    prices once a day (`--no-network` skips that).
  - `krowk sessions` lists every session newest-first, with title, agent,
    model, turns, cost and recency; `--harness`, `--worktree`, `--limit`
    and `--all` narrow it. On a terminal it opens a picker.
  - `krowk sessions show <id>` reads one session back — turns, messages,
    tool calls with their results — from a full id, an 8-character prefix,
    or the agent's own session id. `--thinking` shows thinking in full.
  - `krowk sessions rebuild` deletes the store and re-imports everything:
    the fix when a store is from a version krowk no longer reads. It asks
    first on a terminal, and needs `--yes` anywhere else.
  - One import runs at a time per store; a second one says so at once
    (exit 6) rather than waiting or colliding.
  - Costs are priced from an embedded models.dev snapshot, and
    `krowk pricing refresh` fetches the current prices. A model krowk has
    no price for shows `—`, never 0.
  - `sessions` is not supported on Windows yet, and says so.
- `krowk doctor` reports the session store's health: its path, schema
  version and journal mode, with the command to run when it is not
  healthy.
- The installer no longer overwrites a Claude Code skill directory it did
  not write. It marks the directory it installs (`.managed-by-krowk-cli`,
  `.installed-version`) and refreshes only a directory carrying its mark,
  or an empty one; anything else it leaves alone and says why. A skill
  directory from an earlier installer, holding only its `SKILL.md`, is
  adopted on the next run. If you keep your own `~/.claude/skills/krowk/`,
  move it aside and re-run to have krowk manage it.

### Fixed

- **A git remote with credentials in it no longer reaches the run
  metadata.** A CI checkout clones from
  `https://x-access-token:<token>@github.com/...`, and that URL went into
  `vcs.repository.url.full` verbatim — and run metadata is public on every
  card. The user and password are now dropped. A token already pushed this
  way is on the cards it was pushed with: rotate it.
- The content type a push declares no longer depends on the machine. It came
  from the host's `mime.types`, so the same file could be declared
  differently from macOS and from Linux. krowk now carries its own table of
  the extensions agents produce. `.md` and `.markdown` are `text/markdown`,
  and a `.webm` without a video track is `audio/webm`; an extension outside
  the table is `application/octet-stream` everywhere.
- The installer's log names the repository it downloads from.

## [0.9.0] - 2026-09-06

### Added

- `krowk push --private` uploads where only your workspace can read it, and
  `krowk_push` takes `private: true` for the same thing. The image still embeds
  anywhere: a private artifact's bytes sit on the CDN under a key whose secret
  segment is the whole of the authorization, which is what lets GitHub, Jira or
  Slack — fetching an embed server-side and anonymously, carrying nobody's
  session — render it at all. What changes is the card. `krowk.com/a/{slug}`
  opens only for a signed-in workspace member and answers everyone else exactly
  as it answers a slug that was never minted, and the API read is gated the same
  way, so nothing unfurls a private link.

  It needs an API key and is refused rather than published without one: a
  keyless upload lands in the shared anonymous workspace, which nobody is a
  member of, so there is nothing for it to be private to. The refusal comes
  before anything is sent — an agent told afterwards that its `--private` was
  dropped would have already published the file.

- Every artifact now reports its own `visibility`, on every read, as a name
  rather than a flag. A visibility this build has not heard of is described by
  name and promised nothing, rather than being described as private:
  understating who can read an artifact is the dangerous way to be wrong about
  a privacy feature. `shared` is the visibility whose card a keyless holder
  of the link *does* see, via `share_url`; this build knows it, links it, and
  labels what it unfurls.

- A push that asks for a visibility now checks it was applied before it sends
  the bytes. A registry predating the field accepts the declare and answers
  without it, which is a silent downgrade to public — and once the bytes are on
  a CDN there is nothing left to refuse. Nothing is uploaded, and the artifact
  the declare made is a pending row that expires on its own.

### Changed

- Claiming is plan-aware everywhere krowk explains it. The registry now keeps a
  claimed artifact only when the workspace is on a paid plan; claiming into a
  free workspace moves the artifact and restamps a fresh 24-hour expiry, and a
  keyed upload into a free workspace expires in 24 hours just as an anonymous
  one does. The `expired` fix line, the claim breadcrumb, `krowk help`, the MCP
  tool description and the README no longer promise that a key alone keeps an
  upload. The stand-in registry behind `--dev` follows the same rules: a key
  with `free` in it is a free workspace, every other key is a paid one.

- A `.webm` or `.mkv` file carrying a video track is declared as video, never
  audio. Go's extension table answers `audio/webm` for `.webm`, so a screen
  recording uploaded on the extension alone landed as audio and previewed as
  audio. The uploader now sniffs the Matroska head for a video track and
  declares `video/webm` (or the `.mkv` video kind) when one is there; audio-only
  files keep the extension answer.

- The paste labels stop promising what a private card cannot do. `Paste into
  Slack, Basecamp — they unfurl the link themselves` is true of a public
  artifact and false of a private one, so a private push is labelled for the
  audience that can actually open it, and the breadcrumb that used to say "hand
  this link on — it is public and needs no key to read" says who it opens for
  instead. The markdown label still promises the image, because the image still
  renders. Human output names the visibility beside the size when it is not the
  public default, and `krowk uploads list` names it per row.

- `--format url`, and any `--destination` the registry's table says wants the
  bare link, warn on stderr when what they printed is a private card. That form
  exists to be unfurled and a private card unfurls nowhere, so
  printing one silently would be the same broken promise in a different place.
  The warning is on stderr rather than in the output, because the output is
  about to be pasted.

- The bundled stand-in registry (`go run ./internal/devregistry`) enforces the
  same contract, so a client developed against it behaves the same in
  production: visibility is declared, validated and served; a private artifact's
  metadata answers its own workspace and answers everyone else `404`; its card
  page is indistinguishable from a slug that never existed; its byte URL names
  neither the workspace nor the artifact; and `PUT /v1/artifacts/{slug}/visibility`
  moves an artifact between public, private and shared, re-keying the bytes and killing
  the old URL in every direction. A shared artifact carries `share_url`
  (`{origin}/a/{slug}?share=krowk_share_{24 base36}`) on every artifact
  response, null otherwise; entering shared mints a fresh token and leaving
  clears it; a keyless declare or visibility change naming shared is refused as
  `shared_needs_key`; and `GET /v1/artifacts/{slug}?share={token}` answers 200
  for the matching token and 404 for anything else.

  One behaviour it had wrong is fixed with it: a **public** artifact's metadata
  read is now scoped to no workspace, keyed or not — matching the registry,
  where a scope a reader escapes by dropping the `Authorization` header would
  protect nothing.

  Its `Idempotency-Key` digest is also now taken over the declared artifact as
  an object — the permitted parameters, canonicalized with keys sorted at every
  level — rather than over a list of fields somebody had to remember to extend,
  which is how `metadata` had fallen out of it. Two things follow that a field
  list could not express: a parameter left out stays distinct from one sent
  empty, and a client that re-serialized its own body between attempts gets its
  first answer back rather than a second artifact.

## [0.8.2] - 2026-08-29

### Added

- A spinner on the one thing krowk does that takes long enough to look hung. An
  upload of a few hundred kilobytes over a slow link is four seconds of a
  terminal that has printed nothing, which is indistinguishable from one that has
  stopped, so a single line on stderr says what is being sent and keeps moving —
  naming each file in turn when there are several. It is not progress and does
  not pretend to be: krowk hands the file to object storage in one request and is
  never told how much of it has landed, so a percentage would be a number krowk
  invented. It erases itself before the durable line is printed, so nothing about
  it reaches the scrollback and a transcript reads as though the wait never
  happened. Shown only when stderr is a terminal and the answer is prose:
  `--json`, `--quiet`, `--destination`, the paste formats and any piped stream
  are read rather than watched, and escape codes in a captured file help nobody.

### Changed

- Human output now reads as a person would say it, while the JSON envelope keeps
  every code and every fix string exactly as it was — agents parse the envelope,
  and none of this reaches them.
  - A failure leads with the fix as a sentence rather than with the wire code:
    `✗ Re-encode below 100 MB or push frames separately.` where it used to open
    on `artifact_too_large`. The code is still there, dimmed, one line down and
    in the envelope; a command the fix names is pulled onto its own line so it
    can be copied rather than picked out of prose, and a fix that names two
    things to do says both, one per line.
  - A success reads as a confirmation rather than as the record read back at
    somebody who already knows what they pushed:
    `✓ Uploaded shot.png → https://krowk.com/a/art_2e1d`, with the size, the run
    and the expiry dimmed on the line under it. `claim`, `uploads delete` and
    `runs start` / `runs finish` got the same treatment — `✓ Took art_2e1d down`,
    `✓ Finished run run_7f`, and no wire timestamp read out at a person.
  - An expiry is said the way somebody would say it out loud: `expires tomorrow`
    rather than `expires in 24h`, counted in midnights and in the reader's own
    zone, so an upload at eleven at night expires tomorrow however few hours that
    is. The MCP server still prints the exact duration, since an agent does
    better with a number.
- `krowk help` is now laid out for reading rather than for completeness: the
  commands are grouped under `PUSH & PASTE`, `RUNS`, `UPLOADS` and
  `ACCOUNT & SYSTEM` in two aligned columns, under a `USAGE` block that leads
  with the one command that matters, and it closes on what to type next. The
  groups are written down once and filled from the same catalog `krowk help
  --json` is rendered from, so a command cannot exist in one and not the other.
  The machine surface is unchanged.

## [0.8.1] - 2026-08-28

### Added

- The Krowk mark at the top of `krowk` and `krowk help`: the four-by-four grid
  of squares from the logo, drawn in half block characters so that two rows of
  the grid share one row of text — a character cell is twice as tall as it is
  wide, so a square of the grid drawn as a whole character would stretch the
  mark, and half a character keeps it square at the smallest size it can be
  drawn at. A blank line above and below so it is not jammed against the chrome
  or the words, and no colour, so it takes the foreground of whatever theme is
  running. Under it, `Krowk` and the version on one line and what krowk is on
  the next. It opens what a person reads and nothing else — the JSON surface
  stays a data structure, and one command's help stays an answer to the
  narrower question.
- A moving major tag for the GitHub Action: `uses: krowkcom/cli@v0` follows the
  0.x line rather than freezing a workflow on one patch release, and each
  release moves it once the archives and the npm packages are up. A release tag
  still pins the action and the CLI together, and an explicit `version` input
  still wins over both. The tag has no release of its own, so it installs the
  latest — and the action now refuses to install across a major line rather
  than hand a workflow pinned to `@v0` a 1.x binary with a changed command
  surface.

### Changed

- `krowk` on its own now greets rather than printing the manual. Typing the name
  to see what happens used to answer with every flag, every exit code and every
  paragraph of prose — about 150 lines, and neither "what is this" nor "what do
  I type" was any easier to find for it. It is now the mark, what krowk is, and
  three lines: the first upload, the key that makes uploads keep, and
  `krowk help` for the rest. Those three are the ones the installer signs off
  with, so a first run says what the install said. `krowk help` and
  `krowk --help` are unchanged and still answer in full, and a program reading
  `krowk` — piped, `--json`, or with a `--jq` expression — still gets the whole
  surface, since prose is no use to it and the surface is what it came for.

### Fixed

- The release workflow no longer triggers on every `v*` tag, only on a
  three-component version. The moving major tag is a `v*` tag too, and a
  release run for `v0` would have tried to cut a release of version "0" and
  publish it to npm.

## [0.8.0] - 2026-08-27

### Added

- `--link`, for the links a piece of work is about — the issue it fixes, the
  spec it implements, the discussion behind it. Repeat it for more than one, up
  to twenty, and label each with `--link-title` and classify it with
  `--link-rel` (`tracks`, `fixes`, `spec`, `discussion`, `source`,
  `supersedes`, or a word of your own); both describe the `--link` before them.
  They land on the run as `krowk.links`, an array of `{url, title, rel}`
  objects, so a reader can name a link instead of showing a raw URL. A link
  that is not an absolute `http(s)` URL, one with a space in it, a title over
  140 characters or a rel over 64, either of them carrying a tab, a newline or
  another control character, a twenty-first link, or a set of links
  large enough to crowd out the detected metadata is refused rather than
  trimmed — metadata is stored verbatim and nothing downstream validates it
  again, so a shortened URL would be a link to somewhere else for as long as
  the record lives. `--reference` is
  unchanged and is now the place for identifiers that are not URLs, such as a
  bare ticket key.
- The same links on the MCP `krowk_push` tool, as a `links` array whose schema
  names the suggested `rel` values, so an agent picks from the vocabulary
  rather than inventing one.
- A GitHub Action, `uses: krowkcom/cli@<tag>`, wrapping the CLI for CI: give it
  files or globs, it installs the binary, pushes them, and hands back `urls`,
  a `markdown` paste block ready for a PR comment, the `run-slug` and the
  `json` envelope with its claim tokens stripped — with the links also written
  to the job's step summary. The pull request, repo and commit are detected
  from the runner's environment, the same way they are locally. Pinning the
  action to a release tag pins the binary to that release, an explicit
  `version` input wins over the tag, a directory a glob swept up is named
  rather than handed to krowk, and a `**` glob on a bash too old for one
  (macOS ships 3.2) fails saying exactly that while plain globs keep working.

### Fixed

- Run metadata passed to a push that names an existing run is now reported as
  dropped instead of vanishing. `krowk push shot.png --run run_… --link …`
  records nothing on that run — a run carries the metadata it was opened with —
  and both the CLI and the MCP tool now say so in `notes`, naming the flags and
  the run. `--caption` and `--metadata` are unaffected: they land on the
  artifact. The keyless note gained `--title` for the same reason: it was
  dropped and unmentioned.
- The branch a run records in CI. GitHub checks out a detached HEAD, where
  git can only answer the literal `HEAD` — the branch is read from the
  runner's environment instead, preferring a pull request's source branch
  over the synthetic `412/merge` ref, and taking nothing from a tag push. A
  local detached HEAD records no branch at all now, which is the truth of it.

## [0.7.0] - 2026-08-26

### Removed

- The `registry serve` command. It was a development tool, but it sat in the
  public help and the surface JSON, where an agent reading `krowk --help` would
  take it for a way to host uploads — and host them on a process whose links
  die with it. The stand-in still exists for developing krowk itself: run it
  with `make mock` (`go run ./internal/devregistry`) and point commands at it
  with `--dev` as before.

## [0.6.0] - 2026-08-24

Upgrading from 0.4.1 over npm? This carries everything 0.5.0 did as well —
`--jq`, pasted links where a slug is asked for, and the rest — because 0.5.0
was released on GitHub but never published to npm.

### Changed

- Every paste form krowk prints now comes from the registry verbatim — the
  block, the bare link, and the `destinations` table beside them in the JSON
  envelope. Nothing is assembled in the CLI any more, which is what lets the
  look of a krowk reference change in a single registry deploy, including for
  the installs that already exist. `--format markdown` therefore prints the
  whole block rather than a one-line embed, and several files come back as
  several blocks separated by a blank line rather than one line each.
- The bundled agent skill now says plainly what it only implied: never paste a
  bare artifact link anywhere, use `paste.markdown` / `paste.url` and pick
  between them with the served `paste.destinations` table, and reach for
  `--destination` where the destination is known. It also nudges: an unclaimed
  artifact pasted into a pull request, an issue or a doc becomes a broken image
  once it expires, so the claim step is surfaced to the person before the paste
  rather than after. A test holds the skill to those lines.
- `--title` no longer relabels a pasted link. It is the title of the work and
  lands on the run, as it always did; what a pasted link says about a file is
  now that file's `--caption`, which is recorded on the artifact and read back
  by whatever renders it. `krowk_push` over MCP follows the same rule.

### Added

- `--destination <tool>` on `push` prints what that tool wants pasted into it —
  the krowk block for `github`, `linear` and the other markdown surfaces, the
  bare card link for `slack`, `basecamp` and the others that unfurl one
  themselves. A tool krowk has not been told about gets the block, and the push
  still succeeds: a block where it does not render is informative text, a bare
  link is a link nobody can tell anything about. Which tool wants which form is
  the registry's table, served with the artifact and readable at
  `paste.destinations`, so a tool proving out reaches installs that predate it —
  there is no list of tools inside the CLI. It cannot be combined with
  `--format`, `--json` or `--jq`, which ask for a different rendering of the same
  result; that is refused as `bad_flag` rather than silently ranked.
- `--caption '<text>'` on `push` records what a file shows on the artifact
  itself, as `krowk.caption`, so whatever renders the link later — a card page,
  a pull request comment, an integration — reads the caption off the record
  instead of being told it again at every destination. It is per file and
  repeatable: `krowk push before.png after.png --caption 'Cart before the fix'
  --caption 'Cart after the fix'` captions each one, a single caption covers a
  whole set, and a count that matches neither is refused as `bad_flag` rather
  than guessed at. Distinct from `--title`, which stays a label for the work and
  lands on the run. A keyless push drops it as it drops all metadata, and says
  so in `notes`.
- Ordinary `krowk push` output now ends with the ready-to-paste krowk block, so
  the last thing on screen is the thing worth copying rather than a bare link.
  `--quiet` still prints the record and nothing suggested.
- The paste envelope `krowk registry serve` answers with now carries the krowk
  block itself — the image, the caption from `krowk.caption`, the link through
  to the card, and the expiry of an unclaimed upload — beside the bare link and
  the destination table. It is what the production registry serves, so a paste
  built against the local stand-in looks like a paste built against production.

## [0.5.0] - 2026-08-21

### Added

- `--jq '<expression>'` filters a result inside krowk, with jq compiled in — no
  jq binary to install and no pipe to build. It works on every command, implies
  `--json`, and reads what the command rendered: the envelope normally, the bare
  record under `--quiet`. A string result prints without its quotes, so
  `URL=$(krowk push shot.png --jq '.data.artifacts[0].url')` is the whole
  ceremony; anything else prints as JSON, one value per line. `krowk help --json`
  is filterable too, which is the shortest way for an agent to read the surface.
  A failure is filtered like any other result, in whatever shape the command
  rendered it: `--jq '.error.error'` reads the code out of an envelope, and
  `--quiet --jq '.error'` reads it out of the bare body. An expression that does
  not parse is refused as `bad_jq` before the command sends anything, and one
  that does not fit the result it was pointed at answers `jq_failed` afterwards,
  saying that the command itself succeeded so that a wrapper retrying on a
  non-zero exit does not repeat the work. A failure `--jq` caused is always
  reported whole, since filtering the complaint with the expression behind it
  would bury it. `auth token`, `registry serve` and `--version` print no JSON,
  and refuse the flag rather than ignore it — the surface says which commands
  those are, under `no_json`. Neither can it be combined with `--format human`,
  `markdown` or `url`, since one of the two would have to be discarded.
  `doctor` and `upgrade` answer with a bare record and no envelope, as they
  always have, so filter those as `--jq '.token_source'`.
- Every command that names a record now takes the link as readily as the slug.
  Paste the card page, `https://krowk.com/a/art_…`, the CDN URL under it, or the
  markdown line carrying both, into `uploads show`, `uploads attach`,
  `uploads delete`, `claim`, `runs show`, `runs finish` or `--run` — the slug is
  read out of it, and the MCP tools take one the same way. Anything that is not
  link-shaped is passed on untouched, so slugs behave exactly as they did.
- A link carrying no slug of the kind the command wants now fails as
  `bad_artifact` or `bad_run` (exit 1) before anything is sent, instead of going
  out and coming back as a record that does not exist. A card link handed to
  `runs show` names the artifact it carries, and a link carrying two different
  artifacts is refused rather than acted on — the takedown has no undo.

### Changed

- A `--format` nobody has heard of is now refused even when `--json` or `--jq`
  was passed as well. It used to be accepted and ignored, so a caller who meant
  `--format markdown` and mistyped it was told nothing.
- A claim token is trimmed before it is sent, on `claim` and on
  `uploads delete`, so one copied with a trailing space or newline works instead
  of failing as an unauthorised claim.
- A blank record where one is required is now the command's own missing-argument
  failure rather than a request. `--run "  "` — a shell expanding an unset
  variable — no longer reads as "no run at all", where it used to open a fresh
  run on a push and widen `uploads list` to the whole workspace. A blank
  artifact answers `no_artifact` (exit 1) on both the CLI and the MCP server,
  which previously answered `missing_claim` (exit 3, a credential to fix).
- Failures about a pasted value no longer quote it back. A URL is where
  credentials travel, and a refusal is written to stderr and into the JSON
  envelope.

[Unreleased]: https://github.com/krowkcom/krowk/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/krowkcom/krowk/compare/v0.12.1...v0.13.0
[0.12.1]: https://github.com/krowkcom/krowk/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/krowkcom/krowk/compare/v0.11.2...v0.12.0
[0.11.2]: https://github.com/krowkcom/krowk/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/krowkcom/krowk/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/krowkcom/krowk/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/krowkcom/krowk/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/krowkcom/cli/compare/v0.8.2...v0.9.0
[0.8.2]: https://github.com/krowkcom/cli/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/krowkcom/cli/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/krowkcom/cli/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/krowkcom/cli/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/krowkcom/cli/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/krowkcom/cli/compare/v0.4.1...v0.5.0
