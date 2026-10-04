//! The TUI's budgets (R-PERF-1 to R-PERF-4), measured on the full build
//! running under a pseudo-terminal the way a person runs it: bare `krowk`
//! in a terminal, against a local stand-in for the Anthropic API. Linux
//! only, like the other /proc readings; the pinned runner is Linux.
//!
//! - `tui.startup_cold` — spawn to the first complete frame (the end of the
//!   first synchronized update), which is the prompt, with no host daemon
//!   running: the TUI starts one and connects before its first frame.
//! - `tui.startup_warm` — the same with the daemon up already.
//!
//! Every other TUI budget runs the TUI's sessions in its own process
//! (`KROWK_TUI_HOST=local`): they hold the TUI and the engine together, as
//! they were written to.
//! - `tui.idle_cpu`, `tui.idle_rss` — the TUI sitting at its prompt with
//!   nothing running, read from /proc like `engine.idle_*`.
//! - `tui.turn_cpu` — CPU ticks while a turn waits on a silent provider:
//!   the spinner's 8 fps and nothing else.
//! - `tui.redraw_fps` — the most frames begun inside any one second while a
//!   500 token-a-second answer streams in.
//! - `session.replay_rss` — peak resident memory (VmHWM) of `krowk --resume`
//!   on a synthetic 200k-token session: its history drawn into scrollback,
//!   then one more turn, which sends the whole history to the model.

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;
#[path = "../../krowk/tests/common/pty.rs"]
mod pty;

use crate::budgets::{median, Outcome};
use crate::idle::{proc_rss_kib, proc_status_kib, proc_switches, proc_ticks, wakeups_between};
use crate::measure::{sandboxed, Idle};
use krowk_harness::log::SessionLog;
use krowk_harness::protocol::{Item, LogBody, ModelRef, PermissionMode, TurnStatus, Usage, WireApi};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// The status line's last words, in the bytes the TUI writes: its key
/// `?` has a colour of its own, so what follows it is matched, not `? help`
/// (and ` to interrupt`, not `esc to interrupt`).
const PROMPT: &str = " help";

const COLS: u16 = 100;
const ROWS: u16 = 30;

/// The full build on a terminal, in its own home, talking to `url`, its
/// sessions in its own process.
fn tui(bin: &Path, home: &Path, url: &str, args: &[&str]) -> Command {
    let mut c = on_daemon(bin, home, url, args, None);
    c.env("KROWK_TUI_HOST", "local");
    c
}

/// The full build on a terminal, its sessions in the host daemon of
/// runtime directory `run` when given.
fn on_daemon(bin: &Path, home: &Path, url: &str, args: &[&str], run: Option<&Path>) -> Command {
    let mut c = sandboxed(bin, home);
    c.args(args).env("TERM", "xterm-256color").env("KROWK_NO_UPDATE_CHECK", "1").env("ANTHROPIC_API_KEY", "sk-bench").env("ANTHROPIC_BASE_URL", url);
    if let Some(run) = run {
        c.env("XDG_RUNTIME_DIR", run);
    }
    c
}

/// A runtime directory for the daemon: short, since a socket path is, and
/// private, as the daemon requires.
fn runtime_dir(name: &str) -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let run = std::path::PathBuf::from(format!("/tmp/krowk-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run);
    std::fs::create_dir_all(&run).map_err(|e| format!("{}: {e}", run.display()))?;
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    Ok(run)
}

/// Whether a daemon's socket is in runtime directory `run`
/// (`krowk/<home>/host.sock`).
fn has_socket(run: &Path) -> bool {
    std::fs::read_dir(run.join("krowk")).into_iter().flatten().flatten().any(|e| e.path().join("host.sock").exists())
}

/// `krowk host stop`, waited for: the next run starts with none.
fn stop_daemon(bin: &Path, home: &Path, run: &Path) {
    let _ = on_daemon(bin, home, "http://127.0.0.1:9", &["host", "stop", "--force"], Some(run)).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
}

/// A provider that is there — its port open, so the TUI's connectivity
/// probe finds it — and answers any request with `reply`.
fn provider(reply: impl Fn() -> mock::Reply + Send + 'static) -> mock::Mock {
    mock::serve(move |_, _| reply())
}

/// Ctrl-D twice: the first only asks for the second.
fn quit(t: &mut pty::Pty) -> Result<(), String> {
    t.write(b"\x04");
    std::thread::sleep(Duration::from_millis(100));
    t.write(b"\x04");
    match t.wait(Duration::from_secs(10)) {
        Some(st) if st.success() => Ok(()),
        Some(st) => Err(format!("the TUI exited {st} on Ctrl-D")),
        None => Err("the TUI did not exit on Ctrl-D within 10 s".into()),
    }
}

/// Spawn to the first complete frame, the median of `runs` fresh processes
/// after one discarded warm-up, as `startup` measures `--version`: with the
/// host daemon warm — started once, up for every run — or cold, stopped
/// after each, so every run starts one.
pub fn startup(bin: &Path, home: &Path, runs: usize, warm: bool) -> Outcome {
    std::fs::create_dir_all(home).ok();
    let run = match runtime_dir(if warm { "warm" } else { "cold" }) {
        Ok(r) => r,
        Err(e) => return Outcome::Error(e),
    };
    let outcome = startup_in(bin, home, runs, warm, &run);
    stop_daemon(bin, home, &run);
    let _ = std::fs::remove_dir_all(&run);
    outcome
}

fn startup_in(bin: &Path, home: &Path, runs: usize, warm: bool, run: &Path) -> Outcome {
    let m = provider(|| mock::Reply::sse(&mock::text_stream("ok")));
    let once = || -> Result<f64, String> {
        let mut t = pty::Pty::spawn(on_daemon(bin, home, &m.url, &[], Some(run)), COLS, ROWS);
        let deadline = t.started + Duration::from_secs(10);
        let first = loop {
            if let Some(end) = t.frame_ends().first() {
                break *end;
            }
            if Instant::now() > deadline {
                return Err(format!("no frame within 10 s: {:?}", t.text()));
            }
            std::thread::sleep(Duration::from_micros(200));
        };
        let ms = first.duration_since(t.started).as_secs_f64() * 1000.0;
        if !t.text().contains(PROMPT) {
            return Err(format!("the first frame is not the prompt: {:?}", t.text()));
        }
        // Measured on the daemon, or not at all: a TUI that fell back to
        // its own process would pass this budget without one.
        if t.text().contains("could not be reached") || !has_socket(run) {
            return Err(format!("the TUI did not reach the host daemon: {:?}", t.text()));
        }
        quit(&mut t)?;
        if !warm {
            stop_daemon(bin, home, run);
        }
        Ok(ms)
    };
    // The warm-up starts the daemon that a warm run finds up.
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
    xs.sort_by(f64::total_cmp);
    let (lo, hi) = (xs[0], xs[xs.len() - 1]);
    Outcome::Measured { value: median(&mut xs), note: format!("{} runs, {lo:.1}–{hi:.1}", xs.len()) }
}

/// The TUI at its prompt with nothing running: sampled once its first
/// frame is drawn and its start-up probe has answered, then again after
/// `window`.
pub fn idle(bin: &Path, home: &Path, window: Duration) -> Result<Idle, String> {
    std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    let m = provider(|| mock::Reply::sse(&mock::text_stream("ok")));
    let mut t = pty::Pty::spawn(tui(bin, home, &m.url, &[]), COLS, ROWS);
    let pid = t.child.id();
    let result = (|| {
        t.wait_for(PROMPT, Duration::from_secs(10)).ok_or_else(|| format!("the TUI never reached its prompt: {:?}", t.text()))?;
        // What is left of start-up — the probe's blocking thread above
        // all, kept alive a quarter of a second — settles well inside this.
        std::thread::sleep(Duration::from_secs(2));
        let drawn = t.output().len();
        let (t0, w0) = (proc_ticks(pid)?, proc_switches(pid)?);
        std::thread::sleep(window);
        let (t1, w1) = (proc_ticks(pid)?, proc_switches(pid)?);
        let rss_mb = proc_rss_kib(pid)? as f64 * 1024.0 / 1e6;
        if t.output().len() != drawn {
            return Err(format!("the idle TUI drew {} bytes", t.output().len() - drawn));
        }
        let ticks = t1.checked_sub(t0).ok_or_else(|| format!("CPU ticks went backwards ({t0} to {t1})"))?;
        Ok(Idle { ticks, wakeups: wakeups_between(&w0, &w1)?, rss_mb })
    })();
    let quit = quit(&mut t);
    let idle = result?;
    quit?;
    Ok(idle)
}

/// A provider that takes the request and never answers: a turn that
/// waits on the model, which is the TUI's busiest quiet state — its clock
/// and spinner redraw, nothing else happens.
fn silent_provider() -> Result<String, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind the silent provider: {e}"))?;
    let url = format!("http://{}", l.local_addr().map_err(|e| e.to_string())?);
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for c in l.incoming().flatten() {
            held.push(c);
        }
    });
    Ok(url)
}

/// CPU ticks of the TUI over `window` while a turn waits on a silent
/// provider, and the frames it drew meanwhile.
pub fn turn_cpu(bin: &Path, home: &Path, window: Duration) -> Outcome {
    let run = || -> Result<(u64, usize), String> {
        std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
        let url = silent_provider()?;
        let mut t = pty::Pty::spawn(tui(bin, home, &url, &[]), COLS, ROWS);
        let pid = t.child.id();
        let r = (|| {
            t.wait_for(PROMPT, Duration::from_secs(10)).ok_or("the TUI never reached its prompt")?;
            t.write(b"wait\r");
            t.wait_for(" to interrupt", Duration::from_secs(10)).ok_or("the turn never started")?;
            // Past the start: the request out, the stall probe answered.
            std::thread::sleep(Duration::from_secs(2));
            let (f0, t0) = (t.frames().len(), proc_ticks(pid)?);
            std::thread::sleep(window);
            Ok((proc_ticks(pid)?.saturating_sub(t0), t.frames().len() - f0))
        })();
        let _ = t.child.kill();
        r
    };
    match run() {
        Ok((ticks, frames)) => Outcome::Measured { value: ticks as f64, note: format!("{} s window, {frames} frames", window.as_secs()) },
        Err(e) => Outcome::Error(e),
    }
}

/// Peak frames per second while an answer streams at 500 tokens a second
/// (one delta every 2 ms) for about four seconds.
pub fn redraw(bin: &Path, home: &Path) -> Outcome {
    std::fs::create_dir_all(home).ok();
    let body = mock::text_stream(&mock::numbered_lines(170));
    let m = provider(move || mock::Reply::paced(body.clone(), Duration::from_millis(2)));
    let mut t = pty::Pty::spawn(tui(bin, home, &m.url, &[]), COLS, ROWS);
    let result = (|| {
        t.wait_for(PROMPT, Duration::from_secs(10)).ok_or("the TUI never reached its prompt")?;
        let before = t.frames().len();
        let t0 = Instant::now();
        t.write(b"stream\r");
        t.wait_for("00170:", Duration::from_secs(30)).ok_or("the stream never finished")?;
        let secs = t0.elapsed().as_secs_f64();
        let frames = t.frames()[before..].to_vec();
        Ok::<_, String>((pty::peak_fps(&frames), frames.len(), secs))
    })();
    let _ = quit(&mut t);
    match result {
        Ok((peak, n, secs)) => Outcome::Measured { value: peak as f64, note: format!("{n} frames over {secs:.1} s at 500 tok/s") },
        Err(e) => Outcome::Error(e),
    }
}

/// A session of `turns` turns and about `tokens` tokens of answer, written
/// through the harness's own log writer, so it is a log krowk wrote.
fn synthetic_session(sessions: &Path, cwd: &Path, turns: usize, tokens: usize) -> Result<String, String> {
    let (mut log, root) = SessionLog::create(sessions, cwd, "0.0.0-bench").map_err(|e| e.message().to_string())?;
    let per_turn = tokens / turns;
    // About four characters a token, in lines a terminal shows whole.
    let line = "the quick brown fox jumps over the lazy dog, and then it does it again. ";
    let answer: String = (0..per_turn * 4 / line.len()).map(|i| format!("{i:04} {line}\n")).collect();
    let model = ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() };
    for t in 0..turns {
        let turn_id = format!("turn-{t}");
        let mut append = |body| log.append(body).map(|_| ()).map_err(|e| e.message().to_string());
        append(LogBody::TurnStarted { turn_id: turn_id.clone(), model: model.clone(), provider: "anthropic".into(), wire_api: WireApi::AnthropicMessages, permission_mode: PermissionMode::Default, effort: None })?;
        append(LogBody::ItemCompleted { turn_id: turn_id.clone(), item_id: format!("p-{t}"), item: Item::user(format!("tell me part {t}")) })?;
        append(LogBody::ItemCompleted { turn_id: turn_id.clone(), item_id: format!("a-{t}"), item: Item::AssistantText { text: answer.clone() } })?;
        let usage = Usage { input_tokens: 20, output_tokens: per_turn as i64, ..Usage::default() };
        append(LogBody::ResponseCompleted { turn_id: turn_id.clone(), response_id: None, model: model.model.clone(), usage, stop_reason: Some("end_turn".into()), item_ids: vec![format!("a-{t}")] })?;
        append(LogBody::TurnCompleted { turn_id, status: TurnStatus::Completed, usage, duration_ms: 1000, error: None, reported_cost_usd: None })?;
    }
    log.sync().map_err(|e| e.message().to_string())?;
    Ok(root.session_id)
}

/// Peak RSS of `krowk --resume` on a 200k-token session, through one more
/// turn that carries the whole history to the provider.
pub fn replay_rss(bin: &Path, home: &Path) -> Outcome {
    let run = || -> Result<(f64, usize), String> {
        std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
        let sessions = home.join(".krowk/sessions");
        let id = synthetic_session(&sessions, home, 100, 200_000)?;
        let m = provider(|| mock::Reply::sse(&mock::text_stream("replayed-and-answered")));
        let mut t = pty::Pty::spawn(tui(bin, home, &m.url, &["--resume", &id]), COLS, ROWS);
        let pid = t.child.id();
        let r = (|| {
            t.wait_for("resumed", Duration::from_secs(30)).ok_or_else(|| format!("the session was not replayed: {:?}", tail(&t.text())))?;
            t.write(b"continue\r");
            t.wait_for("replayed-and-answered", Duration::from_secs(30)).ok_or_else(|| format!("the next turn never answered: {:?}", tail(&t.text())))?;
            let sent = m.seen.lock().unwrap().iter().map(|s| s.body.to_string().len()).max().unwrap_or(0);
            Ok((proc_status_kib(pid, "VmHWM:")? as f64 * 1024.0 / 1e6, sent))
        })();
        let _ = quit(&mut t);
        r
    };
    match run() {
        Ok((mb, sent)) => Outcome::Measured { value: mb, note: format!("100 turns, request of {:.1} MB", sent as f64 / 1e6) },
        Err(e) => Outcome::Error(e),
    }
}

fn tail(s: &str) -> String {
    s.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect()
}
