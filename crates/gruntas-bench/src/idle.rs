//! The idle checks (R-PERF-2, R-PERF-3), read from Linux /proc: a process
//! that is waiting on nothing but an event should be charged no CPU and
//! woken not once. Linux only; elsewhere the budgets are reported skipped,
//! and the pinned runner, which is Linux, holds them.

use crate::measure::{Idle, sandboxed};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// `gruntas -p` with a turn in flight on a Claude Code account whose
/// `claude` — the fake the harness's tests use — has taken the prompt and
/// never answers: gruntas waiting on its backend, the most idle a turn gets.
/// Sampled from /proc once the prompt has reached the fake, then again
/// after `window`; anything gruntas did in between is polling. gruntas's own
/// process is measured, not the fake's.
pub fn engine_idle(bin: &Path, home: &Path, window: Duration) -> Result<Idle, String> {
    std::fs::create_dir_all(home.join(".gruntas")).map_err(|e| format!("{}: {e}", home.display()))?;
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/claude/fake-claude");
    let (account, scenario, log) = (home.join("claude"), home.join("silent.jsonl"), home.join("fake.log"));
    std::fs::create_dir_all(&account).map_err(|e| e.to_string())?;
    std::fs::write(account.join("fake-login"), "").map_err(|e| e.to_string())?;
    std::fs::write(&scenario, "@await-interrupt\n").map_err(|e| e.to_string())?;
    let config = serde_json::json!({
        "defaultModel": "claude/sonnet",
        "instances": {"claude": {"kind": "claude-code", "binary": fake, "configDir": account, "env": {"FAKE_CLAUDE_SCENARIO": scenario, "FAKE_CLAUDE_LOG": log}}},
    });
    std::fs::write(home.join(".gruntas/config.json"), config.to_string()).map_err(|e| e.to_string())?;
    let stderr = home.join("stderr");
    let mut child = sandboxed(bin, home)
        .args(["-p", "say hello", "--trust"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let pid = child.id();
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if std::fs::read_to_string(&log).is_ok_and(|l| l.lines().any(|l| l.starts_with(r#"in {"type":"user""#))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            if let Ok(Some(st)) = child.try_wait() {
                return Err(format!("`gruntas -p` exited {st} before its prompt reached claude: {}", std::fs::read_to_string(&stderr).unwrap_or_default().trim()));
            }
            if Instant::now() > deadline {
                return Err("`gruntas -p` sent claude no prompt within 30 s".into());
            }
        }
        // The prompt is out. What is left of startup settles: the write
        // returning, the await parking, and the blocking threads startup
        // used (the account's readiness check) leaving the runtime's pool
        // once they have been idle its ten seconds — held to the same set
        // of threads for longer than that, then measured.
        let settle = Instant::now() + Duration::from_secs(40);
        let (mut threads, mut since) = (proc_switches(pid)?.into_keys().collect::<Vec<_>>(), Instant::now());
        while since.elapsed() < Duration::from_secs(11) {
            if Instant::now() > settle {
                return Err(format!("`gruntas -p`'s threads never settled: {threads:?}"));
            }
            std::thread::sleep(Duration::from_millis(250));
            let now: Vec<u32> = proc_switches(pid)?.into_keys().collect();
            if now != threads {
                (threads, since) = (now, Instant::now());
            }
        }
        let (t0, w0) = (proc_ticks(pid)?, proc_switches(pid)?);
        std::thread::sleep(window);
        let (t1, w1) = (proc_ticks(pid)?, proc_switches(pid)?);
        let rss_mb = proc_rss_kib(pid)? as f64 * 1024.0 / 1e6;
        if let Ok(Some(st)) = child.try_wait() {
            return Err(format!("`gruntas -p` exited {st} while it should have been waiting"));
        }
        let ticks = t1.checked_sub(t0).ok_or_else(|| format!("CPU ticks went backwards ({t0} to {t1})"))?;
        Ok(Idle { ticks, wakeups: wakeups_between(&w0, &w1)?, rss_mb })
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// utime + stime from /proc/<pid>/stat: every thread, since the process's
/// line sums them. The fields are counted after the command name's closing
/// parenthesis, which may itself contain spaces.
pub(crate) fn proc_ticks(pid: u32) -> Result<u64, String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| format!("/proc/{pid}/stat: {e}"))?;
    parse_ticks(&stat).ok_or_else(|| format!("/proc/{pid}/stat: unreadable line {stat:?}"))
}

fn parse_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state is field 3, utime 14 and stime 15.
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

/// Voluntary and involuntary context switches, per thread id.
pub(crate) fn proc_switches(pid: u32) -> Result<Switches, String> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).map_err(|e| format!("/proc/{pid}/task: {e}"))?;
    let mut per = Switches::new();
    for t in tasks.flatten() {
        let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let status = std::fs::read_to_string(t.path().join("status")).map_err(|e| format!("/proc/{pid}/task/{tid}/status: {e}"))?;
        let n = status_field(&status, "voluntary_ctxt_switches:").unwrap_or(0) + status_field(&status, "nonvoluntary_ctxt_switches:").unwrap_or(0);
        per.insert(tid, n);
    }
    Ok(per)
}

pub(crate) type Switches = std::collections::BTreeMap<u32, u64>;

/// The wakeups between two samples. Only a comparison of the same threads
/// means anything: a thread that exited mid-window takes its switches out
/// of the sum, and one that started brings in switches from before the
/// window, so either is an error rather than a number — and a thread
/// being started or reaped while idle is not idle anyway.
pub(crate) fn wakeups_between(before: &Switches, after: &Switches) -> Result<u64, String> {
    if before.keys().ne(after.keys()) {
        return Err(format!("the threads changed during the idle window ({:?} to {:?})", before.keys().collect::<Vec<_>>(), after.keys().collect::<Vec<_>>()));
    }
    let mut n = 0;
    for (tid, b) in before {
        let a = after[tid];
        n += a.checked_sub(*b).ok_or_else(|| format!("thread {tid}'s context switches went backwards ({b} to {a})"))?;
    }
    Ok(n)
}

pub(crate) fn proc_rss_kib(pid: u32) -> Result<u64, String> {
    proc_status_kib(pid, "VmRSS:")
}

/// A kB field of /proc/<pid>/status: `VmRSS:` now, `VmHWM:` at its peak.
pub(crate) fn proc_status_kib(pid: u32, key: &str) -> Result<u64, String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(|e| format!("/proc/{pid}/status: {e}"))?;
    status_field(&status, key).ok_or_else(|| format!("/proc/{pid}/status has no {key}"))
}

fn status_field(status: &str, key: &str) -> Option<u64> {
    status.lines().find_map(|l| l.strip_prefix(key)).and_then(|v| v.split_whitespace().next()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_perf_2_ticks_are_read_past_a_name_with_spaces() {
        let stat = "4242 (gruntas -p x) S 1 4242 4242 0 -1 4194304 700 0 0 0 17 5 0 0 20 0 1 0 100 7000000 1787 18446744073709551615";
        assert_eq!(parse_ticks(stat), Some(22));
    }

    #[test]
    fn r_perf_2_switches_and_rss_are_read_from_status() {
        let status = "Name:\tkrowk\nVmRSS:\t    7148 kB\nvoluntary_ctxt_switches:\t5\nnonvoluntary_ctxt_switches:\t2\n";
        assert_eq!(status_field(status, "VmRSS:"), Some(7148));
        assert_eq!(status_field(status, "voluntary_ctxt_switches:"), Some(5));
        assert_eq!(status_field(status, "nonvoluntary_ctxt_switches:"), Some(2));
    }

    #[test]
    fn r_perf_2_wakeups_compare_the_same_threads_only() {
        let s = |xs: &[(u32, u64)]| xs.iter().copied().collect::<Switches>();
        assert_eq!(wakeups_between(&s(&[(1, 5), (2, 7)]), &s(&[(1, 5), (2, 9)])), Ok(2));
        // A thread that exits mid-window would lower the sum and hide the
        // wakeups of the rest; one that starts is not idle either.
        let exited = wakeups_between(&s(&[(1, 5), (2, 7)]), &s(&[(1, 9)])).unwrap_err();
        assert!(exited.contains("threads changed"), "{exited}");
        let started = wakeups_between(&s(&[(1, 5)]), &s(&[(1, 5), (3, 0)])).unwrap_err();
        assert!(started.contains("threads changed"), "{started}");
        let back = wakeups_between(&s(&[(1, 5)]), &s(&[(1, 4)])).unwrap_err();
        assert!(back.contains("went backwards"), "{back}");
    }
}
