---
name: krowk
description: |
  Publish agent output — screenshots, logs, diffs, build artifacts — to a permalink
  with the krowk CLI, and paste that link where a person will read it. Uploading
  needs no account; an API key adds runs, listings and permanence.
  Use for ANY request to share, upload, host or link a local file produced during a session.
triggers:
  # Direct invocations
  - krowk
  - /krowk
  - krowk push
  - krowk runs
  - krowk claim
  - krowk uploads
  - krowk doctor
  # What the person actually asks for
  - push a screenshot
  - upload artifact
  - upload a screenshot
  - share this screenshot
  - share agent output
  - get a permalink
  - permalink for this file
  - link to this file
  - host this image
  - attach this to the PR
  - put this in the PR comment
  - show me the screenshot in Slack
  - paste this into Linear
  - group these uploads
  - take this down
  - delete the upload
  # Domains
  - krowk.com
  - cdn.krowk.com
invocable: true
argument-hint: "[push <file> | runs start | claim <artifact> <token> | doctor]"
---

# /krowk — permalinks for agent output

krowk turns a local file into a URL that unfurls in GitHub, Slack, Basecamp and
Linear. One file, one artifact, one link. That link is a card page —
`krowk.com/a/{slug}` — carrying the file and its run metadata; the bytes on the
CDN are `file_url`, which only an image embed should ever name. What groups
several links together is a **run**, not an artifact.

Twenty-two commands, one JSON envelope, and a surface you can read as data:
`krowk help --json` is the whole thing — every command, argument, flag and
environment variable — so never guess at a spelling this file does not carry.

## Agent Invariants

**MUST follow these rules.**

1. **Parse the JSON, not the prose.** Pass `--json` on every call whose result
   you intend to read. Output is human on a TTY and JSON when piped, so a
   captured stdout is usually JSON already — but "usually" is not a contract, and
   `--json` is.
2. **One envelope, every command.** Success is
   `{"ok": true, "data": …, "paste": …, "summary": …, "breadcrumbs": […]}`.
   Failure is the same shape with `"ok": false` and an `error` object. Read
   `data` for the result and `summary` for the one-line version to show a person.
3. **Follow the breadcrumbs.** `breadcrumbs[]` is the work that is left, as
   commands rather than hints: `cmd` is ready to run with this result's own slugs
   and tokens already substituted. Prefer running a breadcrumb over composing the
   same call yourself.
4. **Never paste a `<placeholder>` into a shell.** A `cmd` containing `<run>` or
   `<file>` is a value krowk genuinely does not have. Substitute it. `<` and `>`
   are redirection, so a verbatim paste runs something other than what was
   suggested. Each placeholder is explained in that breadcrumb's `description`.
5. **A claim token is a bearer secret, and it is shown once.** `claim_token`
   comes back from a keyless push and is the only authority over that artifact —
   whoever holds it can keep it, re-upload over it, or destroy it. Never write one
   into a PR comment, a commit message, a log line, a chat message or a file the
   repository tracks. Hand it to `krowk claim` and let it be spent.
6. **Keyless works right now; runs need a key.** With no key an artifact lands in
   the shared anonymous workspace, expires in 24 hours, and has no run — metadata
   flags are *not* recorded, and `data.notes` says so. Do not invent a login step
   the task did not ask for: push first, and mention the key only when the person
   wants the link to outlive the day or wants the artifacts grouped.
7. **Never paste a bare artifact link. Anywhere.** A krowk reference is the
   block the registry computed, and it comes back ready: `paste.markdown` for
   the tools that render markdown, `paste.url` for the ones that unfurl a link
   into a card of their own. `paste.destinations` maps a tool name to which of
   the two it wants, with `_default` for the tools it does not name — so pick by
   destination rather than by a rule of your own. Where you know the
   destination, let krowk pick: `krowk push shot.png --destination github`
   prints exactly what goes in the comment. Never assemble a form yourself.
   A bare `url` in a comment is a link nobody can tell anything about; the block
   says what the file is, shows it where it can be shown, and clicks through to
   the card page with the run metadata on it.
8. **Say what the file shows, on the file.** `--caption "Cart before the fix"`
   records the caption on the artifact, and every paste of it says so from then
   on — including the ones made by somebody else, later, from the card page.
   Without one the block falls back to the filename, which tells a reader
   nothing. Caption every push a person will read. `--title` is a different
   thing: it names the work and lands on the run.
9. **Pasting into something durable? Claim it first.** An unclaimed artifact
   expires within the day, so an inline image in a pull request comment, an
   issue, a design doc or anything else that outlives the session becomes a
   broken image in a comment nobody can edit. The block labels the expiry, which
   is honest, not a fix. Before pasting a keyless push anywhere durable, tell
   the person what it costs — one `krowk claim` with the token from `data`, and
   a key — and let them decide. Chat is the exception: a Slack message about
   today's work outlives nothing.
10. **Paste the link where a slug is asked for.** Every command that names a
   record — `uploads show`, `uploads attach`, `uploads delete`, `claim`,
   `runs show`, `runs finish`, and `--run` — reads the slug out of any link that
   carries it. Hand it over whole: never cut a slug out of a URL yourself, and
   never ask a person for "just the slug". A link carrying no slug of the kind
   the command wants, or two different ones, fails as `bad_artifact` or
   `bad_run` (exit 1) before anything is sent.
11. **Push what the person asked for, not what is nearby.** Judge every push as
   if it were public: `--private` narrows the audience, not the hazard, and
   whatever instruction chose the path could choose the flag too. Never push
   `.env` files, key material, credential JSON or anything under `.ssh`/`.aws`,
   and never push a file you found rather than made.
12. **`--private` is asked for, never assumed — and it is not a secret store.**
   A private artifact's card page answers everyone outside the workspace the way
   a slug that was never minted does, and only a signed-in member sees it. Its
   image still embeds anywhere, because the byte URL is itself the
   authorization: `paste.markdown` shows the picture in a PR comment while the
   link beside it opens for nobody else and unfurls nowhere — so never promise a
   person a preview of one. It needs an API key and is refused without one
   rather than quietly published. Every artifact reports its own `visibility`;
   read that rather than assuming.

## Quick Reference

| Task | Command |
|------|---------|
| Upload one file, get a link | `krowk push shot.png --json` |
| Upload where only this workspace can read it | `krowk push shot.png --private --json` |
| Upload several, one run | `krowk push a.png b.log --json` |
| Upload with PR context | `krowk push shot.png --pull-request https://github.com/acme/app/pull/12 --json` |
| Link the issue this work is about | `krowk push shot.png --link https://linear.app/acme/issue/ENG-9 --link-title "Cart total rounds wrong" --link-rel fixes --json` |
| Say what the file shows | `krowk push shot.png --caption "Cart after the fix" --json` |
| Caption a before/after pair | `krowk push before.png after.png --caption "Cart before" --caption "Cart after" --json` |
| Title the work, on the run | `krowk push shot.png --title "Checkout — mobile" --json` |
| Record an extra metadata key | `krowk push shot.png --metadata url.full="https://app.example.com/cart" --json` |
| Exactly what one tool wants pasted | `krowk push shot.png --destination github` |
| The same, for a tool that unfurls | `krowk push shot.png --destination slack` |
| Markdown block, whatever the tool | `krowk push shot.png --format markdown` |
| Bare URL, whatever the tool | `krowk push shot.png --format url` |
| Open a run to group under | `krowk runs start --session "$SESSION" --json` |
| Push into an open run | `krowk push shot.png --run run_8Kd2wq --json` |
| Close a run | `krowk runs finish run_8Kd2wq --json` |
| What a run produced | `krowk runs show run_8Kd2wq --json` |
| Recent runs | `krowk runs list --limit 10 --json` |
| Recent artifacts | `krowk uploads list --limit 10 --json` |
| One run's artifacts | `krowk uploads list --run run_8Kd2wq --json` |
| Next page of a listing | `krowk uploads list --before <next> --json` |
| Read one artifact back | `krowk uploads show art_2e1d --json` |
| Read one back from a pasted link | `krowk uploads show https://krowk.com/a/art_6jqi53mmiey1tuxpdo7bbknq --json` |
| Keep an anonymous artifact | `krowk claim art_2e1d <claim-token> --json` |
| Keep it and group it at once | `krowk claim art_2e1d <claim-token> --run run_8Kd2wq --json` |
| Group an artifact afterwards | `krowk uploads attach art_2e1d --run run_8Kd2wq --json` |
| Take an artifact down | `krowk uploads delete art_2e1d --json` |
| Take down a keyless artifact | `krowk uploads delete art_2e1d <claim-token> --json` |
| Store an API key you were handed | `krowk login --token krowk_sk_… --json` |
| Sign in when there is no key to paste | `krowk login --no-browser --json` |
| Which key is this, whose workspace | `krowk whoami --json` |
| Which workspaces have a stored key | `krowk workspaces list --json` |
| Switch the machine-wide default | `krowk workspaces use ws_9hj3kd8a --json` |
| Pin this repository to a workspace | `krowk config set workspace ws_9hj3kd8a --json` |
| What resolves here, and why | `krowk config show --json` |
| One command in another workspace | `krowk push shot.png --workspace ws_9hj3kd8a --json` |
| Diagnose a failure | `krowk doctor --json` |
| The whole surface, as data | `krowk help --json` |
| One command's surface | `krowk help uploads attach --json` |
| One field out of a result | `krowk push shot.png --jq '.data.artifacts[0].url'` |
| Every slug in a listing | `krowk uploads list --jq '.data.artifacts[].slug'` |

Global flags on every command: `--json` (or `--format json`), `--format`
(`human`, `json`, `markdown`, `url`), `--quiet` (the raw record, no envelope and
therefore no breadcrumbs), `--jq`, `--workspace`, `--dev`, `--help`,
`--version`.

`--destination` is a push flag rather than a global one, and it prints a paste
form rather than the envelope — so it cannot be combined with `--json`, `--jq`
or `--format`, and krowk refuses the combination as `bad_flag` rather than
picking one. When you need both the envelope and the exact form, take the
envelope: `paste.markdown`, `paste.url` and `paste.destinations` are all in it.

### Filtering with --jq

`--jq '<expression>'` runs a jq expression over the JSON inside krowk — the jq
binary does not have to be installed, and the pipe through it is not needed. It
implies `--json`, and it reads whatever the command rendered: the envelope
normally, the bare record under `--quiet`. A string result prints without its
quotes, so it drops straight into a shell variable; anything else prints as
JSON, one value per line.

```bash
URL=$(krowk push shot.png --jq '.data.artifacts[0].url')
krowk uploads list --limit 10 --jq '[.data.artifacts[] | {slug, filename}]'
krowk help --json --jq '.commands[].name'
```

A failure is filtered too, and it is filtered in whatever shape the command
rendered it: `--jq '.error.error'` reads the code out of an envelope, and
`--quiet --jq '.error'` reads it out of the bare body. A failure `--jq` itself
caused is always reported whole. An expression that does not parse fails as
`bad_jq` before the command sends anything; one that does not fit the result
fails as `jq_failed` afterwards, and says so, because by then the command has
already done its work and running it again would repeat it. Both exit 1.

`auth token` and `--version` print no JSON and refuse `--jq`
rather than ignore it; `krowk help --json` marks them `no_json`. A `--jq` given
with nothing in it — a shell expanding an unset variable — is `bad_jq`, never
"no filter". `--jq` cannot be combined with `--format human`, `markdown` or
`url`: it reads the JSON, so one of the two would have to be discarded, and it
is refused rather than picked.

Two commands answer with a bare record and no envelope, whatever the flags:
`doctor` and `upgrade`. Filter those as `--jq '.token_source'`, not
`--jq '.data.token_source'`.

### Workspaces

A key belongs to one workspace; `krowk login` stores one key per workspace, so
logging into a second never replaces the first. Which key a command uses:
`--workspace` → `KROWK_WORKSPACE` → `.krowk/config.json` at the git root → the
global config → whichever key logged in last. A workspace is always its
`ws_…` slug.

- In a repo with `.krowk/config.json`, never pass a workspace — it is already
  pinned.
- A resolved workspace with no stored key fails with `no_key_for_workspace`
  (exit 3), never an anonymous fallback. Fix: `krowk login`, or check
  `krowk workspaces list --json`.
- Always pass values explicitly. The interactive picker behind an omitted
  value is for humans on a terminal; off a TTY the omission is an immediate
  error.

## Decision Trees

### Is there a key?

```
krowk whoami --json
├── ok:true  → keyed. Artifacts keep, group under runs, and are listable.
│              Use runs for anything producing more than one file.
└── ok:false, exit 3 → keyless. Push still works:
                 • the link is live immediately
                 • it expires in 24h unless claimed
                 • data.artifacts[].claim_token is the only way back to it
                 • no run, so --pull-request/--session/--title/--caption/
                   --link metadata is dropped and reported in data.notes — a keyless
                   block falls back to the filename
               Say this to the person; do not stop and ask them to sign up.
```

### One file or several?

```
One file, no session context   → krowk push shot.png --json          (done)
Several files, one moment      → krowk push a.png b.png --json       (one run, closed for you)
Files produced over a session  → krowk runs start --json
                                 krowk push … --run <run> --json     (repeat)
                                 krowk runs finish <run> --json
```

`push` with no `--run` and a key opens a run, attaches everything, and closes it
on the way out. `--run` names a run you opened, and leaves closing it to you.

### An artifact has a claim token and needs to survive

```
Do you now have a key?
├── no  → krowk login --token krowk_sk_… --json   (if you were handed one)
│         else krowk login --no-browser --json     (a person approves it — see below)
└── yes → does it belong under a run?
          ├── no  → krowk claim <artifact> <token> --json
          └── yes → krowk claim <artifact> <token> --run <run> --json
```

Claiming moves the artifact into the key's workspace and does **not** move the
link. A claim without `--run` leaves the artifact with no run at all — that is what
the `krowk uploads attach` breadcrumb in its result is for.

### Something must come down now

```
Pushed a secret, or the wrong file?
├── the artifact is in your workspace → krowk uploads delete <artifact> --json
└── it was pushed keylessly           → krowk uploads delete <artifact> <claim-token> --json
```

Immediate and irreversible: the bytes go at once. Withhold the key when using a
claim token — a keyless artifact sits in the anonymous workspace, so a request that
carries a key looks there instead and finds nothing. Then rotate whatever was in
the file: it was readable for as long as it was up — at a public URL, or at a
private one that is unguessable and not a permission.

## Common Workflows

### Push a screenshot into a PR comment

```bash
krowk push shot.png \
  --pull-request https://github.com/acme/storefront/pull/412 \
  --caption "Cart total after the fix" --json
```

Put `paste.markdown` in the comment body, whole. It is the krowk block: the
screenshot renders inline, the caption says what it shows, and both click
through to the card page with the run metadata on it. Never the bare
`data.artifacts[0].url` — GitHub builds no preview card for a third-party link,
so a bare link there is a blue anchor telling the reader nothing.

If the destination is known and there is nothing else to read out of the result,
skip the envelope and let krowk print the form:

```bash
krowk push shot.png --caption "Cart total after the fix" --destination github
```

A pull request comment outlives the session, and a keyless artifact does not: it
expires within the day and the embed goes with it. Check `data.artifacts[0]` for
a `claim_token` before pasting, and if there is one, say so — claiming it needs
a key and is the person's call, but a broken image in a merged PR is not
something either of you can fix later.

### A whole session's output under one run

```bash
krowk runs start --session "$CLAUDE_SESSION_ID" \
  --pull-request https://github.com/acme/storefront/pull/412 --json
# → data.slug, e.g. run_8Kd2wq

krowk push before.png --run run_8Kd2wq --json
krowk push after.png  --run run_8Kd2wq --json
krowk push build.log  --run run_8Kd2wq --json

krowk runs finish run_8Kd2wq --json
```

The run records the work — PR, session, links — once, when opened. Every
push also stamps that artifact with the state it finds *then*: commit, branch,
dirty, harness, model. So `before.png` and `after.png` pushed across a mid-run
commit carry two different revisions, which is the point. All of it is detected;
pass only what detection cannot know, and `--metadata key=value` (repeatable)
for anything else — your value beats a detected one. Metadata is public.
Afterwards, `krowk runs show run_8Kd2wq --json` lists everything the run produced.

### Sign in when there is no key to paste

**Never run this unprompted.** It blocks until a person approves it, up to a
quarter of an hour, so a login started on your own initiative is a session that
looks hung. A keyless push works now; reach for this only when the person asked
for a key, or asked for something a key is required for.

**It does not work against `api.krowk.com` yet.** The endpoints and the approval
page are not built there, so the command answers `404 no_such_endpoint` with a fix
line saying so. Against production, `--token` is the only way in. Use this flow
against a registry that serves it — the repository's dev stand-in
(`make mock` in a checkout, then `--dev`), or `api.krowk.com` once the gap
closes. With only an installed krowk there is no local registry to point at:
don't try to host one, run the flow with `--token` or not at all.

```bash
krowk login --no-browser --json
```

While it waits it writes `{"authorizing": {"code": …, "page": …}}` to **stderr** —
capture both streams, show the person both fields, and say that the code on the
page has to match the one you printed. That document carries no `ok`: it is not
the outcome. stdout stays one document, the receipt, once the key is stored; on
stderr the **last** document is the verdict.

`--no-browser` is the flag for you to pass. Over SSH or with no display it is
what happens anyway, and **on CI a login without it is refused**
(`no_one_to_approve`, exit 3) — passing it is how you say there is a person to
hand the code to. Drop it only where the browser on this machine is that person's
own.

The key goes behind `--token`, never as a bare argument.
`krowk login krowk_sk_…` is refused with `token_not_a_positional` rather than
silently ignored.

The key itself is not yours to read. It lands in the credentials file at 0600 and
every later command finds it there. Two things in the receipt are worth reading
rather than repeating:

- `shadowed_by_env` — `KROWK_TOKEN` is set and outranks what was just stored, so
  uploads use that key instead. Say so rather than reporting the workspace the
  receipt names.
- `confirmed: false` with a `reason` — the key is stored and works, but the
  registry did not say which key it is or where it acts. Settle it with
  `krowk whoami --json` before telling the person where their uploads land.

### Keep an artifact that was pushed before signing in

```bash
krowk push diagram.png --json
# → data.artifacts[0].claim_token — treat as a secret, never echo it

krowk login --token krowk_sk_… --json
krowk claim art_2e1d "$CLAIM_TOKEN" --run run_8Kd2wq --json
```

The token is spent by the claim and is worthless afterwards, which is the whole
reason it can be handed over: one shot, then gone. The link does not change, so
anything already pasted keeps working.

### Take down something that should never have been pushed

```bash
krowk uploads delete art_2e1d --json                    # yours
krowk uploads delete art_2e1d "$CLAIM_TOKEN" --json     # pushed keylessly
```

Do this before anything else — before reporting it, before cleaning up the
repository. Then tell the person exactly what was exposed, and rotate it.

## Environment

| Variable | Does |
|----------|------|
| `KROWK_TOKEN` | API key, e.g. `krowk_sk_…`. Outranks the stored credentials file |
| `KROWK_API_URL` | Base URL of the registry. Default `https://api.krowk.com/v1` |
| `KROWK_WORKSPACE` | Workspace whose stored key to use, as if by `--workspace` |
| `KROWK_AGENT` | Harness name recorded as `krowk.harness`. Detected otherwise |
| `KROWK_SESSION` | Session ID recorded as `krowk.session`. Detected otherwise |
| `KROWK_MODEL` | Model recorded as `gen_ai.request.model`. `ANTHROPIC_MODEL` also read |
| `KROWK_DEV` | `1`/`true`/`yes`/`on` — same as `--dev`, a local registry |
| `KROWK_HOME` | Where krowk keeps its files, as an absolute path. Default `~/.krowk` |

A stored key lives at `~/.krowk/credentials.json`, mode 0600, written by
`krowk login`, in the `0700` directory where krowk keeps everything else too.
Never read it or anything else in `~/.krowk` out to show a person, never
push it — `krowk_push` refuses — and never put a token on a command line in
a shared shell: `KROWK_TOKEN` in the environment is better.

## Error Handling

**Read `error.fix` first.** Every failure carries it, and it names the one thing
to change. Repeating the same call after a non-retryable error gets the same
answer.

```json
{"ok": false,
 "error": {"error": "run_needs_key", "status": 401, "retryable": false,
           "fix": "krowk login --token krowk_sk_…, then push again"}}
```

- `error.retryable` says whether trying again can work at all. When it is false,
  change the command.
- **429:** `error.retry_after` carries the registry's own Retry-After in seconds
  when it sent one. Wait that long — not less — then retry once. Do not open a
  parallel push to get around it.
- **Anything unexplained:** `krowk doctor --json` reports the version, whether
  the registry is reachable, whether a key is configured and what run context was
  detected. Run it before reporting a bug, and include its output.
- **A link that 404s or is gone:** the artifact expired (keyless, 24 hours) or was
  taken down. Exit 8. No retry brings it back; push again.
- **Exit 8 on a login:** a browser login lapsed unapproved (`authorization_expired`)
  or its key was already collected (`spent`). Same rule, different fix — nothing
  brings that login back, so run `krowk login` again for a new one.
- **`krowk help <command> --json`** settles any question about a flag, and cannot
  be out of date — it is generated from the same catalog that routes the command.

## Exit Codes

| Exit | Means | What to do |
|------|-------|------------|
| 0 | It worked | — |
| 1 | The command was wrong, or krowk failed on its own — bad flag, unknown command, unreadable file. Also anything unclassified | Fix the command; `krowk help <command> --json` |
| 2 | Not found — no such artifact or run in this workspace, or an unrecognised claim token | Check the slug and the token, or `KROWK_API_URL` |
| 3 | Refused for want of credentials — no key where one is needed, a rejected key, a browser login somebody denied or that CI could not approve, or no claim token where that is the only authority | `krowk login --token …`, or pass the claim token |
| 4 | Understood and refused — validation, an artifact already finalized, a run that needs a key | Change something; retrying unchanged answers the same |
| 5 | Rate limited | Wait `error.retry_after` seconds, then retry |
| 6 | The bytes did not move — registry or object storage unreachable | Retry; `krowk doctor --json` |
| 7 | The registry failed on its side, answered something this client could not read, or named a login page krowk will not open | Retry, and report it if it persists; for the login page, check `KROWK_API_URL` |
| 8 | Gone — an artifact expired or was taken down, or a browser login lapsed or had its key collected already | Push again, or log in again; nothing brings the old one back |

1 stays the catch-all, so a check for `!= 0` keeps working and a failure that
gains a class can only move *out* of 1.

## Learn More

- CLI repo, wire contract and the MCP server: https://github.com/krowkcom/krowk
- The surface as data, always current: `krowk help --json`
