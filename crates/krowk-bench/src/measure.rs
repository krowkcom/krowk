//! The measurements, one per enforced budget id. Each runs the real built
//! binary (or, for the log, the harness's own writer) and returns the median
//! of its runs; none of them compares against an earlier run.

use crate::budgets::{Outcome, median};
use krowk_harness::log::SessionLog;
use krowk_harness::protocol::{Item, LogBody};
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
/// scripts/lean_deps_check.sh holds to crates/krowk/lean-deps.txt by name.
pub fn lean_deps() -> Outcome {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo).args(["tree", "-p", "krowk", "-e", "normal", "--prefix", "none", "--locked", "--target", "all"]).output();
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
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(home)
        .stdin(Stdio::null());
    c
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

/// The system prompt plus tool definitions every model call carries, in
/// the tokens `krowk -p` records for them in the session's context.jsonl:
/// the largest over the toolset presets. Each preset is one real `krowk -p
/// --toolset <preset>` against a local provider that refuses every request
/// at once — the record is written before the first call, so a refusal
/// costs nothing but the process. The count is krowk's estimate (four
/// bytes a token), deterministic, so one run each is the number. Run from
/// `/`: the working directory is the one variable part of the system
/// prompt, and the number should be the code's, not the checkout path's.
pub fn context_tokens(bin: &Path, home: &Path) -> Outcome {
    use std::io::{Read, Write};
    if let Err(e) = std::fs::create_dir_all(home) {
        return Outcome::Error(format!("{}: {e}", home.display()));
    }
    let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => return Outcome::Error(format!("bind the refusing provider: {e}")),
    };
    let url = format!("http://{}", listener.local_addr().expect("bound"));
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            // Read the request's head and whatever of the body came with
            // it, then refuse: 400 is not retried.
            let mut buf = [0u8; 64 * 1024];
            let _ = conn.read(&mut buf);
            let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"krowk-bench refuses every request"}}"#;
            let _ = write!(conn, "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        }
    });
    let mut per = Vec::new();
    for preset in krowk_harness::toolset::PRESETS {
        let before = sessions(home);
        let run = sandboxed(bin, home)
            .args(["-p", "hello", "--toolset", preset.name, "--output-format", "json"])
            .current_dir("/")
            .env("ANTHROPIC_API_KEY", "sk-bench")
            .env("ANTHROPIC_BASE_URL", &url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Err(e) = run {
            return Outcome::Error(format!("{}: {e}", bin.display()));
        }
        let Some(dir) = sessions(home).into_iter().find(|d| !before.contains(d)) else {
            return Outcome::Error(format!("`krowk -p --toolset {}` left no session behind", preset.name));
        };
        let rec = std::fs::read_to_string(dir.join("context.jsonl")).ok().and_then(|raw| serde_json::from_str::<serde_json::Value>(raw.lines().next()?).ok());
        let tokens = rec.as_ref().and_then(|r| Some(r["systemTokens"].as_u64()? + r["toolsTokens"].as_u64()?));
        match tokens {
            Some(t) => per.push((preset.name, t)),
            None => return Outcome::Error(format!("{}: no context record with token counts", dir.display())),
        }
    }
    let max = per.iter().map(|(_, t)| *t).max().unwrap_or(0);
    let note = per.iter().map(|(p, t)| format!("{p} {t}")).collect::<Vec<_>>().join(" · ");
    Outcome::Measured { value: max as f64, note }
}

/// The session directories `krowk -p` has made under a sandboxed home:
/// those holding a context record, never krowk.db or the import lock that
/// sit beside them in `sessions/`.
fn sessions(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home.join(".krowk/sessions")).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.join("context.jsonl").is_file()).collect()).unwrap_or_default()
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
#[cfg(unix)]
pub fn remote_attach(runs: usize) -> Outcome {
    use krowk_client::e2e::{self, AccountKey, DeviceKey, SessionKey, SessionKeys};
    use krowk_harness::sync::store;
    let reg = match krowk_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").expect("loopback"), Default::default()) {
        Ok(r) => r,
        Err(e) => return Outcome::Error(e.to_string()),
    };
    let account = AccountKey::generate();
    let user = krowk_client::user_key::UserKey::first();
    let device = DeviceKey::generate();
    let signing = e2e::SigningKey::generate();
    let signer = e2e::DeviceSigner::new(device.id(), e2e::SigningKey::from_secret(&*signing.secret_bytes()).expect("a key")).shared();
    let api = std::sync::Arc::new(krowk_api::Client::new(&format!("{}/v1", reg.url()), "krowk_sk_bench_remote_attach_000000000").signed_by(signer));
    let setup = || -> Result<(String, SessionKeys), String> {
        api.register_device(&e2e::hex(&device.public().0), &e2e::hex(&signing.public().0), "bench", &account.id().to_string()).map_err(|e| e.to_string())?;
        let id = "01a0ec7b-0000-7000-8000-00000000be0c".to_string();
        let raw = krowk_harness::daemon::ws::uuid(&id);
        let key = SessionKeys::from(SessionKey::generate());
        let wrapped = e2e::hex(&e2e::wrap_session_keys(&key, &raw, &user));
        let index = store::Index { title: "bench".into(), ..Default::default() };
        let sealed = e2e::hex(&e2e::seal_session_index(key.current(), &raw, &serde_json::to_vec(&index).map_err(|e| e.to_string())?));
        api.put_sync_session(&id, &wrapped, None, Some(&sealed), None).map_err(|e| e.to_string())?;
        let lease = api.acquire_lease(&id, &device.id().to_string(), 60, "development").map_err(|e| e.to_string())?;
        let mut w = store::Writer::take_up(api.clone(), key.clone(), &id, wrapped, index, lease.fence)?;
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(40);
        for turn in 0..40 {
            for i in 0..6 {
                w.push(serde_json::json!({"id": format!("{turn}-{i}"), "type": "item.completed", "item": {"type": "assistantText", "text": text}}));
            }
            w.flush(&lease.token)?;
            if turn == 29 {
                w.checkpoint(None, &lease.token)?;
            }
        }
        Ok((id, key))
    };
    let (id, key) = match setup() {
        Ok(x) => x,
        Err(e) => return Outcome::Error(e),
    };
    let mut xs = Vec::with_capacity(runs);
    let mut events = 0;
    for _ in 0..runs.max(1) {
        let t = Instant::now();
        let got = api.show_sync_session(&id).map_err(|e| e.to_string()).and_then(|s| store::open_index(&key, &id, &s.sealed_index)).and_then(|index| store::attach(&api, &key, &id, index, None));
        match got {
            Ok(a) => events = a.events.len(),
            Err(e) => return Outcome::Error(e),
        }
        xs.push(t.elapsed().as_secs_f64() * 1e3);
    }
    Outcome::Measured { value: median(&mut xs), note: format!("{} runs, {events} events from a checkpoint and 10 chunks of tail", xs.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_perf_5_an_append_is_measured_through_the_session_log() {
        let dir = std::env::temp_dir().join(format!("krowk-bench-test-{}", std::process::id()));
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
