# Changelog

What changed in each release of the krowk CLI, for the people upgrading rather
than for the people who wrote it. Newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the versions are the `v*` tags a release is cut from. Entries land under
`[Unreleased]` as the work merges, and move under a version when it is tagged.

## [Unreleased]

### Changed

- **The TUI's status line is one line in one order**:
  `<user>/<host> | <instance>/<model> | $cost | [N tasks] | [N subagents] | ? help`,
  under the prompt box. The task count is the todo list's open items and
  shows only while there are any; the subagent count only while they run;
  `offline`, in yellow, comes before `? help` while the API cannot be
  reached, and an instance near its rate limit says so beside its model
  (`claude:work/haiku (78% of 7-day)`). Whether an instance runs on a
  subscription or an API key moved to the session details (Ctrl-O), and
  online, `connecting…` and the key hints are no longer shown. On a narrow
  terminal the device goes first, then the counts and the cost, then the
  model is cut short; `? help` stays. `tui.statusItems` takes `device`,
  `model`, `cost`, `tasks`, `subagents` and `help`; a config with the old
  names still reads — `todos` is `tasks`, `instance` is `model`, and
  `connectivity` and `session` are ignored.
- **The working line says what runs, and one duration**: `Running Read
  README.md…` or `Waiting on 2 subagents…`, with the turn's clock on the
  right — no longer `Running for 10s… 10s`.

- **Edits into `.git`, `.claude`, `.codex` and `.krowk` are asked about
  instead of refused**, and so is anything in krowk's own config directory
  or a backend's: the TUI shows the request, and `krowk -p` still refuses
  it. No allow rule or remembered grant opens those directories — only a
  yes for that one call, or `bypassPermissions`. Reading outside the
  working directory is asked about the same way, where it used to be
  refused.

- **The release ships two builds of krowk, and the installer picks.** The
  full build — krowk's agent and its TUI, the session store, everything —
  keeps the archive name every release has used (`krowk_<version>_…`), so
  links, npm and `krowk upgrade` from an earlier release get it. The lean
  agent-container build is `krowk-lean_<version>_…` beside it.
  `curl -fsSL https://krowk.com/install | bash` installs the full build on a
  workstation and the lean one in CI (`CI`, `GITHUB_ACTIONS`, `GITLAB_CI`, …)
  and containers (`/.dockerenv`, `/run/.containerenv`, `$container`, a
  Kubernetes pod, or no terminal at all, as in a Dockerfile `RUN`); a
  pinned release from before the lean build installs its one build; `bash -s -- --lean` / `--full`, or `KROWK_LEAN=1` / `0`,
  choose either way. `krowk upgrade` stays on the build it is, and the
  GitHub Action installs the lean build. **Toolbox and distrobox count as
  containers: pass `--full` there** for the agent.
- **Network failures say "no network connectivity".** When the Anthropic
  API cannot be reached, `krowk -p` now fails with that sentence first,
  still under `network_unreachable`.
- **Costs are priced per turn, by the model each turn ran on.** A session
  that switched models mid-way, or a ledger with three models in it, is no
  longer priced entirely at its last model's rates. `sessions show` prints
  each turn's cost (and a per-model subtotal when there is more than one),
  and a turn whose source reported its own dollars — opencode, a provider
  ledger — shows that figure, marked `reported`. Stores imported before
  this release price every turn by the session's model until
  `krowk sessions rebuild` re-reads the transcripts.
- **A missing price reads —, never $0.** A session with any tokens krowk
  cannot price shows — for its cost, with `unpriced` naming the models in
  `--json`, and `sessions import` names every unpriced model once at the
  end. Every listing and `show` footnotes where the rates came from (the
  refreshed cache's fetch date, or the snapshot embedded in the build):
  costs are today's rates, not the ones in force when the tokens were
  spent. JSON costs are unrounded; costs under a cent print to three
  significant digits, so $0.00007 and $0.00012 no longer both read $0.0001.
- **JSON changes** for anyone reading costs programmatically: `sessions
  show` turns drop `cost_unknown` and gain `cost_usd` (null when unknown),
  `cost_source` (`priced` or `reported`), `model`, `provider` and the
  reasoning and cache token counts; the session's `priced_cost_usd` is now
  `cost_usd`, and `priced_with` reads as a footnote. `sessions` rows gain
  `unpriced`, the list gains `priced_with`, and the import report gains
  `unpriced_models`. `cost_display` prints sub-cent figures to three
  significant digits (`$0.00500`), and a ledger session whose rows all
  sit in transcripts reads `counted elsewhere`. A reported cost of 0 is
  treated as no report — opencode writes 0 for models it cannot price.
- **Claude usage is counted once per API message.** Claude Code writes one
  transcript line per content block and repeats the message's usage on
  each; turns summed every line, overstating tokens and cost (about 1.8×
  on a real session). Each message now counts once, at its final usage.
  Claude turns also keep thinking tokens apart from output
  (`output_tokens_details.thinking_tokens` → reasoning), as opencode and
  ledger turns already did. opencode subagents are now linked to their
  parent session even though opencode lists them first. Run `krowk
  sessions rebuild` to apply all three to sessions already imported —
  `sync` does not re-read them.
- **You can see how old the prices are.** `krowk doctor` has a `pricing`
  check naming when models.dev prices were last fetched and how many days
  ago — a warning past 30 days, or when only the snapshot built into
  krowk answers — with the command that fixes it; `pricing refresh`
  reports `fetched_at_ms` and `age_days` too. `sessions sync` still
  refreshes prices once they are a day old, now judged by the recorded
  fetch time rather than a file's mtime. Nothing else touches the
  network, and there is no background timer. A cache that is missing or
  holds no prices is fetched whole again rather than confirmed by a stale
  ETag. Audio rates stay unread: no
  transcript krowk imports carries audio tokens, and in a provider ledger
  they are priced at the text rate inside input and output — an
  undercount to revisit when one reports audio at all.
- `sessions` lists in ~25 ms on a machine whose price cache holds the whole
  models.dev file (was ~80 ms): the cache is parsed without building the
  fields pricing never reads.

### Added

- **`krowk status` says which providers can run a turn here, and how to
  fix the rest** (in the `harness` build). One row per instance — the
  seven built in and every one you configured — with its kind, its state
  (`ready`, `key not set`, `not signed in`, `expired`, `not installed`,
  `unknown`), where its key or login comes from (`$ANTHROPIC_API_KEY`,
  `Claude Code's own login in ~/.claude`, krowk's SuperGrok login file —
  never the key itself), and the one command or variable that makes it
  ready. It exits 0 when at least one instance is ready and 3
  (`none_ready`) when none is; `--json` gives the rows with the same keys
  on every row, and on exit 3 the rows are the error's `details`.
  **`providers list` and `krowk doctor` now use the same check**: doctor
  gains a `providers` line, and `providers list` rows carry `source` and
  `fix` in place of `auth`, with `state` spelled as `krowk status` spells
  it (`key_not_set`, `not_signed_in`, …). Claude Code and Codex are asked
  at the same time instead of one after the other, so a listing takes as
  long as the slowest vendor (at most 10 seconds, after which it and
  anything it started are stopped) rather than their sum;
  Codex is asked through `codex app-server`'s `account/read`, falling back
  to `codex login status`. A SuperGrok login whose token expired and
  cannot be refreshed now shows as `expired` instead of ready.
- **A switch to a signed-out Claude Code or Codex account is refused
  before anything starts.** `/model claude:work/sonnet`, `--model` on a
  resumed session, and a rollover or its offer now ask the vendor whether
  the account is signed in first, and refuse with the command that signs
  it in (`krowk providers add claude --name work`); the session stays on
  its model. Before, Claude Code was started and the turn failed. A
  signed-in answer is remembered for a minute in the TUI, so switching
  back and forth does not re-ask; a signed-out one is asked again every
  time, so signing in in another terminal works at once. Before a turn the
  vendor is asked in the session's own directory, once its repository is
  trusted — where the turn will start it — so a project that signs
  Claude Code in through its own settings — Bedrock, Vertex, an
  `apiKeyHelper` — still runs; `krowk status` and `providers list` ask in
  a directory of krowk's own, where such an account shows `not signed
  in`.

- **Switch model, instance or engine at any time, without losing the
  thread.** In the TUI, `/model` opens a picker of the models the session
  has run on and every instance you have, and `/model <instance>/<model>`
  switches directly; `krowk -p --resume <id> --model <instance>/<model>`
  does the same headless. Between the native APIs (Anthropic, OpenAI,
  xAI, OpenRouter, anything compatible) nothing is lost but another
  provider's private reasoning, which arrives as marked plain text. **Into
  Claude Code or Codex**, the backend is seeded with krowk's handoff — the
  earlier turns summarized, the last three as they happened, then your
  prompt — and a backend that ran earlier in the session is caught up on
  only what it missed; **back from one** the native model reads its turns
  whole. **Between two accounts of the same vendor** (`claude:work` →
  `claude:personal`, `codex:team` → `codex:personal`) krowk copies the
  vendor's own transcript into the other account's config directory and
  resumes it there, so the context is whole, falling back to the handoff
  when the copy or the resume fails. A switch that cannot run — no key, no
  login, no binary — is refused with the fix, and the session stays on the
  model it was on; one that fails on its first turn goes back by itself.
  Switching never loosens the permission mode, and a session's budget
  counts what it spent on every model.
- **Rate limits are detected on every engine, and offered around.** A
  turn that hits its instance's limit (an API's 429, Claude Code's plan
  limit, Codex's usage limit) ends with an offer — "claude:work limited
  until 14:00, continue on claude:personal? [y/N]" — which `y` accepts in
  the TUI and `krowk -p` prints as a `--resume … --model …` fix. It is
  never taken silently. With `"rollover": "auto"` and an ordered
  `"rolloverOrder"` in `config.json`, krowk moves to the next instance by
  itself, tells every client, and logs where from, where to and why. It is
  off by default, and stacking one plan's limits across accounts is your
  own call: Anthropic's plan limits assume ordinary, individual usage.
  Each instance's usage and how near its limit it last said it was show in
  the TUI's session details (Ctrl-O), and in the status bar once it is
  close.
- **krowk's agent follows Claude Code's permission rules, so a repository
  set up for Claude Code needs nothing new.** `permissions.allow`, `ask`
  and `deny` are read in Claude Code's syntax — `Bash(git:*)`,
  `Bash(npm test)`, `Read(./secrets/**)`, `Edit(src/**)`,
  `WebFetch(domain:docs.rs)`, `Mcp(github:create_issue)` — from
  `.claude/settings.json` and `.claude/settings.local.json`, your own
  `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`), the `permissions` key
  of krowk's `config.json`, and a repository's `.krowk/config.json`, along
  with `defaultMode` and `additionalDirectories`. **A deny rule wins in every
  mode, `bypassPermissions` included**, and a command line is judged the way
  the shell runs it: `git status && rm -rf x` is two commands, `sudo rm`,
  `bash -c 'rm …'` and `find -delete` still meet `Bash(rm:*)`, and an allow
  rule never covers a line that writes a file through `>`. **Git is
  allowed fail-closed**: `Bash(git:*)` covers a git command only when its
  global options, and the options of any subcommand that can run a program
  or install hooks (`clone`, `fetch`, `push`, `rebase`, `grep`, `config`,
  …), are all on a known-safe list — anything else, abbreviated or
  bundled, is asked about. `git log`, `git status`, `git commit -m`,
  `git push -u origin main`, `git clone <url>`, `git grep x` and
  `git config --get` are still covered; `--git-dir`, `--work-tree`, bare
  repositories, and a git after `cd` are not. A command whose program name the shell computes (`$X`,
  `$(printf rm)`, `r{m,}`) is asked about whenever a `Bash` deny rule
  applies, even under `bypassPermissions`, and refused by `krowk -p`. Plan mode
  refuses every edit and command. `bash` now runs in `default` and
  `acceptEdits` when a rule allows the command. Claude Code backends are
  started with your deny rules as `--disallowedTools`, and Codex's
  commands and patches are judged by the same rules. **A repository's own
  allow rules, extra directories, `defaultMode` and hooks count only once
  you trust it** (the same trust list and `--trust` as a backend's); its
  deny and ask rules always count. `publish` is judged by the same rules:
  it runs where an edit does, and a file a `Read` deny rule covers is never
  uploaded. A settings file that does not parse stops the prompt and names
  the file.
- **The TUI asks before a call its rules do not allow.** A command, an
  edit in the default mode, or anything reaching outside the working
  directory shows over the prompt with why it is asked (a long one cut to
  fit is printed whole with `v` before it can be allowed): `y` allows it once,
  `s` for the rest of the session, `p` for this project from now on
  (remembered in krowk's own `permissions.json`), `n` or Esc refuses it.
  The request is part of krowk's protocol (`approval.requested`, answered
  by `approve`), so any client can answer it — the phone, once the daemon
  lands. **`krowk -p` never waits on a question**: the call is refused, and
  the model is told which allow rule or `--permission-mode` would have
  allowed it.
- **Instructions, skills and hooks from Claude Code, Codex and Cursor load
  as they are.** krowk's agent reads `AGENTS.md`, then `CLAUDE.md` and
  `CLAUDE.local.md`, then `.cursor/rules` and `.cursorrules`, in every
  directory from the repository root down to where it runs (deeper files
  win), after your own `~/.claude/CLAUDE.md`. Claude-format skills
  (`SKILL.md` in `.claude/skills`, `~/.claude/skills` or krowk's config
  directory) are listed by their description, and a skill's full text is
  loaded only when the model uses it. Claude-format command hooks run for
  `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse` and
  `Stop`, with Claude Code's JSON on stdin; a hook that exits 2 blocks, and
  the model reads its reason.

- **Bare `krowk` on a terminal opens krowk's own agent.** It opens on a
  clean window — what was on screen scrolls up into scrollback, kept — with
  one header line (`krowk · opus via claude · ~/project`), the prompt in a
  box (`→ Plan, search, build anything`) and one row under it for the
  model, the keys worth knowing and the cost. A repository it has not
  trusted yet is asked about as a short card answered with one key — `y`
  trusts it; any other key, or anything typed before the card showed,
  does not; Ctrl-C leaves. The
  window title says what it is doing (`✳ krowk` waiting, `◑ <what you
  asked>` working, `✋` when a call waits for your yes), and inside herdr
  krowk reports the same to its pane, so herdr lists it as an agent with
  live status and notifications. An inline prompt
  at the bottom of the terminal, with the conversation going into the
  terminal's normal scrollback as it finishes — no alternate screen, so it
  scrolls, copies and searches like any other output, and works over SSH,
  in tmux and in phone terminals. Answers stream at up to 60 frames a
  second, each frame one synchronized update, and an idle prompt uses no
  CPU at all. Enter sends; Alt-Enter, Ctrl-J or a trailing `\` adds a line,
  and ↑/↓ walk the prompt history. Esc or Ctrl-C interrupts a turn and
  keeps what arrived; typing while it runs steers it, and the model reads
  it before its next step. `krowk --resume` picks a krowk session to
  continue from a list (`--resume <id>` names one), `--model` and
  `--permission-mode` work as they do for `-p`. A status bar shows the
  model, the instance, the session's cost and connectivity — configurable,
  or off, under `"tui"` in `config.json` (see the README) — and `?` and
  Ctrl-O toggle the keys and the session's details. When the model's API
  cannot be reached, a persistent **no network connectivity** notice
  appears within two seconds and clears when it answers again (behind a
  proxy — `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` — the proxy is what is
  checked). Ctrl-Z stops to the shell and `fg` brings the prompt back;
  SIGTERM and SIGHUP leave the terminal as they found it. Each tool call
  is shown once, with its outcome (`◆ Read README.md (3 lines)`, an edit's
  removed and added lines on red and green bands, a command's head and
  tail), thinking as `◆ Thought for 4.2s`, and answers in light markdown;
  the look follows xAI's Grok Build. Everything sits two columns in from
  both edges; the left padding is moved over, never written, as Claude
  Code does it, and krowk wraps answers inside it itself, so lines already
  printed keep their width when the window changes. Ctrl-Y copies the
  last answer as the model wrote it, without the padding or the wrapping,
  to the clipboard (OSC 52, and `wl-copy`, `xclip` or `pbcopy` where
  there is one). The prompt sits
  at the bottom of the terminal, what was on screen moved down to meet
  it, so narrowing the window never leaves a copy of it in scrollback.
  Resizing mid-answer neither repeats nor drops the line being streamed:
  every frame moves from the cursor rather than to numbered rows, so one
  the terminal reads after it has changed size still lands where it
  should, and a shorter window scrolls the conversation up rather than
  clearing a line of it. **Known limits:** a resize the terminal takes in
  the middle of a single frame can still cost that line, and a frame read
  after the window narrowed (the prompt box spans the window) can blank
  the line above the prompt area; krowk takes a terminal to reflow on
  resize unless it is real xterm (`XTERM_VERSION`) or the Linux console;
  one that is taken wrongly can leave a copy of the prompt area in
  scrollback, or blank a few lines above it. A live region that reflows
  taller than the whole screen (a very narrow window under a long overlay)
  leaves its top rows in scrollback. Steering an interrupted turn never read comes back into the
  prompt, and on `-p`'s result as `unreadSteers`. With stdin
  or stdout not a terminal, bare `krowk` prints exactly what it always has.
- **`krowk -p "…"` runs krowk's own agent, headless, on the Anthropic API.**
  The release's full build carries it; the lean agent build does not. The prompt comes from the arguments, or from stdin when there
  are none; the key from `ANTHROPIC_API_KEY`, and `ANTHROPIC_BASE_URL`
  points it at a router or a stand-in. `--output-format text` prints the
  answer, `json` the `result` event, and `stream-json` every event as it
  happens — `item.started`/`item.delta`/`item.completed` per item, then a
  `result` with usage (input, output, cache read, cache write, reasoning),
  the cost at models.dev prices, the duration and the session id.
  `--model <instance>/<model>` picks the model (a bare Claude id runs on
  the `anthropic` instance; `claude-opus-5-5` by default), `--resume <id>`
  continues a session by the id its result named or its `krowk sessions`
  id, and Ctrl-C stops a turn and keeps what it made. Its `bash` tool runs
  only under `--permission-mode bypassPermissions` until permission rules
  land, and is refused with a reason the model can read otherwise (its
  other tools are below). Its output is the command's stdout and stderr
  through one pipe, in the order they were written. Prompt caching is on by default.
- **`krowk -p`'s agent can change files, in the edit format its model was
  trained on.** Beside `read` and `bash` it now has `write`, `grep`, `glob`
  and one edit tool: `str_replace` for Claude models, `apply_patch` (the
  Codex patch envelope) for GPT and Codex models, and `search_replace` for
  Grok models, picked from the model's family in the models.dev cache, or
  from its id when the cache does not know it. `--toolset claude|gpt|grok`
  picks one for a prompt, and `"toolset"` in `config.json` for every
  model. An edit that matches twice, or no longer matches the file, and a
  patch that does not apply, change nothing and tell the model why. `grep`
  and `glob` skip what `.gitignore` excludes and never search binary
  files. `write` and the edit tools run only under `--permission-mode
  acceptEdits` or `bypassPermissions` until permission rules land; in the
  default mode the model is told it may not. Every file tool — `read`,
  `grep` and `glob` included — reaches only inside the working directory,
  judged by where a path really leads (`..`, absolute paths and symlinks
  that point out are refused), unless krowk runs with `bypassPermissions`.
  Nothing inside a `.git` directory is changed by them either (git runs
  what its config and hooks name), and a read-only file is refused rather
  than replaced. Edits are written to a temporary file and renamed into
  place, keeping the file's permissions, so a failure never leaves half a
  file. Each turn's `context.jsonl`
  record now names its `toolset` and estimates the system prompt's and
  tools' size in tokens (`systemTokens`, `toolsTokens`, at four bytes a
  token).
- **`krowk -p` runs on OpenAI, xAI, OpenRouter and any Chat Completions
  server, and on a SuperGrok subscription.** `--model openai/gpt-5.4`
  reads `OPENAI_API_KEY` (and `OPENAI_BASE_URL`), `xai/…` reads
  `XAI_API_KEY`, `openrouter/…` reads `OPENROUTER_API_KEY`, and a bare
  `gpt-…`, `o3` or `grok-…` id now runs on `openai` or `xai` rather than
  `anthropic` — pass `anthropic/<id>` for a router that serves them there.
  GPT models go through OpenAI's Responses API, stateless (`store:
  false`): their encrypted reasoning is kept in the session log and sent
  back as it came, every call carries the session as `prompt_cache_key` so
  a second turn reads the cache, and `apply_patch` is offered to GPT-5 and
  Codex models as a freeform tool, in their own patch format. xAI,
  OpenRouter and compatible servers go through Chat Completions, with each
  vendor's reasoning fields (`reasoning_content`, `reasoning_details`) sent
  back to it unmodified. Which API a model is served on, and which
  reasoning efforts it takes, come from the models.dev cache. Reasoning
  another provider produced is passed to the next model as plain text,
  marked as an earlier model's, never as its own words.
- **`--effort none|minimal|low|medium|high|xhigh|max`** sets how hard the
  model thinks, on one ladder for every provider: the rung is mapped onto
  the nearest one the model takes (`max` on a model that tops out at
  `xhigh` sends `xhigh`), and a model that takes none is sent none.
  `"effort"` on an instance in `config.json` sets it for every prompt.
- **`krowk providers add|list|remove`** manages the engine's instances
  (in the `harness` build). `krowk providers add openai --name work
  --base-url https://gateway.example/v1` writes an `openai:work` instance
  that reads its key from `$OPENAI_WORK_API_KEY` — the variable's name is
  stored, never the key. `openai-compatible --name local --base-url …`
  adds any Chat Completions server. `krowk providers add supergrok` signs
  in to xAI with a SuperGrok or X Premium subscription in the browser
  (`--device` prints a code to enter anywhere instead) and keeps the
  tokens in `~/.config/krowk/providers/credentials.json`, created `0600`
  and refreshed as they expire; `krowk -p --model supergrok/grok-4.7`
  then runs on the subscription. `list` shows which instances have their
  key or login; `remove` takes a definition and its login away.
- **`krowk -p` can run Claude Code on your Claude subscription, with as
  many accounts as you like.** `--model claude/sonnet` drives the `claude`
  already on your PATH, signed in the way you signed it in; `krowk
  providers add claude --name work` makes a second account, `claude:work`,
  with a config directory of its own (under
  `~/.local/share/krowk/claude/`, or `--config-dir`) and signs it in by
  running `claude auth login` — Anthropic's own login, on your terminal.
  krowk never reads Claude's credentials or keychain entry; `providers
  list` asks `claude auth status`. One `claude` process serves the whole
  session: its turns stream into the same log and `krowk sessions` listing
  as native ones, Ctrl-C interrupts the turn and keeps the session, `-p
  --resume` continues it on `claude --resume`, and the log records the
  Claude session id, its transcript's path and whether it ran on the
  subscription or an API key (`backend.session`). Claude Code runs in
  its `default` mode (`plan` under `--permission-mode plan`) whatever its
  settings' `defaultMode` says, and a turn it reports in a looser mode is
  stopped before it runs anything. Tool calls Claude Code asks about are
  answered by krowk's `--permission-mode` (edits need `acceptEdits` and
  never reach `.git`, `.claude` — in any case, as macOS and Windows would
  open it — or the account's config directory; Bash and anything else
  need `bypassPermissions`). The native edit tools keep out of `.claude`
  the same way. **Claude Code's own allow
  rules (`permissions.allow` in your or the project's settings) and its
  hooks still apply first**: what they approve runs without krowk being
  asked, as it does when you run `claude` yourself. `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_BASE_URL` set for krowk's native
  engine are not passed to Claude Code, so an exported key never moves a
  subscription account onto it. A router is an instance of its own:
  `krowk providers add claude --name router --base-url
  https://openrouter.ai/api --api-key-env OPENROUTER_API_KEY` stores the
  variable's name, and krowk hands its key to Claude Code as
  `ANTHROPIC_AUTH_TOKEN` (as `ANTHROPIC_API_KEY` without a base URL, for a
  Console key); no Claude login is run for it. Stopping
  a session stops everything Claude Code started with it, and a host that
  keeps sessions lets an idle one's process go after 15 minutes. krowk's
  own tools reach Claude Code as the `krowk` MCP server: `session_info`
  and `publish`.
- **`krowk -p --max-usd 0.50` (or `--max-tokens N`) stops the session
  before the model call that would take it past the limit.** The check runs
  inside krowk's engine before every call, from what the provider metered —
  never from the `max_tokens` a request was sent with, which providers
  overshoot — over the session and every subagent it spawned, and the
  next call is counted at the least it can cost (its prompt resent from
  cache and one token), so a call that could fit is never refused. A
  stopped session exits 4 with `budget_exceeded`, as `krowk sessions
  budget` does, keeps what it made, and says how to go on (`krowk -p
  --resume <id> --max-usd <more>`); a model with no price trips
  `--max-usd` rather than passing it. A limit holds the invocation it is
  given to — it is not stored with the session, so a `--resume` without the
  flag runs with none — and counts the session's whole spend, earlier turns
  included. Claude Code sessions are checked before each turn and
  interrupted as soon as a metered call goes over; what Claude Code's own
  subagents (`Task`) spend counts too, and so does Claude Code's reported
  total for a turn when it is more than krowk priced (or when the model is
  an alias like `sonnet` that the price list does not name).
  Bare `krowk` takes the same flags for its TUI, whose cost item now shows
  the session's spend after every call, subagents included. For krowk's
  own sessions this replaces a `krowk sessions budget` hook; the command
  still checks imported Claude Code and opencode sessions.
- **krowk's agent can publish evidence: `publish` pushes screenshots,
  diffs and logs as krowk artifacts.** Like an edit, it runs only under
  `--permission-mode acceptEdits` or `bypassPermissions` — an artifact is
  readable by anyone with its link — and elsewhere the model is told which
  mode it needs. It is `krowk_push`, run with the
  session's working directory as the root — the same refusals of paths
  outside it, credential files and hard links — and it answers with each
  artifact's card URL and markdown embed. With an API key, a session's
  first publish opens a krowk run recording the session, every artifact is
  tagged `krowk.session` and attached to that run, and a resumed session
  keeps publishing under it (`run.opened` in the log). Without a key the
  upload is anonymous and belongs to no run, and the answer points at
  `krowk doctor`; the claim command that keeps such an upload is printed
  for you on stderr (shown on screen in the TUI) and never given to the
  model, written to the session log or printed by `stream-json`, because its
  token is a secret. Detecting the run's repository and commit runs git with
  the repository's `core.fsmonitor` switched off, in the session's own
  directory, so a repository's config cannot make it run a command — this
  holds for `krowk push` and krowk-mcp too. **`krowk.vcs.dirty` is left
  out wherever a git filter is configured** (a `filter.<name>` in the
  repository's or your git config, git-lfs's included): telling whether a
  file changed can mean running its clean filter, and a repository can name
  any command as one. `--dev` publishes to the stand-in registry. Claude Code
  sessions get the same tool as `mcp__krowk__publish`.
- **krowk's agent can hand work to subagents, several at once.** The
  model's new `subagent` tool starts a child session with a fresh context
  — only the prompt it is given — that does one task and hands back only
  its final summary, so a search or a review that reads fifty files costs
  the main conversation a paragraph. The subagent calls of one response run
  in parallel, four at a time (`"subagents": {"maxParallel": N}` in
  `config.json`), and each runs on a cheaper model by default: the
  catalog's tier below the session's (Opus to Sonnet, Sonnet to Haiku, a
  GPT to its mini), or `"subagents": {"model": "…"}`. A subagent runs in
  the session's permission mode, never a looser one, with the same file
  fences, on krowk's own loop, and starts no subagents of its own. Its
  calls are judged by the session's permission rules and hooks like the
  session's own — a deny rule holds in it, an approval it needs is asked
  in the TUI under its own line (and refused at once under `krowk -p`),
  and a session grant covers it. `subagent` and `todo_write` need no mode,
  but a deny or ask rule on Claude Code's names for them (`Task`,
  `Task(<agent>)`, `TodoWrite`) refuses or asks, and hooks see them under
  those names — an agent's name matched regardless of case, and a `Task`
  hook reading Claude Code's `{description, prompt, subagent_type}`. As in
  Claude Code, a subagent fires `SubagentStop` when it
  is done — not `Stop` or `UserPromptSubmit` — and its hooks get the
  parent's `session_id`, with the subagent's as `agent_session_id`. Its
  spend counts toward the session's `--max-usd` and `--max-tokens`: a
  subagent's calls are held to the parent's limit, and the parent's next
  call counts what its subagents spent; subagents running at once can
  together end up to one call each past the limit. A turn's `costUsd`
  includes its subagents'. In the TUI each subagent is one
  line — what it is doing, how long, its tokens and cost — and Ctrl-G
  selects among them: Enter expands a line to what that subagent did last,
  `x` interrupts that one alone, and its siblings carry on; Ctrl-C still
  interrupts the whole turn, subagents included. Each subagent is a session of its own in
  `krowk sessions`, listed under the session that started it, and
  `krowk sessions rebuild` restores the tree from the logs. `-p
  --output-format stream-json` now carries the subagents' lines too, each
  under its own `sessionId`. Codex's own subagent threads are metered into
  the session's spend as well, as Claude Code's already were.
- **Agent definitions, krowk's and Claude Code's.** A Markdown file with a
  `name`, a `description` (when the model should use it), a `model` and a
  `tools` allowlist, the body being its instructions, in the repository's
  `.krowk/agents/` or `.claude/agents/`, or your own
  (`~/.config/krowk/agents/`, `~/.claude/agents/`), is offered to the model
  by name. Claude Code's files work as they are: `tools: Read, Grep, Bash`
  maps to krowk's tools (what krowk has no tool for, like `WebFetch`, is
  left out), and `model: haiku`, `sonnet`, `opus` or `inherit` pick the
  newest of that family the catalog lists (`krowk pricing refresh` fills
  it) or the session's own model. A definition only narrows: the
  permission mode is always the session's. **A repository's definitions
  can pick a model only on the session's own instance until you trust the
  repository** (the list `krowk -p` asks about for Claude Code and Codex,
  or `--trust`); otherwise the subagent runs on the default and you are
  told. They are never read through a symlink out of the repository, and
  the agent cannot write `.krowk/` (like `.claude/`) below
  `bypassPermissions`.
- **`todo_write`: the agent keeps a todo list.** One tool that replaces the
  whole list each time (up to 50 items; Claude Code's `activeForm` and
  similar fields are accepted and dropped), for work of three or more
  steps; the list is in the
  session's log, so it survives `--resume` and a switch of model. Ctrl-T
  shows it in the TUI, and the status bar counts it (`todos 2/5`, and
  `2 agents` while subagents run; both are `tui.statusItems`). When items
  have stayed open for ten model calls without an update, the model is
  reminded of the list — shown in the TUI as krowk's reminder, not as your
  words.
- **krowk asks before Claude Code runs in a repository you have not
  trusted.** `claude -p` runs a repository's hooks and MCP servers without
  its usual trust prompt, so krowk shows its own on a terminal — `krowk
  -p`, and bare `krowk` before the TUI opens — and
  remembers a yes in `~/.config/krowk/trusted.json` for that repository
  alone (not for repositories inside it); without a terminal it refuses
  (exit 4, `untrusted_directory`) unless you pass `--trust`, which lasts
  for that run. Your home directory and `/` are never trusted for good —
  in a home kept in git, every plain directory is part of that repository
  — so only `--trust` runs Claude Code there. A native model runs nothing
  of the repository's and is never asked about. Steering (typing while a
  turn runs, in the TUI) is not taken by a Claude Code turn: it comes back
  into the prompt, unsent.
- **`krowk -p` can run Codex on your ChatGPT subscription, with as many
  accounts as you like.** krowk drives OpenAI's own `codex app-server` —
  the interface OpenAI built for other programs to use Codex — with the
  `codex` already on your PATH: `--model codex/gpt-5.5` runs it signed in
  the way you signed it in. `krowk providers add codex --name team` makes
  a second account, `codex:team`, with a `CODEX_HOME` of its own (under
  `~/.local/share/krowk/codex/`, or `--config-dir`) that links in your
  Codex `config.toml`, `AGENTS.md`, prompts, skills and rules, so the
  accounts share one configuration while each keeps its own login and
  threads, and signs it in by running `codex login` — OpenAI's own login,
  on your terminal (`--device` for its device code). krowk never reads
  Codex's login file or uses Codex's OAuth client; `providers list` asks
  Codex itself (`account/read`, else `codex login status`). One `codex app-server` serves the whole session:
  its turns stream into the same log and `krowk sessions` listing as
  native ones, with the commands Codex ran and the patches it applied as
  tool calls; typing while a turn runs steers it; Ctrl-C interrupts it
  and keeps the session; `-p --resume` continues the same Codex thread;
  and the log records the thread, its transcript's path and whether it
  ran on ChatGPT or an API key, which the TUI's status bar shows beside
  the instance. Codex runs in its read-only sandbox with every approval
  routed to krowk — whatever its config's `sandbox_mode` or
  `approvals_reviewer` say; a thread it opens looser is stopped before a
  turn runs — and krowk answers by `--permission-mode`: a patch needs
  `acceptEdits` and never reaches `.git`, `.codex`, `.claude`, the
  account's home or your own Codex home — a move judged by where it lands;
  a command beyond the sandbox needs `bypassPermissions`, which is Codex's
  full access. **The MCP servers your Codex config names do not run
  outside `bypassPermissions`**: krowk turns each off on the thread, as it
  keeps Claude Code's out with `--strict-mcp-config` (servers an
  installed Codex plugin brings may not be listed, and are not covered
  yet). **What the sandbox lets a command do
  without asking — read your disk, not write it — and what your own Codex
  rules allow, still apply first.** `OPENAI_API_KEY`, `OPENAI_BASE_URL`,
  `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, `CODEX_SQLITE_HOME` and Codex's
  other identity and endpoint overrides in krowk's environment are not
  passed to Codex. Writes Codex makes to its config (trusting a project)
  land in your own `config.toml`, which the accounts share; its bundled
  skills stay in each account's home. A skill you add to your own Codex
  later reaches every account the next time it starts. A router is an instance of its own: `--api-key-env NAME` hands
  that variable's key to Codex under the same name, for the model
  provider its `args` name. krowk's own tools reach Codex as its dynamic
  tools — today `session_info` — and the trust question above covers a
  repository's `.codex` too. The native edit tools keep out of `.codex`
  the way they keep out of `.git`. The app-server's schema is pinned for
  the Codex version in `crates/krowk-harness/schema/codex/VERSION`, and
  `scripts/codex_schema.sh --check` fails CI when it goes stale.
- **A second Ctrl-C leaves at once during a Claude Code or Codex turn,
  and takes the backend with it.** Headless or in the TUI, the vendor
  process and everything it started are killed rather than left running,
  and the TUI no longer hangs waiting on the turn it was asked to abandon.
- **Native sessions are logs you own, listed beside imported ones.** Each
  session is an append-only JSONL log under
  `~/.local/share/krowk/sessions/<id>/` (with each turn's exact system
  prompt and tool definitions beside it in `context.jsonl`), and it lists
  in `krowk sessions` as harness `krowk` next to Claude, Cursor and
  opencode sessions. `krowk sessions rebuild` re-derives them from the logs
  like any other transcript, and `sessions import --from krowk` reads them
  alone. The event format is generated as JSON Schema in
  `crates/krowk-harness/schema/`.
- **Instances in `config.json`.** An `"instances"` map names provider
  accounts — `{"anthropic:work": {"kind": "anthropic-api", "apiKeyEnv":
  "WORK_ANTHROPIC_KEY"}}` — and `"defaultModel"` the model `-p` uses. A key
  is never stored in the file, only the variable it is read from.
- **The performance and size promises fail CI when broken.** Every number
  krowk promises is in `crates/krowk-bench/budgets.toml`, and `make bench`
  (run by CI on every pull request, on one pinned runner) holds the release
  builds to it: the agent build's size (3.76 MiB, budget 4.00 MiB) and its
  136 crates, the full build's size, `krowk --version` and `krowk sessions`
  startup, a session log append (under 1 ms; ~8 µs today), and `krowk -p`
  waiting on a provider using no CPU, no wakeups and under 30 MB. Each is
  the median of repeated runs against the absolute number, never against
  the last run. The TUI, redraw and remote-attach budgets are listed and
  shown as pending until the features they measure exist.
- **`krowk sessions budget <id> --max-usd N --max-tokens N`** checks a
  session against a spend limit by what the provider metered, never by the
  `max_tokens` its requests asked for — providers do not strictly enforce
  it (a call capped at 1,200 has metered 3,422). It re-reads the session's
  transcripts, and its subagents', when they moved — importing a session
  the store has not seen yet, by its Claude or opencode id — and waits
  for a running import rather than answering stale (after 15 s it gives
  up with `import_locked`, exit 6 — which the hook recipe below lets
  through; test `-ne 0` instead to block on that too). `--max-tokens`
  holds generated tokens (output and reasoning); input and cache tokens are reported and priced. Within
  its limits it prints the report and exits 0; over one it exits 4 with
  `budget_exceeded` and the report under `error.details` (on stderr). A
  cost krowk cannot price trips `--max-usd`, with the priced part as a
  lower bound. krowk cancels nothing — the hook or wrapper that runs the
  check stops the run. A Claude Code hook blocks only on exit 2, so block
  on a trip alone: `krowk sessions budget "$ID" --max-usd 5; [ $? -ne 4 ]
  || exit 2`. A provider-ledger row nobody saw an answer to is budgeted
  with its ledger's session: nothing ties it to the run that sent it.
- **Provider usage ledgers.** A request your agent gave up on — a timeout,
  a killed shell, a Ctrl-C — can still finish and bill on the provider's
  side, and no transcript ever sees it. Drop the provider's per-request
  usage export as JSONL into `~/.local/share/krowk/ledger/<name>.jsonl`
  (one execution per line: `id`, `provider`, `model`, token counts,
  optional `cost_usd` and `time`) and `sessions import --from ledger`
  (or `all`, `sync`, `rebuild`) reads it. Every import then reconciles the
  ledger against your transcripts: a row a transcript already accounts for
  is counted once, from the transcript; the same execution in two exports
  is counted once; a row nothing local saw shows up as its own
  `unobserved` turn in `krowk sessions` and `sessions show`. The import
  report says how many of each it found.

### Fixed

- **A Claude Code `defaultMode` krowk does not run no longer refuses every
  prompt.** `"defaultMode": "auto"` in `~/.claude/settings.json` (or any
  mode krowk does not know) used to fail the settings with `bad_settings`,
  even with `--permission-mode` given. Such a mode now sets nothing: a
  mode in krowk's own config (`permissions.defaultMode` in
  `~/.config/krowk/config.json`) or `--permission-mode` wins without a
  word, and with neither krowk runs in `default` and says so in one line.
  That is Claude Code's `auto`; any other mode krowk does not run
  (`dontAsk`, a value that is not a mode) is read as `default`, so it
  still narrows a looser mode set before it. A rule that does not parse
  still refuses the prompt, and the TUI now says so before it asks the
  trust question rather than after saving the answer.

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

[Unreleased]: https://github.com/krowkcom/krowk/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/krowkcom/krowk/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/krowkcom/cli/compare/v0.8.2...v0.9.0
[0.8.2]: https://github.com/krowkcom/cli/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/krowkcom/cli/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/krowkcom/cli/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/krowkcom/cli/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/krowkcom/cli/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/krowkcom/cli/compare/v0.4.1...v0.5.0
