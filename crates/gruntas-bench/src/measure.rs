//! The measurements, one per enforced budget id. Each runs the real built
//! binary (or, for the log, the harness's own writer) and returns the median
//! of its runs; none of them compares against an earlier run.

use crate::budgets::{Outcome, median};
use gruntas_harness::log::SessionLog;
use gruntas_harness::protocol::{Item, LogBody};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub fn size(bin: &Path) -> Outcome {
    match std::fs::metadata(bin) {
        Ok(m) => Outcome::Measured { value: m.len() as f64, note: format!("{} bytes", m.len()) },
        Err(e) => Outcome::Error(format!("{}: {e}", bin.display())),
    }
}

/// The agent build's crates, resolved for every target — the same set
/// scripts/lean_deps_check.sh holds to crates/gruntas/lean-deps.txt by name.
pub fn lean_deps() -> Outcome {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo).args(["tree", "-p", "gruntas", "-e", "normal", "--prefix", "none", "--locked", "--target", "all"]).output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let crates: std::collections::BTreeSet<&str> = text.lines().filter_map(|l| l.split_whitespace().next()).collect();
            Outcome::Measured { value: crates.len() as f64, note: String::new() }
        }
        Ok(o) => Outcome::Error(format!("cargo tree: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => Outcome::Error(format!("cargo tree: {e}")),
    }
}

/// A clean environment for a measured process: its own home, so neither
/// the person's config nor their sessions change what is measured.
pub fn sandboxed(bin: &Path, home: &Path) -> Command {
    let mut c = Command::new(bin);
    c.env_clear()
        .env("HOME", home)
        .env("PATH", bare_path(home))
        .current_dir(home)
        .stdin(Stdio::null());
    c
}

/// The CLIs krowk connects by finding them on PATH: kept off it, so a
/// machine with them installed measures the same as one without — no
/// implicit instance of the real `claude` or `codex` beside the bench's.
const AGENT_CLIS: [&str; 2] = ["claude", "codex"];

/// The caller's PATH with each directory that holds one of `AGENT_CLIS`
/// stood in for by a directory beside the homes (`home`'s parent, the
/// work directory) of links to all it holds but those: the measured
/// krowk still finds `git`, `sh` or `bwrap` there. Made once a run.
fn bare_path(home: &Path) -> std::ffi::OsString {
    static PATH: std::sync::OnceLock<std::ffi::OsString> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let work = home.parent().unwrap_or(home).join("path");
        let _ = std::fs::remove_dir_all(&work);
        let dirs = std::env::split_paths(&path).enumerate().filter_map(|(i, d)| {
            // A relative entry means nothing to the measured krowk, nor to
            // the daemon it starts (which runs in `/`, where `usr/bin`
            // would find a CLI): left out.
            if !d.is_absolute() {
                return None;
            }
            if cfg!(not(unix)) || !AGENT_CLIS.iter().any(|b| d.join(b).exists()) {
                return Some(d);
            }
            let stand_in = work.join(i.to_string());
            let _ = std::fs::create_dir_all(&stand_in);
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                #[cfg(unix)]
                if !AGENT_CLIS.iter().any(|b| e.file_name() == *b) {
                    let _ = std::os::unix::fs::symlink(e.path(), stand_in.join(e.file_name()));
                }
            }
            Some(stand_in)
        });
        // Never the caller's PATH instead: that measures nothing again.
        std::env::join_paths(dirs.collect::<Vec<_>>()).unwrap_or_else(|e| panic!("{} cannot stand in PATH entries: {e}", work.display()))
    })
    .clone()
}

/// Wall time from spawn to exit, the median of `runs` new processes. One run
/// is made first and thrown away: it creates what a first run creates (an
/// empty krowk.db) and pulls the binary into the page cache, which a runner
/// cannot drop without root. "Cold" is the process — no daemon, nothing
/// loaded — not the disk.
pub fn startup(bin: &Path, args: &[&str], runs: usize, home: &Path) -> Outcome {
    if let Err(e) = std::fs::create_dir_all(home) {
        return Outcome::Error(format!("{}: {e}", home.display()));
    }
    let once = || -> Result<f64, String> {
        let t = Instant::now();
        let st = sandboxed(bin, home).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().map_err(|e| format!("{}: {e}", bin.display()))?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if !st.success() {
            return Err(format!("`krowk {}` exited {st}", args.join(" ")));
        }
        Ok(ms)
    };
    if let Err(e) = once() {
        return Outcome::Error(e);
    }
    let mut xs = Vec::with_capacity(runs);
    for _ in 0..runs.max(1) {
        match once() {
            Ok(ms) => xs.push(ms),
            Err(e) => return Outcome::Error(e),
        }
    }
    let (lo, hi) = spread(&mut xs);
    Outcome::Measured { value: median(&mut xs), note: format!("{} runs, {lo:.1}–{hi:.1}", xs.len()) }
}

fn spread(xs: &mut [f64]) -> (f64, f64) {
    xs.sort_by(f64::total_cmp);
    (xs[0], xs[xs.len() - 1])
}

/// One event appended through `SessionLog`, as a turn appends them: a
/// completed assistant message of 1 KiB, a line to a file opened O_APPEND,
/// no sync (the log syncs once per turn). The median of `runs` appends, in
/// microseconds, with the 99th percentile beside it.
pub fn log_append(work: &Path, runs: usize) -> Outcome {
    let sessions = work.join("log-append");
    let _ = std::fs::remove_dir_all(&sessions);
    let (mut log, _) = match SessionLog::create(&sessions, work, "0.0.0-bench") {
        Ok(l) => l,
        Err(e) => return Outcome::Error(e.message().to_string()),
    };
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(23);
    let mut xs = Vec::with_capacity(runs);
    for i in 0..runs.max(1) {
        let body = LogBody::ItemCompleted { turn_id: "turn-bench".into(), item_id: format!("item-{i}"), item: Item::AssistantText { text: text.clone() } };
        let t = Instant::now();
        if let Err(e) = log.append(body) {
            return Outcome::Error(e.message().to_string());
        }
        xs.push(t.elapsed().as_secs_f64() * 1e6);
    }
    xs.sort_by(f64::total_cmp);
    let p99 = xs[(xs.len() * 99 / 100).min(xs.len() - 1)];
    Outcome::Measured { value: median(&mut xs), note: format!("{} events, p99 {p99:.1} µs", xs.len()) }
}

/// What a process did while it sat idle for the window.
#[derive(Debug, Clone)]
// Only Linux can read one; elsewhere the idle budgets are skipped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Idle {
    /// utime + stime, in clock ticks, over the window.
    pub ticks: u64,
    /// Context switches of every thread over the window: each one a wakeup.
    pub wakeups: u64,
    /// Resident set at the end of the window, in MB.
    pub rss_mb: f64,
}

/// Where the measured processes keep their files: under target/, on the
/// same disk the build is on rather than a tmpfs, so the log is written the
/// way a real one is.
pub fn fresh_dir(work: &Path, name: &str) -> PathBuf {
    let d = work.join(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// R-PERF-6: attaching to a synced session from its checkpoint on another
/// device, in milliseconds — the sealed index, the checkpoint chunk and the
/// tail after it, read from the stand-in registry over HTTP and opened
/// under the session key, which is what a viewer does before it goes live.
/// The session is 40 turns: a checkpoint cut after 30, ten turns of tail.
/// With `resume`, from where a reading after turn 35 stopped instead
/// (`store::resume`, ticket 126): the index and five chunks.
#[cfg(unix)]
pub fn remote_attach(runs: usize, resume: bool) -> Outcome {
    use gruntas_client::device_chain::{Chain, Kind, Subject};
    use gruntas_client::e2e::{self, DeviceKey, SessionKey, SessionKeys};
    use gruntas_harness::sync::store;
    let reg = match gruntas_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").expect("loopback"), Default::default()) {
        Ok(r) => r,
        Err(e) => return Outcome::Error(e.to_string()),
    };
    let device = DeviceKey::generate();
    let signing = e2e::SigningKey::generate();
    let signer = e2e::DeviceSigner::new(device.id(), e2e::SigningKey::from_secret(&*signing.secret_bytes()).expect("a key")).shared();
    let api = std::sync::Arc::new(gruntas_api::Client::new(&format!("{}/v1", reg.url()), "krowk_sk_bench_remote_attach_000000000").signed_by(signer));
    let setup = || -> Result<(String, SessionKeys, Option<store::ReadPlace>), String> {
        // The device alone on a device list, as `krowk sync init` posts it,
        // which binds the key to it.
        let subject = Subject { kind: Kind::Device, name: "bench".into(), os: "linux".into(), device: device.public(), signing: signing.public() };
        let (_, start) = Chain::start(subject, &signing, None, 1_790_000_000).map_err(|e| e.0)?;
        let user = start.newest;
        let post = gruntas_api::sync::ListPost {
            entries: start.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
            links: Vec::new(),
            wraps: start.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
            start_over: false,
        };
        api.init_device_list(&post).map_err(|e| e.to_string())?;
        let id = "01a0ec7b-0000-7000-8000-00000000be0c".to_string();
        let raw = gruntas_harness::daemon::ws::uuid(&id);
        let key = SessionKeys::from(SessionKey::generate());
        let wrapped = e2e::hex(&e2e::wrap_session_keys(&key, &raw, &user));
        let index = store::Index { title: "bench".into(), ..Default::default() };
        let sealed = e2e::hex(&e2e::seal_session_index(key.current(), &raw, &serde_json::to_vec(&index).map_err(|e| e.to_string())?));
        api.put_sync_session(&id, &wrapped, None, Some(&sealed), None).map_err(|e| e.to_string())?;
        let lease = api.acquire_lease(&id, &device.id().to_string(), 60, "development").map_err(|e| e.to_string())?;
        let mut w = store::Writer::take_up(api.clone(), key.clone(), &id, wrapped, index, lease.fence)?;
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(40);
        let mut place = None;
        for turn in 0..40 {
            if turn == 35 {
                let index = store::open_index(&key, &id, &api.show_sync_session(&id).map_err(|e| e.to_string())?.sealed_index)?;
                place = store::attach(&api, &key, &id, index, None)?.place();
            }
            for i in 0..6 {
                w.push(serde_json::json!({"id": format!("{turn}-{i}"), "type": "item.completed", "item": {"type": "assistantText", "text": text}}));
            }
            w.flush(&lease.token)?;
            if turn == 29 {
                w.checkpoint(None, &lease.token)?;
            }
        }
        Ok((id, key, place))
    };
    let (id, key, place) = match setup() {
        Ok(x) => x,
        Err(e) => return Outcome::Error(e),
    };
    let mut xs = Vec::with_capacity(runs);
    let mut events = 0;
    for _ in 0..runs.max(1) {
        let t = Instant::now();
        let got = api.show_sync_session(&id).map_err(|e| e.to_string()).and_then(|s| store::open_index(&key, &id, &s.sealed_index)).and_then(|index| match (resume, place) {
            (true, Some(p)) => store::resume(&api, &key, &id, index, p, None).map_err(String::from),
            (true, None) => Err("the reading after turn 35 kept no place".into()),
            (false, _) => store::attach(&api, &key, &id, index, None).map_err(String::from),
        });
        match got {
            Ok(a) => events = a.events.len(),
            Err(e) => return Outcome::Error(e),
        }
        xs.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let from = if resume { "from a kept place, 5 chunks" } else { "from a checkpoint and 10 chunks of tail" };
    Outcome::Measured { value: median(&mut xs), note: format!("{} runs, {events} events {from}", xs.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_perf_5_an_append_is_measured_through_the_session_log() {
        let dir = std::env::temp_dir().join(format!("gruntas-bench-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let Outcome::Measured { value, note } = log_append(&dir, 50) else { panic!("the log append was not measured") };
        assert!(value > 0.0 && note.starts_with("50 events"), "{value} {note}");
        let sessions = dir.join("log-append");
        let session = std::fs::read_dir(&sessions).unwrap().next().unwrap().unwrap().path();
        let lines = std::fs::read_to_string(session.join("events.jsonl")).unwrap().lines().count();
        assert_eq!(lines, 51, "the root and fifty appends");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
