//! WT16: the worktree wave measured end to end, with no model, on a scratch
//! clone of a repository. `scripts/bench-worktrees` makes the clone and
//! runs this:
//!
//! ```text
//! bench_worktrees <clone> <worktrees root> [count]
//! ```
//!
//! 1. One worktree made and finished first, timed on its own line: a
//!    repository's first creation builds its template (WT10), once.
//! 2. `count` (20) made at once through the creation path,
//!    `create_fastest` then the prepare steps, each timed from the moment
//!    all of them start, so a wait for the repository's lock counts.
//! 3. `cargo check` in all of them at once, each behind a slot of the pool
//!    the bash tool queues builds behind (`Builds::resolve`, its default
//!    size) with the `CARGO_BUILD_JOBS` bash would give it, the machine's
//!    used memory sampled every 100 ms.
//! 4. Each finished (`finish`): unchanged, so removed with its branch.
//! 5. Three subagents' worktrees: one adds a file, two change the same
//!    line. Applied back to the clone by `finish_into` in turn: the first
//!    two land, the third is reported as a conflict, then applied by
//!    `krowk worktrees apply`'s path (`manage::apply`) once the clone's
//!    line is put back.
//! 6. What is left: `git worktree list`, `krowk/` branches, and worktrees
//!    under the root.
//!
//! Exits 1 when any step failed or anything is left.

use krowk_harness::builds::{Builds, BuildsConfig};
use krowk_harness::instances::WorktreesConfig;
use krowk_harness::worktree::apply::Applied;
use krowk_harness::worktree::{self, Finished, Made, Prepare, Worktree, manage};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The session the worktrees are locked for.
const OWNER: &str = "bench-worktrees";
/// How often the machine's used memory is read.
const SAMPLE: Duration = Duration::from_millis(100);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(repo), Some(root)) = (args.first(), args.get(1)) else {
        eprintln!("usage: bench_worktrees <clone> <worktrees root> [count]");
        std::process::exit(2);
    };
    let count: usize = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(20);
    let (repo, root) = (PathBuf::from(repo), PathBuf::from(root));
    let config = WorktreesConfig::default();
    let mut failed = Vec::new();

    let t = Instant::now();
    match create(&repo, &root, &config) {
        Ok((wt, _held, made)) => {
            let finished = worktree::finish(&wt);
            println!("first creation, the template built: {} ({made:?}, {finished:?})", secs(t.elapsed()));
        }
        Err(e) => failed.push(format!("first creation: {e}")),
    }

    let made = create_all(&repo, &root, &config, count);
    let mut times = Vec::new();
    let mut methods = Vec::new();
    let mut live = Vec::new();
    for m in made {
        match m {
            Ok((wt, held, how, took)) => {
                times.push(took);
                methods.push(how);
                live.push((wt, held));
            }
            Err(e) => failed.push(format!("creation: {e}")),
        }
    }
    println!("{} of {count} made at once", live.len());

    let builds = Builds::resolve(&BuildsConfig::default(), &|k| std::env::var(k).unwrap_or_default());
    let paths: Vec<PathBuf> = live.iter().map(|(wt, _)| wt.path.clone()).collect();
    let checked = check_all(&paths, &builds);
    failed.extend(checked.failed.iter().cloned());

    for (wt, held) in live {
        match worktree::finish(&wt) {
            Ok(Finished::Removed) => {}
            other => failed.push(format!("finish {}: {other:?}", wt.hex)),
        }
        drop(held);
    }

    match subagents(&repo, &root, &config) {
        Ok(line) => println!("{line}"),
        Err(e) => failed.push(format!("subagents: {e}")),
    }

    let left = leftovers(&repo, &root);
    if let Ok(kept) = git(&repo, &["for-each-ref", "--format=%(refname)", "refs/krowk/snapshots/"]) {
        println!("snapshot refs kept as the way back, as designed: {}", kept.lines().count());
    }
    print_table(&times, &methods, &checked, &builds, left.len());
    for line in failed.iter().chain(&left) {
        eprintln!("- {line}");
    }
    if !failed.is_empty() || !left.is_empty() {
        std::process::exit(1);
    }
}

/// A worktree of `repo` made as a subagent's is: `create_fastest`, then
/// the prepare steps.
fn create(repo: &Path, root: &Path, config: &WorktreesConfig) -> Result<(Worktree, manage::Held, Made), String> {
    let (wt, held, made) = worktree::create_fastest(repo, root, OWNER, config).map_err(|e| e.to_string())?;
    let mut p = Prepare::new(&wt, config);
    p.seeded = made == Made::Snapshot { seeded: true };
    for note in worktree::prepare(&p) {
        eprintln!("{}: {note}", wt.hex);
    }
    Ok((wt, held, made))
}

type Created = Result<(Worktree, manage::Held, Made, Duration), String>;

/// `count` worktrees made by as many threads let go at once, each timed.
fn create_all(repo: &Path, root: &Path, config: &WorktreesConfig, count: usize) -> Vec<Created> {
    let start = Barrier::new(count);
    std::thread::scope(|s| {
        let threads: Vec<_> = (0..count)
            .map(|_| {
                s.spawn(|| {
                    start.wait();
                    let t = Instant::now();
                    create(repo, root, config).map(|(wt, held, made)| (wt, held, made, t.elapsed()))
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap_or_else(|_| Err("the thread panicked".into()))).collect()
    })
}

/// What `check_all` measured.
struct Checked {
    wall: Duration,
    waited: Duration,
    /// The machine's memory and the most of it in use, in KiB: none where
    /// `/proc/meminfo` is not.
    total: Option<u64>,
    peak: u64,
    failed: Vec<String>,
}

/// `cargo check` in every one of `dirs` at once, behind the build slots,
/// the machine's used memory sampled meanwhile.
fn check_all(dirs: &[PathBuf], builds: &Builds) -> Checked {
    let done = AtomicBool::new(false);
    let peak = AtomicU64::new(0);
    let t = Instant::now();
    let results: Vec<Result<Duration, String>> = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                if let Some((_, used)) = memory() {
                    peak.fetch_max(used, Ordering::Relaxed);
                }
                std::thread::sleep(SAMPLE);
            }
        });
        let threads: Vec<_> = dirs.iter().map(|d| s.spawn(|| check(d, builds))).collect();
        let results = threads.into_iter().map(|t| t.join().unwrap_or_else(|_| Err("the thread panicked".into()))).collect();
        done.store(true, Ordering::Relaxed);
        results
    });
    let wall = t.elapsed();
    let waited = results.iter().filter_map(|r| r.as_ref().ok()).sum();
    let failed = results.into_iter().filter_map(Result::err).collect();
    Checked { wall, waited, total: memory().map(|(total, _)| total), peak: peak.into_inner(), failed }
}

/// `cargo check` in `dir` once a build slot is free, as the bash tool runs
/// a heavy command: how long it waited for the slot.
fn check(dir: &Path, builds: &Builds) -> Result<Duration, String> {
    let pool = builds.pool.as_ref().ok_or_else(|| "no build slots".to_string())?;
    let t = Instant::now();
    let _slot = loop {
        match pool.try_take()? {
            Some(slot) => break slot,
            None => std::thread::sleep(krowk_harness::slots::RETRY),
        }
    };
    let waited = t.elapsed();
    let mut c = Command::new("cargo");
    c.args(["check", "--locked", "--workspace", "--quiet"]).current_dir(dir).env_remove("CARGO_TARGET_DIR");
    if let Some(jobs) = &builds.jobs {
        c.env("CARGO_BUILD_JOBS", jobs);
    }
    let out = c.output().map_err(|e| format!("cargo check in {}: {e}", dir.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(5).collect();
        return Err(format!("cargo check in {}: {}", dir.display(), tail.into_iter().rev().collect::<Vec<_>>().join(" / ")));
    }
    Ok(waited)
}

/// The machine's memory and how much of it is in use (total less
/// available), in KiB, from `/proc/meminfo`.
fn memory() -> Option<(u64, u64)> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |key: &str| info.lines().find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse::<u64>().ok());
    let total = field("MemTotal:")?;
    Some((total, total.saturating_sub(field("MemAvailable:")?)))
}

/// The file a subagent adds, and the one two of them both change.
const ADDED: &str = "bench-added.txt";
const SHARED: &str = "README.md";

/// Three subagents' worktrees applied back to `repo` (see the module
/// docs): the line that says how it went, or what went wrong.
fn subagents(repo: &Path, root: &Path, config: &WorktreesConfig) -> Result<String, String> {
    let [a, b, c] = [(); 3].map(|()| create(repo, root, config));
    let ((a, _ha, _), (b, _hb, _), (c, hc, _)) = (a?, b?, c?);
    std::fs::write(a.path.join(ADDED), "added by subagent A\n").map_err(|e| e.to_string())?;
    set_first_line(&b.path.join(SHARED), "# changed by subagent B")?;
    set_first_line(&c.path.join(SHARED), "# changed by subagent C")?;

    expect(worktree::finish_into(&a, repo), &Applied::Applied(vec![ADDED.into()]))?;
    expect(worktree::finish_into(&b, repo), &Applied::Applied(vec![SHARED.into()]))?;
    expect(worktree::finish_into(&c, repo), &Applied::Conflicts(vec![SHARED.into()]))?;
    if !repo.join(ADDED).exists() || first_line(&repo.join(SHARED))? != "# changed by subagent B" {
        return Err("the parent's working tree lacks A's or B's change".into());
    }

    // The person resolves it their way, then applies the kept one.
    git(repo, &["checkout", "--", SHARED])?;
    drop(hc);
    let (_, applied) = manage::apply(root, &c.hex, Some(repo)).map_err(|e| format!("krowk worktrees apply {}: {e:?}", c.hex))?;
    if applied != Applied::Applied(vec![SHARED.into()]) || first_line(&repo.join(SHARED))? != "# changed by subagent C" {
        return Err(format!("krowk worktrees apply {}: {applied:?}", c.hex));
    }
    Ok("subagents: A's new file and B's edit applied; C's edit of the same line reported as a conflict, then applied by `krowk worktrees apply`".into())
}

fn expect(got: Result<Option<Applied>, worktree::Error>, want: &Applied) -> Result<(), String> {
    match got {
        Ok(Some(a)) if &a == want => Ok(()),
        other => Err(format!("applied {other:?}, expected {want:?}")),
    }
}

fn first_line(file: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(text.lines().next().unwrap_or("").to_string())
}

fn set_first_line(file: &Path, line: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let rest = text.split_once('\n').map_or("", |(_, rest)| rest);
    std::fs::write(file, format!("{line}\n{rest}")).map_err(|e| format!("{}: {e}", file.display()))
}

/// What git printed in `repo`, or what it said when it failed.
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Everything a worktree leaves that should be gone: git's worktrees other
/// than the clone, `krowk/` branches, what `krowk worktrees` lists, and a
/// worktree's files under the root. Not the `refs/krowk/snapshots/<hex>`
/// an applied worktree leaves as the way back: kept on purpose, for 30
/// days (`krowk worktrees prune`).
fn leftovers(repo: &Path, root: &Path) -> Vec<String> {
    let mut left = Vec::new();
    match git(repo, &["worktree", "list", "--porcelain"]) {
        Ok(list) => left.extend(list.lines().filter_map(|l| l.strip_prefix("worktree ")).skip(1).map(|p| format!("git worktree {p}"))),
        Err(e) => left.push(e),
    }
    match git(repo, &["for-each-ref", "--format=%(refname)", "refs/heads/krowk/"]) {
        Ok(refs) => left.extend(refs.lines().map(|r| format!("branch {r}"))),
        Err(e) => left.push(e),
    }
    left.extend(manage::list(root).into_iter().map(|l| format!("listed {}", l.path.display())));
    let entries = |d: &Path| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).collect::<Vec<_>>();
    for repo_dir in entries(root) {
        let scratch = entries(&repo_dir.join(worktree::template::SCRATCH_DIR));
        let own = entries(&repo_dir).into_iter().filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(worktree_file));
        left.extend(scratch.into_iter().chain(own).map(|p| format!("file {}", p.display())));
    }
    left
}

/// A worktree's directory, record or live file: `<8 hex>`, `.json`, `.live`.
fn worktree_file(name: &str) -> bool {
    let stem = name.strip_suffix(".json").or_else(|| name.strip_suffix(".live")).unwrap_or(name);
    stem.len() == 8 && stem.bytes().all(|b| b.is_ascii_hexdigit())
}

fn print_table(times: &[Duration], methods: &[Made], checked: &Checked, builds: &Builds, left: usize) {
    let mut sorted = times.to_vec();
    sorted.sort();
    let snapshots = methods.iter().filter(|m| matches!(m, Made::Snapshot { .. })).count();
    let method = if snapshots == methods.len() { format!("snapshot ({snapshots}/{})", methods.len()) } else { format!("snapshot {snapshots}, checkout {}", methods.len() - snapshots) };
    let peak = match checked.total {
        Some(total) => format!("{} ({:.0}% of {})", gib(checked.peak), checked.peak as f64 * 100.0 / total as f64, gib(total)),
        None => "n/a".into(),
    };
    let slots = builds.pool.as_ref().map_or(0, krowk_harness::slots::Pool::size);
    let jobs = builds.jobs.as_deref().unwrap_or("-");
    println!();
    println!("| method | creation p50 | creation p95 | check wall time | peak RSS | total slot wait | leftovers |");
    println!("|---|---|---|---|---|---|---|");
    println!(
        "| {method} | {} | {} | {} ({slots} slots × {jobs} jobs) | {peak} | {} | {left} |",
        secs(percentile(&sorted, 50)),
        secs(percentile(&sorted, 95)),
        secs(checked.wall),
        secs(checked.waited)
    );
}

/// The `p`th percentile of `sorted`, by nearest rank.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[(p * sorted.len()).div_ceil(100).saturating_sub(1)]
}

fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}

fn gib(kib: u64) -> String {
    format!("{:.1} GiB", kib as f64 / (1u64 << 20) as f64)
}
