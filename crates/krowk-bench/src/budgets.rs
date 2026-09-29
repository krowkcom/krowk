//! budgets.toml, and what a set of measurements makes of it: a row per
//! budget, a table, and the list of budgets broken.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Deserialize)]
pub struct File {
    pub budget: Vec<Budget>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub id: String,
    pub req: String,
    pub what: String,
    pub unit: Unit,
    pub max: Max,
    #[serde(default)]
    pub runs: Option<usize>,
    #[serde(default)]
    pub window_s: Option<u64>,
    pub status: Status,
    /// The ticket that turns a pending budget on.
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Enforced,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Unit {
    #[serde(rename = "bytes")]
    Bytes,
    #[serde(rename = "count")]
    Count,
    #[serde(rename = "ms")]
    Ms,
    #[serde(rename = "us")]
    Us,
    #[serde(rename = "ticks")]
    Ticks,
    #[serde(rename = "wakeups")]
    Wakeups,
    #[serde(rename = "MB")]
    Mb,
    #[serde(rename = "fps")]
    Fps,
    #[serde(rename = "tokens")]
    Tokens,
}

/// One number for every target, or one per target where the number is the
/// target's own — a binary's size is.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Max {
    All(f64),
    PerTarget(BTreeMap<String, f64>),
}

impl Max {
    pub fn for_target(&self, target: &str) -> Option<f64> {
        match self {
            Max::All(n) => Some(*n),
            Max::PerTarget(m) => m.get(target).copied(),
        }
    }
}

pub fn parse(text: &str) -> Result<File, String> {
    let f: File = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = std::collections::HashSet::new();
    for b in &f.budget {
        if !seen.insert(b.id.as_str()) {
            return Err(format!("budget {} is listed twice", b.id));
        }
        if b.status == Status::Pending && b.owner.is_none() {
            return Err(format!("pending budget {} names no owner ticket to turn it on", b.id));
        }
    }
    // The idle budgets are read off one idle process, so they share one
    // window; a row printing a window it was not measured over would lie.
    let windows: std::collections::BTreeSet<Option<u64>> = f.budget.iter().filter(|b| b.id.starts_with("engine.idle_")).map(|b| b.window_s).collect();
    if windows.len() > 1 {
        return Err(format!("the engine.idle_* budgets share one idle process, so they need one window_s (found {windows:?})"));
    }
    Ok(f)
}

/// The host as budgets.toml names targets: `x86_64-linux`, `aarch64-macos`.
pub fn host_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// What measuring one budget came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The median of the runs, in the budget's unit, and a note (the spread,
    /// what was measured) for the table.
    Measured { value: f64, note: String },
    /// Not measurable here: a Linux-only check elsewhere, say.
    Skipped(String),
    /// The measurement itself failed to run.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Pending,
    Skipped,
}

pub struct Row {
    pub budget: Budget,
    pub max: Option<f64>,
    pub outcome: Option<Outcome>,
    pub verdict: Verdict,
    /// Why, for anything but a pass.
    pub reason: String,
}

/// Holds each measurement to its budget. `strict` is the pinned runner's
/// mode: there, a budget that could not be measured fails too, because a
/// skip on the one machine that holds the numbers would be a budget nobody
/// holds.
pub fn judge(budget: &Budget, target: &str, outcome: Option<Outcome>, strict: bool) -> Row {
    let max = budget.max.for_target(target);
    let (verdict, reason) = match (&outcome, max) {
        (None, _) => (Verdict::Pending, format!("pending — ticket {}", budget.owner.as_deref().unwrap_or("?"))),
        (Some(Outcome::Error(e)), _) => (Verdict::Fail, format!("could not be measured: {e}")),
        (Some(Outcome::Skipped(why)), _) => (if strict { Verdict::Fail } else { Verdict::Skipped }, format!("skipped: {why}")),
        (Some(Outcome::Measured { .. }), None) => (
            if strict { Verdict::Fail } else { Verdict::Skipped },
            format!("no budget for {target} in budgets.toml — the pinned runner holds this one"),
        ),
        (Some(Outcome::Measured { value, .. }), Some(max)) => {
            if *value <= max {
                (Verdict::Pass, String::new())
            } else {
                (Verdict::Fail, format!("{} is over its budget of {}", show(budget.unit, *value), show(budget.unit, max)))
            }
        }
    };
    Row { budget: budget.clone(), max, outcome, verdict, reason }
}

/// The median: the middle run, so one slow run on a busy machine moves
/// nothing.
pub fn median(xs: &mut [f64]) -> f64 {
    assert!(!xs.is_empty(), "a median of no runs");
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n % 2 == 1 { xs[n / 2] } else { (xs[n / 2 - 1] + xs[n / 2]) / 2.0 }
}

pub fn show(unit: Unit, v: f64) -> String {
    match unit {
        Unit::Bytes => format!("{:.2} MiB", v / (1024.0 * 1024.0)),
        Unit::Count => format!("{v:.0}"),
        Unit::Ms => format!("{v:.1} ms"),
        Unit::Us => format!("{v:.1} µs"),
        Unit::Ticks => format!("{v:.0} ticks"),
        Unit::Wakeups => format!("{v:.0} wakeups"),
        Unit::Mb => format!("{v:.1} MB"),
        Unit::Fps => format!("{v:.0} fps"),
        Unit::Tokens => format!("{v:.0} est. tokens"),
    }
}

/// The table `make bench` prints and CI writes to the job summary: the same
/// bytes in both places.
pub fn table(rows: &[Row], target: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "### Performance and size budgets ({target})\n");
    let _ = writeln!(out, "| Budget | Req | What | Measured | Max | Result |");
    let _ = writeln!(out, "|---|---|---|---|---|---|");
    for r in rows {
        let b = &r.budget;
        let measured = match &r.outcome {
            Some(Outcome::Measured { value, note }) if note.is_empty() => show(b.unit, *value),
            Some(Outcome::Measured { value, note }) => format!("{} ({note})", show(b.unit, *value)),
            _ => "—".into(),
        };
        let max = r.max.map_or_else(|| "—".into(), |m| show(b.unit, m));
        let result = match r.verdict {
            Verdict::Pass => "pass".to_string(),
            Verdict::Fail => format!("**FAIL**: {}", r.reason),
            Verdict::Pending | Verdict::Skipped => r.reason.clone(),
        };
        let _ = writeln!(out, "| `{}` | {} | {} | {measured} | {max} | {result} |", b.id, b.req, b.what.replace('|', "\\|"));
    }
    let count = |v| rows.iter().filter(|r| r.verdict == v).count();
    let _ = writeln!(
        out,
        "\n{} pass · {} fail · {} pending · {} skipped. Any fail fails the job (R-PERF-7).",
        count(Verdict::Pass),
        count(Verdict::Fail),
        count(Verdict::Pending),
        count(Verdict::Skipped)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(max: Max, status: Status) -> Budget {
        Budget {
            id: "lean.size".into(),
            req: "R-PKG-2".into(),
            what: "size".into(),
            unit: Unit::Bytes,
            max,
            runs: None,
            window_s: None,
            status,
            owner: Some("04".into()),
        }
    }

    fn measured(value: f64) -> Option<Outcome> {
        Some(Outcome::Measured { value, note: String::new() })
    }

    #[test]
    fn r_perf_7_a_median_over_its_budget_fails_and_names_it() {
        let b = budget(Max::PerTarget([("x86_64-linux".to_string(), 4_194_304.0)].into()), Status::Enforced);
        let row = judge(&b, "x86_64-linux", measured(5_242_880.0), false);
        assert_eq!(row.verdict, Verdict::Fail);
        assert_eq!(row.reason, "5.00 MiB is over its budget of 4.00 MiB");
        let t = table(&[row], "x86_64-linux");
        assert!(t.contains("| `lean.size` | R-PKG-2 |"), "{t}");
        assert!(t.contains("**FAIL**: 5.00 MiB is over its budget of 4.00 MiB"), "{t}");
        assert!(t.contains("0 pass · 1 fail"), "{t}");
    }

    #[test]
    fn r_perf_7_at_the_budget_passes() {
        let b = budget(Max::All(0.0), Status::Enforced);
        assert_eq!(judge(&b, "x86_64-linux", measured(0.0), false).verdict, Verdict::Pass);
        assert_eq!(judge(&b, "x86_64-linux", measured(1.0), false).verdict, Verdict::Fail);
    }

    #[test]
    fn r_perf_7_a_measurement_that_fails_to_run_fails() {
        let b = budget(Max::All(1.0), Status::Enforced);
        assert_eq!(judge(&b, "x86_64-linux", Some(Outcome::Error("no /proc".into())), false).verdict, Verdict::Fail);
    }

    #[test]
    fn r_perf_7_the_pinned_runner_fails_what_it_cannot_measure() {
        // A size budget with no number for this target: reported off the
        // pinned runner, a failure on it.
        let b = budget(Max::PerTarget([("x86_64-linux".to_string(), 1.0)].into()), Status::Enforced);
        assert_eq!(judge(&b, "aarch64-macos", measured(0.5), false).verdict, Verdict::Skipped);
        assert_eq!(judge(&b, "aarch64-macos", measured(0.5), true).verdict, Verdict::Fail);
        let skip = Some(Outcome::Skipped("needs Linux /proc".into()));
        assert_eq!(judge(&b, "x86_64-linux", skip.clone(), false).verdict, Verdict::Skipped);
        assert_eq!(judge(&b, "x86_64-linux", skip, true).verdict, Verdict::Fail);
    }

    #[test]
    fn pending_shows_as_pending_with_its_owner() {
        let b = budget(Max::All(150.0), Status::Pending);
        let row = judge(&b, "x86_64-linux", None, true);
        assert_eq!(row.verdict, Verdict::Pending);
        let t = table(&[row], "x86_64-linux");
        assert!(t.contains("| — | 0.00 MiB | pending — ticket 04 |"), "{t}");
        assert!(t.contains("0 pass · 0 fail · 1 pending"), "{t}");
    }

    #[test]
    fn the_median_ignores_one_slow_run() {
        assert_eq!(median(&mut [3.0, 1.0, 900.0, 2.0, 2.5]), 2.5);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn a_duplicate_or_ownerless_pending_budget_is_refused() {
        let one = "[[budget]]\nid = \"a\"\nreq = \"R\"\nwhat = \"w\"\nunit = \"ms\"\nmax = 1\nstatus = \"enforced\"\n";
        assert!(parse(&format!("{one}{one}")).unwrap_err().contains("listed twice"));
        let pending = "[[budget]]\nid = \"a\"\nreq = \"R\"\nwhat = \"w\"\nunit = \"ms\"\nmax = 1\nstatus = \"pending\"\n";
        assert!(parse(pending).unwrap_err().contains("no owner"));
    }

    #[test]
    fn r_perf_2_idle_budgets_with_different_windows_are_refused() {
        let row = |id: &str, w: u64| format!("[[budget]]\nid = \"{id}\"\nreq = \"R-PERF-2\"\nwhat = \"w\"\nunit = \"ticks\"\nmax = 0\nwindow_s = {w}\nstatus = \"enforced\"\n");
        assert!(parse(&format!("{}{}", row("engine.idle_cpu", 10), row("engine.idle_wakeups", 10))).is_ok());
        let e = parse(&format!("{}{}", row("engine.idle_cpu", 10), row("engine.idle_wakeups", 5))).unwrap_err();
        assert!(e.contains("one window_s"), "{e}");
    }

    /// The file in the repository: every R-PERF item the spec states a
    /// number for is in it, with that number, and R-PKG-2 is enforced.
    #[test]
    fn r_perf_budgets_file_lists_every_requirement_with_its_number() {
        let f = parse(include_str!("../budgets.toml")).expect("budgets.toml parses");
        let find = |id: &str| f.budget.iter().find(|b| b.id == id).unwrap_or_else(|| panic!("budgets.toml has no {id}"));
        let spec: &[(&str, &str, f64)] = &[
            ("tui.startup_warm", "R-PERF-1", 50.0),
            ("tui.startup_cold", "R-PERF-1", 150.0),
            ("tui.idle_cpu", "R-PERF-2", 0.0),
            ("engine.idle_cpu", "R-PERF-2", 0.0),
            ("tui.idle_rss", "R-PERF-3", 30.0),
            ("session.replay_rss", "R-PERF-3", 100.0),
            ("tui.redraw_fps", "R-PERF-4", 60.0),
            ("log.append", "R-PERF-5", 1000.0),
            ("remote.attach", "R-PERF-6", 500.0),
        ];
        for (id, req, n) in spec {
            let b = find(id);
            assert_eq!(b.req, *req, "{id}");
            assert_eq!(b.max.for_target("x86_64-linux"), Some(*n), "{id} holds the spec's number");
        }
        for req in ["R-PERF-1", "R-PERF-2", "R-PERF-3", "R-PERF-4", "R-PERF-5", "R-PERF-6", "R-PKG-2"] {
            assert!(f.budget.iter().any(|b| b.req == req), "budgets.toml has nothing for {req}");
        }
        for id in [
            "lean.size",
            "lean.deps",
            "log.append",
            "engine.idle_cpu",
            "context.tokens",
            "tui.startup_cold",
            "tui.idle_cpu",
            "tui.turn_cpu",
            "tui.idle_rss",
            "tui.redraw_fps",
            "session.replay_rss",
            "remote.attach",
        ] {
            assert_eq!(find(id).status, Status::Enforced, "{id} is measurable now, so it is enforced");
        }
        let lean = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../krowk/lean-deps.txt")).unwrap();
        assert_eq!(find("lean.deps").max.for_target("any"), Some(lean.lines().count() as f64), "lean.deps is lean-deps.txt's length");
    }
}
