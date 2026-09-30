# Performance and size budgets

Every performance and size number krowk promises lives in one file,
[`budgets.toml`](budgets.toml), beside this one. `make bench` builds the two
release binaries — the agent build (no features) and the full build
(`--features harness`) — measures each enforced budget, prints a table, and
fails if any is broken. CI runs the same `make bench` before every release
(`.github/workflows/bench.yml`, which `release.yml` waits on) and puts the same
table in the job summary. A branch that moves a number runs it by hand with
`gh workflow run bench.yml --ref <branch>`. A broken budget fails the job with a
line naming it:

```text
budget lean.size (R-PKG-2) broken: Agent build (no features): release binary size — 5.12 MiB is over its budget of 4.00 MiB
```

## How the numbers are read

- **Against the absolute number, never the last run.** A slow drift fails as
  surely as a jump, and a noisy run cannot raise the bar for the next one.
- **The median of repeated runs.** Startup is timed over 21 fresh processes
  (after one discarded run that creates the empty store and warms the page
  cache); the log append over 2,000 events.
- **One pinned runner class.** CI measures on `ubuntu-24.04`, named by version
  so an image upgrade cannot move the numbers unannounced, and passes
  `--strict`: there, a budget that cannot be measured fails instead of being
  skipped.
- **Sizes are per target.** A binary's size is its target's own, so size
  budgets are a table keyed by target (`x86_64-linux`, `aarch64-macos`, …).
  Only the pinned runner's target has a number; `make bench` elsewhere prints
  its size and marks it skipped.
- **Idle is read from Linux /proc.** `krowk -p` is started against a local
  provider that takes the request and never answers, and once the request has
  arrived its CPU ticks (`/proc/<pid>/stat`) and context switches (every
  thread's `/proc/<pid>/task/*/status`) are read, then read again 10 seconds
  later. Both must not move. On macOS these are skipped.

## Changing a number

A number in `budgets.toml` is a promise, not a record. Raising (or lowering)
one takes a line of justification in the pull request that changes it: what
got bigger or slower, and why that is worth it. A new dependency in the full
build names its cost against `full.size` in its `Cargo.toml` comment and in
the PR.

## Turning a pending budget on

A budget whose thing does not exist yet is `status = "pending"` with an
`owner`, the ticket that builds it; the table shows it as `pending — ticket
NN`. The ticket that builds it turns it on in the same pull request:

1. Write its measurement in `src/measure.rs` (or `src/idle.rs` for anything
   read from /proc), running the built binary the way a person would, and
   returning the median of its runs in the budget's unit.
2. Add its `id` to the match in `src/main.rs`. An enforced budget with no
   measurement fails the run, so the two cannot drift apart.
3. Change its `status` to `"enforced"` and drop `owner`. Leave `max` alone:
   it is the spec's number.
4. Run `make bench` and put the table in the PR.

From then on it enforces, on every release.
