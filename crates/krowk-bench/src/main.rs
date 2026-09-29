//! krowk-bench holds built krowk binaries to crates/krowk-bench/budgets.toml
//! and exits non-zero when any budget is broken (R-PERF-7). `make bench`
//! builds the two binaries and runs it; CI runs `make bench` on one pinned
//! runner, so the numbers are always read on the same class of machine.
//!
//! ```text
//! krowk-bench --budgets FILE --lean BIN --full BIN --work DIR [--strict]
//! ```
//!
//! The table goes to stdout, and to `$GITHUB_STEP_SUMMARY` when CI sets it;
//! each broken budget is also named on stderr. `--strict` is the pinned
//! runner's mode: a budget that cannot be measured there fails rather than
//! being skipped.

mod budgets;
#[cfg(target_os = "linux")]
mod idle;
mod measure;
#[cfg(target_os = "linux")]
mod tui;

use budgets::{Outcome, Status, Verdict};
#[cfg(target_os = "linux")]
use idle::engine_idle;
use measure::Idle;
use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;

struct Args {
    budgets: PathBuf,
    lean: PathBuf,
    full: PathBuf,
    work: PathBuf,
    strict: bool,
}

fn args() -> Args {
    let mut a = std::env::args().skip(1);
    let (mut budgets, mut lean, mut full, mut work, mut strict) = (None, None, None, None, false);
    while let Some(flag) = a.next() {
        let mut value = || a.next().map(PathBuf::from).unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--budgets" => budgets = Some(value()),
            "--lean" => lean = Some(value()),
            "--full" => full = Some(value()),
            "--work" => work = Some(value()),
            "--strict" => strict = true,
            _ => usage(&format!("unknown argument {flag}")),
        }
    }
    let need = |v: Option<PathBuf>, name: &str| v.unwrap_or_else(|| usage(&format!("{name} is required")));
    Args { budgets: need(budgets, "--budgets"), lean: need(lean, "--lean"), full: need(full, "--full"), work: need(work, "--work"), strict }
}

fn usage(why: &str) -> ! {
    eprintln!("krowk-bench: {why}\nusage: krowk-bench --budgets FILE --lean BIN --full BIN --work DIR [--strict]");
    exit(2)
}

fn main() {
    let args = args();
    let text = std::fs::read_to_string(&args.budgets).unwrap_or_else(|e| usage(&format!("{}: {e}", args.budgets.display())));
    let file = budgets::parse(&text).unwrap_or_else(|e| {
        eprintln!("krowk-bench: {}: {e}", args.budgets.display());
        exit(2)
    });
    if let Err(e) = std::fs::create_dir_all(&args.work) {
        usage(&format!("{}: {e}", args.work.display()));
    }
    // Relative paths would move under the measured processes, which run in
    // their own directories.
    let abs = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
    let (lean, full, work) = (abs(&args.lean), abs(&args.full), abs(&args.work));

    let target = budgets::host_target();
    // The three idle budgets are read off one idle process, measured once;
    // the TUI's two off another.
    let mut idle: Option<Result<Idle, String>> = None;
    let mut tui_idle: Option<Result<Idle, String>> = None;
    let mut rows = Vec::new();
    for b in &file.budget {
        let outcome = match b.status {
            Status::Pending => None,
            Status::Enforced => {
                eprintln!("measuring {} …", b.id);
                let runs = b.runs.unwrap_or(21);
                Some(match b.id.as_str() {
                    "lean.size" => measure::size(&lean),
                    "lean.deps" => measure::lean_deps(),
                    "full.size" => measure::size(&full),
                    "startup.version" => measure::startup(&lean, &["--version"], runs, &measure::fresh_dir(&work, "startup-version")),
                    "startup.sessions" => measure::startup(&full, &["sessions"], runs, &measure::fresh_dir(&work, "startup-sessions")),
                    "log.append" => measure::log_append(&measure::fresh_dir(&work, "log"), runs),
                    #[cfg(unix)]
                    "remote.attach" => measure::remote_attach(runs),
                    "context.tokens" => measure::context_tokens(&full, &measure::fresh_dir(&work, "context-tokens")),
                    "engine.idle_cpu" | "engine.idle_wakeups" | "engine.idle_rss" => {
                        let window = Duration::from_secs(b.window_s.unwrap_or(10));
                        let sample = idle.get_or_insert_with(|| engine_idle(&full, &measure::fresh_dir(&work, "engine-idle"), window));
                        let note = format!("{} s window", window.as_secs());
                        match sample {
                            Ok(s) if b.id == "engine.idle_cpu" => Outcome::Measured { value: s.ticks as f64, note },
                            Ok(s) if b.id == "engine.idle_wakeups" => Outcome::Measured { value: s.wakeups as f64, note },
                            Ok(s) => Outcome::Measured { value: s.rss_mb, note: String::new() },
                            Err(e) if e == NOT_LINUX => Outcome::Skipped(e.clone()),
                            Err(e) => Outcome::Error(e.clone()),
                        }
                    }
                    "tui.startup_cold" | "tui.startup_warm" | "tui.redraw_fps" | "session.replay_rss" => tui_measure(&b.id, &full, &measure::fresh_dir(&work, &b.id), runs),
                    "tui.turn_cpu" => tui_turn_cpu(&full, &measure::fresh_dir(&work, "tui-turn"), Duration::from_secs(b.window_s.unwrap_or(5))),
                    "tui.idle_cpu" | "tui.idle_rss" => {
                        let window = Duration::from_secs(b.window_s.unwrap_or(10));
                        let sample = tui_idle.get_or_insert_with(|| tui_idle_sample(&full, &measure::fresh_dir(&work, "tui-idle"), window));
                        match sample {
                            // Wakeups have no budget of their own for the
                            // TUI yet; they are printed beside the ticks.
                            Ok(s) if b.id == "tui.idle_cpu" => Outcome::Measured { value: s.ticks as f64, note: format!("{} s window, {} wakeups", window.as_secs(), s.wakeups) },
                            Ok(s) => Outcome::Measured { value: s.rss_mb, note: String::new() },
                            Err(e) if e == NOT_LINUX => Outcome::Skipped(e.clone()),
                            Err(e) => Outcome::Error(e.clone()),
                        }
                    }
                    // Enforced with nothing to measure it: turning a budget
                    // on means writing its measurement in the same change.
                    other => Outcome::Error(format!("krowk-bench has no measurement for {other}")),
                })
            }
        };
        rows.push(budgets::judge(b, &target, outcome, args.strict));
    }

    let table = budgets::table(&rows, &target);
    println!("\n{table}");
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        let appended = std::fs::OpenOptions::new().append(true).create(true).open(&summary).and_then(|mut f| f.write_all(table.as_bytes()));
        if let Err(e) = appended {
            eprintln!("krowk-bench: could not write the job summary: {e}");
        }
    }
    let failed: Vec<_> = rows.iter().filter(|r| r.verdict == Verdict::Fail).collect();
    let annotate = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
    for r in &failed {
        let line = format!("budget {} ({}) broken: {} — {}", r.budget.id, r.budget.req, r.budget.what, r.reason);
        if annotate {
            eprintln!("::error title=budget {} broken::{line}", r.budget.id);
        } else {
            eprintln!("{line}");
        }
    }
    if !failed.is_empty() {
        eprintln!("{} budget(s) broken. A number in {} changes only with a line of justification in the PR.", failed.len(), args.budgets.display());
        exit(1);
    }
}

/// The TUI's timings and peak memory, run on a pseudo-terminal. Linux only
/// for now: the replay's peak is read from /proc, and one runner class
/// holds all of them.
#[cfg(target_os = "linux")]
fn tui_measure(id: &str, full: &std::path::Path, home: &std::path::Path, runs: usize) -> Outcome {
    match id {
        "tui.startup_cold" => tui::startup(full, home, runs, false),
        "tui.startup_warm" => tui::startup(full, home, runs, true),
        "tui.redraw_fps" => tui::redraw(full, home),
        _ => tui::replay_rss(full, home),
    }
}

#[cfg(not(target_os = "linux"))]
fn tui_measure(_: &str, _: &std::path::Path, _: &std::path::Path, _: usize) -> Outcome {
    Outcome::Skipped(NOT_LINUX.into())
}

#[cfg(target_os = "linux")]
fn tui_turn_cpu(bin: &std::path::Path, home: &std::path::Path, window: Duration) -> Outcome {
    tui::turn_cpu(bin, home, window)
}

#[cfg(not(target_os = "linux"))]
fn tui_turn_cpu(_: &std::path::Path, _: &std::path::Path, _: Duration) -> Outcome {
    Outcome::Skipped(NOT_LINUX.into())
}

#[cfg(target_os = "linux")]
fn tui_idle_sample(bin: &std::path::Path, home: &std::path::Path, window: Duration) -> Result<Idle, String> {
    tui::idle(bin, home, window)
}

#[cfg(not(target_os = "linux"))]
fn tui_idle_sample(_: &std::path::Path, _: &std::path::Path, _: Duration) -> Result<Idle, String> {
    Err(NOT_LINUX.into())
}

#[cfg(not(target_os = "linux"))]
fn engine_idle(_: &std::path::Path, _: &std::path::Path, _: Duration) -> Result<Idle, String> {
    Err(NOT_LINUX.into())
}

const NOT_LINUX: &str = "needs Linux /proc; the pinned runner measures it";
