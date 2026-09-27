//! The inline TUI that bare `krowk` opens on a terminal: a client of the
//! harness's in-process protocol (R-PROTO-1). It sends `Command`s to a
//! `Host` and draws the `StreamLine`s that come back — the same two types
//! `krowk -p` speaks, and the daemon's socket clients will — and it reads a
//! resumed session's history from its log, the protocol's persisted half.
//! Nothing here reaches into the engine.
//!
//! - `term` — the inline viewport and synchronized frames (R-TUI-1).
//! - `app` — what is shown, driven by frames and keys.
//! - `editor` — the multi-line prompt and its history.
//! - `look` — glyphs, colours, the spinner and the light markdown.
//! - `settings` — the status line's configuration (R-TUI-2).
//! - `device` — the `<user>/<host>` the status line opens with, read once.
//! - `net` — the connectivity probe behind the offline notice (R-OFF-1).
//!
//! The loop is event-driven end to end (R-PERF-2): it sleeps in one
//! `select!` until a key, a frame of the stream, the turn's end or a
//! deadline it set itself wakes it, and it sets deadlines only while there
//! is something to time — a frame owed (at most one per 1/60 s, R-PERF-4),
//! a running turn's clock, an interrupt not yet taken, a probe. Idle, with
//! nothing running and the API reachable, it has none, and wakes for
//! nothing but a key.

pub mod app;
pub mod card;
pub mod clipboard;
pub mod device;
pub mod presence;
pub mod editor;
pub mod help;
pub mod look;
pub mod net;
pub mod settings;
pub mod term;

use app::{App, Overlay};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editor::Editor;
use futures_core::Stream;
use krowk_harness::engine::EngineError;
use krowk_harness::host::{Host, HostConfig, Pricer};
use krowk_harness::instances::Asked;
use krowk_harness::log;
use krowk_harness::protocol::{ApprovalDecision, BudgetLimits, Command, Effort, ModelRef, PermissionMode, RunResult, StreamLine, TurnStatus};
use net::Target;
use ratatui::layout::Size;
use settings::Settings;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use term::Term;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// The shortest time between two frames: 60 per second at most (R-PERF-4),
/// with a millisecond to spare, so a frame that reaches the terminal late
/// and the one after it on time still never make 61 in a second.
pub const FRAME: Duration = Duration::from_millis(17);
/// A model call silent for this long gets a connectivity probe.
const STALL: Duration = Duration::from_millis(700);
/// How long an approval request is on screen before a key answers it.
const APPROVAL_SETTLE: Duration = Duration::from_millis(400);
/// How often an interrupt the host could not take yet is asked again.
const INTERRUPT_RETRY: Duration = Duration::from_millis(50);

pub struct Options {
    pub host: HostConfig,
    /// A session to continue: its history is drawn first.
    pub resume: Option<String>,
    /// The model for every prompt; the session's own when absent.
    pub model: Option<ModelRef>,
    /// The model the person named with `--model`: beside the session's
    /// own, the one a bare `/model` id stays on when it can.
    pub chosen: Option<ModelRef>,
    /// A model still to be routed — a bare `--model`, or none at all on a
    /// session that has none — which the TUI routes itself once its first
    /// frame is up, so that frame never waits on a vendor's status check.
    pub route: Option<Route>,
    /// The trust question for the session's repository, asked in the TUI
    /// when the model it routed runs on a backend there.
    pub trust: Option<TrustAsk>,
    pub permission_mode: PermissionMode,
    /// The toolset preset for every prompt; the model's own when absent.
    pub toolset: Option<String>,
    /// The reasoning effort for every prompt, on krowk's ladder.
    pub effort: Option<Effort>,
    /// `--max-usd` and `--max-tokens`, held for every prompt.
    pub budget: Option<BudgetLimits>,
    pub settings: Settings,
    /// Where prompt history is kept; none keeps it in memory only.
    pub history_file: Option<PathBuf>,
    /// Lines shown above the first prompt: config warnings and the like.
    pub notices: Vec<String>,
    pub version: String,
}

/// What the TUI routes once it is up (`Host::route_model`).
pub struct Route {
    /// A bare id; none routes the default.
    pub asked: Option<Asked>,
    /// The session's model, whose instance a bare id stays on when it can.
    pub current: Option<ModelRef>,
}

/// The trust question for the session's repository, asked in the TUI:
/// the one the launcher asks before the TUI takes the terminal, for a model
/// that was only known once routed.
#[derive(Clone)]
pub struct TrustAsk {
    /// The repository's root.
    pub root: PathBuf,
    /// Whether it is trusted now.
    pub trusted: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Why it cannot be trusted for good (the home directory, `/`); then
    /// nothing is asked.
    pub refuses: Option<String>,
    /// Trusts it: for this run, and remembered when it can be — an error
    /// when it could not be.
    pub accept: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

/// How the TUI ended.
pub struct Outcome {
    /// The session it ran turns in, for the caller to project into krowk.db.
    pub session_id: Option<String>,
    /// Why it could not run or stopped early.
    pub error: Option<String>,
    /// Left without waiting for the running turn — a second Ctrl-C, or a
    /// second SIGTERM/SIGHUP: the caller exits 130 once the session is
    /// recorded.
    pub abandoned: bool,
}

/// Opens the TUI on this process's terminal and runs it until the person
/// quits. Raw mode is on for the duration and off again however it ends —
/// a panic included, since the release profile aborts rather than unwinds.
pub fn run(opts: Options) -> Outcome {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        // A probe's name lookup runs on a blocking thread; one left idle
        // would be a wakeup when it is reaped, so none lingers.
        .thread_keep_alive(Duration::from_millis(250))
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return Outcome { session_id: None, abandoned: false, error: Some(format!("the async runtime could not start: {e}")) },
    };
    if let Err(e) = crossterm::terminal::enable_raw_mode() {
        return Outcome { session_id: None, abandoned: false, error: Some(format!("the terminal could not be put in raw mode: {e}")) };
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));
    let outcome = rt.block_on(session(opts));
    restore_terminal();
    // A readiness check still running (the person quit while the model was
    // being routed) is not waited for: its process group, registered while
    // it runs, is killed here, and its blocking thread is let go with the
    // runtime instead of holding the exit for up to its deadline.
    krowk_harness::group::kill_all();
    rt.shutdown_background();
    outcome
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?2004l\x1b[?25h");
    let _ = out.flush();
}

type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<RunResult>, EngineError>> + 'a>>;
type ProbeFuture = Pin<Box<dyn Future<Output = bool>>>;
type RouteFuture<'a> = Pin<Box<dyn Future<Output = Result<ModelRef, EngineError>> + 'a>>;

async fn session(opts: Options) -> Outcome {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[?2004h");
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    let size = Size { width: w.max(1), height: h.max(1) };
    // Asked once, before anything else reads the terminal. A cursor mid-line
    // (a prompt without a trailing newline) gets a line of its own.
    let top = match crossterm::cursor::position() {
        Ok((0, y)) => Some(y),
        Ok((_, y)) => {
            let _ = stdout.write_all(b"\r\n");
            Some((y + 1).min(size.height - 1))
        }
        Err(_) => None,
    };
    // A clean window to open on: what the shell left on screen scrolls up
    // into scrollback — kept, not erased, the way a clear-screen that
    // scrolls first keeps it — and the session starts on an empty screen.
    // Where the cursor is unknown nothing is scrolled: a screenful of
    // blank rows in scrollback is no clean window.
    let top = match top {
        Some(top) => clear_by_scrolling(&mut stdout, size, top),
        None => size.height - 1,
    };

    let sessions_dir = opts.host.sessions_dir.clone();
    let pricer: Pricer = opts.host.pricer.clone();
    let mut app = App::new(Editor::new(opts.history_file.clone()), inner(size.width), opts.settings.clone(), None, Some(pricer));
    app.log_dir = Some(sessions_dir.display().to_string());
    app.permission_mode = serde_json::to_value(opts.permission_mode).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    // Where the session runs: a resumed one where it started.
    let mut runs_in = opts.host.cwd.clone();
    if let Some(id) = &opts.resume {
        match log::read_events(&sessions_dir.join(id).join(log::EVENTS_FILE)) {
            Ok(events) => {
                if let Some(krowk_harness::protocol::LogBody::SessionStarted { cwd, .. }) = events.first().map(|e| &e.body) {
                    runs_in = PathBuf::from(cwd);
                }
                let head = events.last().map(|e| e.id.clone()).unwrap_or_default();
                app.replay(&log::branch(&events, &head));
                app.session_id = Some(id.clone());
                app.say(&format!("resumed session {id}"), app::dim());
            }
            Err(e) => return Outcome { session_id: None, abandoned: false, error: Some(format!("session {id} could not be read: {}", e.message())) },
        }
    }
    // The model shown before the first turn names one: the one given
    // (`--model`, or the configured default that names its instance), else
    // the session's last. One still to be routed is shown once it is.
    let shown = opts.model.clone().or_else(|| app.model.clone());
    let target = shown.as_ref().and_then(|m| opts.host.registry.get(&m.instance).ok()).and_then(|i| Target::for_url(&i.base_url, &|k| std::env::var(k).unwrap_or_default()));
    app.model = shown;
    app.device = device::name(&|k| std::env::var(k).unwrap_or_default());
    app.skills = krowk_harness::compat::skills::discover(&opts.host.permissions, &opts.host.cwd).into_iter().filter(|k| k.user_invocable).map(|k| (k.name, k.description)).collect();
    app.vendor_instances = opts.host.registry.instances.values().filter(|i| i.backend.is_some()).map(|i| i.name.clone()).collect();
    let branch = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "--no-optional-locks", "rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&opts.host.cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| b != "HEAD")
        .unwrap_or_default();
    let effort = opts.effort.and_then(|e| serde_json::to_value(e).ok()).and_then(|v| v.as_str().map(String::from));
    app.header(&home_relative(&opts.host.cwd), &branch, effort.as_deref());
    for n in &opts.notices {
        app.note(n);
    }
    if !opts.notices.is_empty() {
        app.say("", app::dim());
    }

    let initial_height = app.view(Instant::now().into_std()).0.len() as u16;
    let mut term = match Term::new(stdout, size, top, initial_height) {
        Ok(mut t) => {
            t.reflows = term::reflows_from(&|k| std::env::var(k).unwrap_or_default());
            t.pad = PAD;
            // Saved only once there is a TUI to put it back on the way out.
            let _ = std::io::stdout().write_all(term::TITLE_SAVE);
            t
        }
        Err(e) => return Outcome { session_id: None, abandoned: false, error: Some(format!("the terminal could not be drawn on: {e}")) },
    };
    let host = Host::new(opts.host);
    // Routed now, while the first frame is drawn: the vendors it asks
    // (a Node start for `claude`) never hold the prompt up.
    let routing: Option<RouteFuture<'_>> = opts.route.map(|r| {
        let (host, cwd) = (&host, runs_in.clone());
        Box::pin(async move { host.route_model(r.asked.as_ref(), r.current.as_ref(), &cwd).await }) as RouteFuture<'_>
    });
    let effort_label = effort.clone();
    let mut ui = Ui {
        host: &host,
        model: opts.model,
        chosen: opts.chosen,
        routing,
        model_route: None,
        held: None,
        needs_trust: None,
        trust_shown: None,
        trust: opts.trust,
        effort_label,
        runs_in: runs_in.clone(),
        permission_mode: opts.permission_mode, toolset: opts.toolset, effort: opts.effort, budget: opts.budget, target, keys: None, turn: None, rx: None, abandoned: false, last_prompt: String::new(), presence: presence::Presence::from_env(&|k| std::env::var(k).unwrap_or_default()) };
    let result = ui.run(&mut app, &mut term).await;
    // A turn still running is let go first: its future holds its backend's
    // lock, and would hold a shutdown waiting on it forever.
    ui.turn = None;
    ui.rx = None;
    // A backend's process (Claude Code, Codex) is let go cleanly, and
    // whatever it started with it, before the terminal is handed back —
    // unless the person left without waiting (a second Ctrl-C, SIGTERM),
    // when every backend's process group is killed at once instead.
    if ui.abandoned {
        krowk_harness::group::kill_all();
    } else {
        host.shutdown().await;
    }
    let _ = term.finish();
    ui.presence.finish();
    let mut out = term.into_inner();
    if let Some(id) = &app.session_id {
        let _ = write!(out, "\x1b[2mresume this session with: krowk --resume {id}\x1b[0m\r\n");
    }
    let _ = out.flush();
    Outcome { session_id: app.session_id.clone(), error: result.err().map(|e| e.to_string()), abandoned: ui.abandoned }
}

struct Ui<'h> {
    host: &'h Host,
    model: Option<ModelRef>,
    /// The model the person named (`--model`, a `/model` switch): with the
    /// session's own, what a bare `/model` id stays beside. A routed
    /// model is not one — it was krowk's pick, not the person's.
    chosen: Option<ModelRef>,
    /// The model being routed at start (`Options::route`).
    routing: Option<RouteFuture<'h>>,
    /// A `/model <id>` being routed: the loop goes on — keys, signals,
    /// resizes — while its vendors are asked.
    model_route: Option<RouteFuture<'h>>,
    /// Prompts sent before the route, or the trust question after it, was
    /// settled, joined as steering is: sent once they are.
    held: Option<String>,
    /// The routed model runs on a backend in a repository nobody trusted:
    /// the question to ask, once a prompt is actually sent for it.
    needs_trust: Option<(ModelRef, String)>,
    /// When the trust question came up: keys before `APPROVAL_SETTLE` has
    /// passed were typed ahead, and are no answer.
    trust_shown: Option<std::time::Instant>,
    /// The trust question, for a route that lands on a backend.
    trust: Option<TrustAsk>,
    /// The effort as the header shows it.
    effort_label: Option<String>,
    /// Where the session runs, where a vendor is asked once trusted.
    runs_in: PathBuf,
    permission_mode: PermissionMode,
    toolset: Option<String>,
    effort: Option<Effort>,
    budget: Option<BudgetLimits>,
    target: Option<Target>,
    keys: Option<EventStream>,
    turn: Option<TurnFuture<'h>>,
    rx: Option<mpsc::Receiver<StreamLine>>,
    /// Set to leave now, without the running turn's end.
    abandoned: bool,
    /// The last prompt sent: what a yes to a limit's offer sends again on
    /// the instance it moves to (R-INST-7).
    last_prompt: String,
    /// The window title and herdr's agent state.
    presence: presence::Presence,
}

/// SIGTERM and SIGHUP as one stream, registered once. Either asks the TUI
/// to stop the way Ctrl-D does: the running turn is interrupted and its end
/// waited for, the terminal restored, the session recorded; a second one
/// does not wait. Nothing on Windows, where neither is sent.
struct Hangups {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hup: Option<tokio::signal::unix::Signal>,
}

impl Hangups {
    fn new() -> Hangups {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Hangups { term: signal(SignalKind::terminate()).ok(), hup: signal(SignalKind::hangup()).ok() }
        }
        #[cfg(not(unix))]
        Hangups {}
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            async fn one(s: &mut Option<tokio::signal::unix::Signal>) {
                let got = match s {
                    Some(s) => s.recv().await.is_some(),
                    None => false,
                };
                if !got {
                    std::future::pending::<()>().await;
                }
            }
            tokio::select! {
                _ = one(&mut self.term) => {}
                _ = one(&mut self.hup) => {}
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await
    }
}

/// Where the terminal says the cursor is, asked after the key reader has
/// stopped (a resize, a job stop).
///
/// Not through crossterm's `cursor::position`: that fails at once when the
/// reader's wake-up is still pending, and the answer to the query it had
/// already written then waits in crossterm's queue, where the next ask —
/// at the next resize — takes it for the answer to its own, one resize
/// stale. So the reader is let settle (a zero-length wait for an event
/// takes the wake-up, and returns once nothing else holds the terminal's
/// input), and the query is written and its answer read here, on the
/// terminal itself, one query and one answer.
#[cfg(unix)]
fn cursor_row() -> Option<u16> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let _ = crossterm::event::poll(Duration::from_millis(20));
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    tty.write_all(b"\x1b[6n").ok()?;
    tty.flush().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut got = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        if let Some(row) = parse_cursor_report(&got) {
            return Some(row);
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || got.len() > 4096 {
            return None;
        }
        let mut pfd = libc::pollfd { fd: tty.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, owned here, for the length of the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(1000) as i32) };
        if ready <= 0 {
            continue;
        }
        match tty.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
}

#[cfg(not(unix))]
fn cursor_row() -> Option<u16> {
    let _ = crossterm::event::poll(Duration::ZERO);
    (0..2).find_map(|_| crossterm::cursor::position().ok()).map(|(_, y)| y)
}

/// Columns of padding on each side of everything the TUI draws.
const PAD: u16 = 2;

/// The width the app lays out in, inside the padding (none on a terminal
/// too narrow to spare it).
fn inner(width: u16) -> u16 {
    if width > 2 * PAD + 10 { width - 2 * PAD } else { width.max(1) }
}

/// Scrolls the rows above `top` off the screen into scrollback, with line
/// feeds from the bottom row, and leaves the cursor at the top left of the
/// now empty screen: the row returned.
fn clear_by_scrolling(out: &mut impl Write, size: Size, top: u16) -> u16 {
    if top == 0 {
        return 0;
    }
    let mut b = format!("\x1b[{};1H", size.height).into_bytes();
    b.extend(std::iter::repeat_n(b'\n', usize::from(top)));
    b.extend_from_slice(b"\x1b[1;1H");
    let _ = out.write_all(&b);
    let _ = out.flush();
    0
}

/// `~/…` for a directory under the home directory.
pub fn home_relative(dir: &std::path::Path) -> String {
    match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() && dir.starts_with(&home) => match dir.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => dir.display().to_string(),
        },
        _ => dir.display().to_string(),
    }
}

/// The row of the first `ESC [ row ; col R` in `bytes`, zero-based.
fn parse_cursor_report(bytes: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(bytes);
    let mut rest = text.as_ref();
    while let Some(i) = rest.find("\x1b[") {
        let tail = &rest[i + 2..];
        if let Some(end) = tail.find('R')
            && let Some((row, col)) = tail[..end].split_once(';')
            && let (Ok(row), Ok(_)) = (row.parse::<u16>(), col.parse::<u16>())
        {
            return Some(row.saturating_sub(1));
        }
        rest = tail;
    }
    None
}

/// When the running turn's clock and spinner next change: the first whole
/// `TICK` after `now`, counted from the turn's start, so it is always in
/// the future and the loop sleeps until then.
fn next_tick(started: Instant, now: Instant) -> Instant {
    let tick = app::TICK.as_millis().max(1);
    let n = now.saturating_duration_since(started).as_millis() / tick + 1;
    started + Duration::from_millis((n * tick) as u64)
}

/// The next of an optional stream, or never.
async fn next_key(keys: &mut Option<EventStream>) -> Option<std::io::Result<Event>> {
    match keys {
        Some(k) => std::future::poll_fn(|cx| Pin::new(&mut *k).poll_next(cx)).await,
        None => std::future::pending().await,
    }
}

async fn recv(rx: &mut Option<mpsc::Receiver<StreamLine>>) -> StreamLine {
    match rx {
        Some(r) => match r.recv().await {
            Some(l) => l,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

async fn finish<F: Future + Unpin>(f: &mut Option<F>) -> F::Output {
    match f {
        Some(f) => f.await,
        None => std::future::pending().await,
    }
}

async fn until(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

impl<'h> Ui<'h> {
    async fn run<W: Write>(&mut self, app: &mut App, term: &mut Term<W>) -> std::io::Result<()> {
        self.keys = Some(EventStream::new());
        let mut frame_at: Option<Instant> = None;
        let mut last_frame: Option<Instant> = None;
        // One probe at start, so a machine that is offline says so before
        // anybody types into it.
        let mut probe: Option<ProbeFuture> = self.target.clone().map(|t| Box::pin(async move { net::reachable(&t).await }) as ProbeFuture);
        let mut probe_at: Option<Instant> = None;
        let mut failures = 0u32;
        let mut last_activity = Instant::now();
        let mut stall_quiet_until: Option<Instant> = None;
        let mut quitting = false;
        let mut hangups = Hangups::new();
        self.draw(app, term)?;
        last_frame.replace(Instant::now());
        loop {
            let now = Instant::now();
            // Deadlines, only for what is actually pending.
            let tick_at = app.turn.as_ref().map(|t| next_tick(Instant::from_std(t.started), now));
            let stall_at = (app.waiting_on_model() && probe.is_none() && app.offline.is_none() && self.target.is_some())
                .then(|| (last_activity + STALL).max(stall_quiet_until.unwrap_or(now)));
            let retry_at = app.turn.as_ref().filter(|t| (t.want_interrupt && !t.interrupt_sent) || !app.unsent_steers.is_empty()).map(|_| now + INTERRUPT_RETRY);
            let wake = [frame_at, tick_at, stall_at, retry_at, probe_at].into_iter().flatten().min();
            tokio::select! {
                biased;
                _ = hangups.recv() => {
                    if !app.running() || quitting {
                        self.abandoned = app.running();
                        return Ok(());
                    }
                    quitting = true;
                    self.interrupt(app).await;
                }
                ev = next_key(&mut self.keys) => match ev {
                    Some(Ok(ev)) => {
                        if self.on_event(app, term, ev, &mut quitting).await? {
                            probe_at = Some(Instant::now());
                        }
                    }
                    // The terminal is gone: nobody is left to answer.
                    Some(Err(_)) | None => return Ok(()),
                },
                line = recv(&mut self.rx) => {
                    last_activity = Instant::now();
                    if matches!(&line, StreamLine::Live(krowk_harness::protocol::LiveEvent::ItemDelta { .. })) {
                        // Bytes are arriving from the model: it is reachable,
                        // and a retry probe still scheduled is moot.
                        app.set_online();
                        failures = 0;
                        probe_at = None;
                    }
                    app.on_line(&line);
                    self.follow(app);
                    self.flush_requests(app).await;
                }
                r = finish(&mut self.turn) => {
                    self.turn = None;
                    // What the host sent before answering is already queued.
                    if let Some(mut rx) = self.rx.take() {
                        while let Ok(line) = rx.try_recv() {
                            app.on_line(&line);
                        }
                    }
                    self.follow(app);
                    // What the engine never read, as the host says on the
                    // result (its own queue, so nothing is guessed), and what
                    // was never accepted at all.
                    let (acked, unsent) = app.end_turn_parts();
                    let mut left = match &r {
                        Ok(Some(res)) => res.unread_steers.clone(),
                        _ => acked,
                    };
                    left.extend(unsent);
                    let network = match &r {
                        Ok(Some(res)) => res.error.as_ref().is_some_and(|e| e.code == "network_unreachable"),
                        Ok(None) => false,
                        Err(e) => e.code == "network_unreachable",
                    };
                    let completed = matches!(&r, Ok(Some(res)) if res.status == TurnStatus::Completed);
                    if let Err(e) = r {
                        app.error(&e.info());
                    }
                    if network && probe.is_none() {
                        probe_at = Some(Instant::now());
                    }
                    if quitting {
                        return Ok(());
                    }
                    // Steering the turn never read: after an answer it is
                    // the next prompt, as it would have been the turn's next
                    // step. After an interrupt or a failure it is not sent
                    // on its own — it goes back into the prompt, to send,
                    // edit or drop.
                    if !left.is_empty() {
                        if completed {
                            self.prompt(app, left.join("\n\n"));
                        } else {
                            app.editor.restore(&left.join("\n\n"));
                            app.notice("the steering this turn never read is back in the prompt");
                        }
                    }
                }
                r = finish(&mut self.model_route) => {
                    self.model_route = None;
                    match r {
                        Ok(m) => {
                            self.switch(app, m).await;
                            self.release(app);
                        }
                        Err(e) => {
                            app.error(&e.info());
                            self.unhold(app);
                        }
                    }
                }
                r = finish(&mut self.routing) => {
                    self.routing = None;
                    if self.routed(app, r) && probe.is_none()
                        && let Some(t) = self.target.clone()
                    {
                        probe = Some(Box::pin(async move { net::reachable(&t).await }));
                    }
                }
                ok = finish(&mut probe) => {
                    probe = None;
                    if ok {
                        app.set_online();
                        failures = 0;
                        probe_at = None;
                    } else {
                        app.set_offline(self.target.as_ref().map(Target::label).unwrap_or_default());
                        probe_at = Some(Instant::now() + net::retry_after(failures));
                        failures += 1;
                    }
                }
                _ = until(wake) => {
                    let now = Instant::now();
                    if frame_at.is_some_and(|t| t <= now) {
                        frame_at = None;
                        self.draw(app, term)?;
                        last_frame = Some(now);
                    }
                    if tick_at.is_some_and(|t| t <= now) {
                        app.touch();
                    }
                    let stalled = stall_at.is_some_and(|t| t <= now);
                    if (stalled || probe_at.is_some_and(|t| t <= now))
                        && probe.is_none()
                        && let Some(t) = self.target.clone()
                    {
                        // A stall that proves reachable is not asked about
                        // again for a while: a model can think in silence.
                        if stalled {
                            stall_quiet_until = Some(now + STALL * 4);
                        }
                        probe_at = None;
                        probe = Some(Box::pin(async move { net::reachable(&t).await }));
                    }
                    if retry_at.is_some_and(|t| t <= now) {
                        self.flush_requests(app).await;
                    }
                }
            }
            if self.abandoned || (app.quit && self.turn.is_none()) {
                return Ok(());
            }
            if app.take_dirty() && frame_at.is_none() {
                let now = Instant::now();
                frame_at = Some(last_frame.map_or(now, |t| (t + FRAME).max(now)));
            }
            // A frame that is due is drawn here too, whichever branch woke
            // the loop: a stream that never pauses must not starve the screen.
            let now = Instant::now();
            if frame_at.is_some_and(|t| t <= now) {
                frame_at = None;
                self.draw(app, term)?;
                last_frame = Some(now);
            }
        }
    }

    fn draw<W: Write>(&mut self, app: &mut App, term: &mut Term<W>) -> std::io::Result<()> {
        // The resize event can trail the resize itself; a frame drawn for
        // the old size in between would land in the wrong rows. The size is
        // one ioctl away, so every frame asks.
        if let Ok((w, h)) = crossterm::terminal::size()
            && (w.max(1), h.max(1)) != (term.size().width, term.size().height)
        {
            self.resize(app, term, w, h)?;
        }
        let (mut rows, mut caret) = app.view(std::time::Instant::now());
        // A live region taller than the terminal keeps its bottom: the
        // prompt and the status bar, over whatever is streaming.
        let skip = rows.len().saturating_sub(usize::from(term.size().height));
        rows.drain(..skip);
        caret.1 = caret.1.saturating_sub(skip as u16);
        let lines = app.take_pending();
        let state = if !app.approvals.is_empty() {
            presence::State::Blocked
        } else if app.running() {
            presence::State::Working
        } else {
            presence::State::Idle
        };
        if let Some(title) = self.presence.update(state, &self.last_prompt) {
            term.title(&title)?;
        }
        if std::mem::take(&mut app.copy) {
            // What was shown, not what was sent: no escape or bidi control
            // reaches the place it is pasted.
            let text: String = app.answer.trim_end().split('\n').map(|l| card::clean(&l.replace('\t', "    "))).collect::<Vec<_>>().join("\n");
            app.flash = Some(if text.len() > clipboard::MAX {
                format!("the answer is too long to copy ({} KB)", text.len() / 1024)
            } else {
                term.clipboard(&text)?;
                clipboard::system(&text);
                format!("sent the last answer to the clipboard ({} lines)", text.lines().count())
            });
        }
        term.frame(&lines, &rows, caret)
    }

    /// A switch the stream brought — a rollover, a failed switch going
    /// back — is where the next prompt goes.
    fn follow(&mut self, app: &mut App) {
        if let Some(m) = app.switched.take() {
            self.retarget(&m);
            self.model = Some(m);
        }
    }

    /// The connectivity probe follows the model's host.
    fn retarget(&mut self, m: &ModelRef) {
        if let Ok(i) = self.host.registry().get(&m.instance) {
            self.target = Target::for_url(&i.base_url, &|k| std::env::var(k).unwrap_or_default());
        }
    }

    /// Moves the session to `m` (R-SWITCH-4): checked by the host first,
    /// and refused with its fix — the session staying where it was — when
    /// it cannot run there. During a turn it takes effect when the turn is
    /// over.
    async fn switch(&mut self, app: &mut App, m: ModelRef) -> bool {
        let (tx, _rx) = mpsc::channel(4);
        match self.host.execute(Command::SwitchModel { session_id: app.session_id.clone(), model: m.clone() }, tx).await {
            Ok(_) => {
                let when = if app.running() { " once this turn is over" } else { "" };
                app.gap_say(&format!("{}now on {m}{when}", look::SWITCH));
                self.retarget(&m);
                self.model = Some(m.clone());
                self.chosen = Some(m.clone());
                if self.needs_trust.as_ref().is_some_and(|(n, _)| *n != m) {
                    self.needs_trust = None;
                    app.trust_question = None;
                }
                if !app.running() {
                    app.model = Some(m);
                }
                true
            }
            Err(e) => {
                app.error(&e.info());
                false
            }
        }
    }

    /// The model routed at start, taken: shown, and followed by the
    /// connectivity probe. On a backend in a repository nobody has trusted,
    /// the trust question is kept for when a prompt is sent — asked now
    /// only if one is already held; nothing else asks it, so a key typed as
    /// the route lands is never its answer. A prompt held for the route
    /// goes now, or once the question is answered. True when the probe's
    /// target changed.
    fn routed(&mut self, app: &mut App, r: Result<ModelRef, EngineError>) -> bool {
        let m = match r {
            Ok(m) => m,
            Err(e) => {
                app.error(&e.info());
                self.unhold(app);
                return false;
            }
        };
        // A `/model` the person gave in the meantime wins.
        if self.model.is_some() {
            return self.release(app);
        }
        self.retarget(&m);
        self.model = Some(m.clone());
        app.header_model(&m, self.effort_label.as_deref());
        app.model = Some(m.clone());
        let backend = self.host.registry().get(&m.instance).ok().filter(|i| i.backend.is_some()).map(|i| i.vendor);
        if let (Some(vendor), Some(t)) = (backend, &self.trust)
            && !(t.trusted)()
        {
            match &t.refuses {
                Some(why) => app.notice(&format!("{m} runs {vendor}, which is started only in a trusted repository, and {} cannot be trusted for good — {why}. Run `krowk -p --trust` there for one run, or pick an API model with /model.", home_relative(&t.root))),
                None => {
                    let runs = krowk_harness::trust::what_runs(&t.root);
                    let has = if runs.is_empty() { "Nothing of that kind is there now.".to_string() } else { format!("It has {}.", runs.join(", ")) };
                    let q = format!("{m} runs {vendor}, which runs a repository's own hooks and MCP servers without asking. {has} Trust {}? y trusts it, n or esc does not", home_relative(&t.root));
                    self.needs_trust = Some((m, q));
                    if self.held.is_some() {
                        self.ask_trust(app);
                    }
                    return true;
                }
            }
        }
        self.release(app);
        true
    }

    /// Puts the trust question up, and notes when.
    fn ask_trust(&mut self, app: &mut App) {
        if let Some((_, q)) = &self.needs_trust {
            app.trust_question = Some(q.clone());
            self.trust_shown = Some(std::time::Instant::now());
        }
    }

    /// Whether the trust question stands between a prompt and its turn:
    /// the model is still the routed backend, and its repository still not
    /// trusted.
    fn trust_owed(&self) -> bool {
        match (&self.needs_trust, &self.trust) {
            (Some((m, _)), Some(t)) => self.model.as_ref() == Some(m) && !(t.trusted)(),
            _ => false,
        }
    }

    /// Sends the prompts held for the route, if there are any. Always
    /// false: nothing about the probe changes.
    fn release(&mut self, app: &mut App) -> bool {
        if let Some(text) = self.held.take() {
            self.prompt(app, text);
        }
        false
    }

    /// Puts the prompts held back into the editor, unsent: the route
    /// failed, the trust question was answered no, or Ctrl-C.
    fn unhold(&mut self, app: &mut App) {
        if let Some(text) = self.held.take() {
            let now = app.editor.text().to_string();
            app.editor.restore(&if now.trim().is_empty() { text } else { format!("{text}\n\n{now}") });
        }
    }

    /// Holds `text` until the model is routed and trusted, beside what is
    /// held already.
    fn hold(&mut self, app: &mut App, text: String, why: &str) {
        app.gap_say(why);
        self.held = Some(match self.held.take() {
            Some(h) => format!("{h}\n\n{text}"),
            None => text,
        });
    }

    fn prompt(&mut self, app: &mut App, text: String) {
        // Held until the model is routed and, on a backend, the repository
        // trusted: the turn would otherwise route it again, or be refused.
        if self.routing.is_some() || self.model_route.is_some() {
            return self.hold(app, text, "choosing the model… the prompt goes once it is chosen");
        }
        if app.trust_question.is_some() || self.trust_owed() {
            self.hold(app, text, "the prompt goes once the trust question is answered");
            if app.trust_question.is_none() {
                self.ask_trust(app);
            }
            return;
        }
        self.last_prompt = text.clone();
        app.offer = None;
        let (tx, rx) = mpsc::channel(1024);
        let cmd = Command::Prompt { session_id: app.session_id.clone(), text, model: self.model.clone(), permission_mode: self.permission_mode, toolset: self.toolset.clone(), effort: self.effort, budget: self.budget };
        self.turn = Some(Box::pin(self.host.execute(cmd, tx)));
        self.rx = Some(rx);
        app.start_turn(std::time::Instant::now());
    }

    /// An interrupt or steering the host could not take yet (the turn had
    /// not registered), asked again.
    async fn flush_requests(&mut self, app: &mut App) {
        let Some(id) = app.session_id.clone() else { return };
        let Some(t) = &app.turn else { return };
        if !t.prompt_seen {
            return;
        }
        if t.want_interrupt
            && !t.interrupt_sent
            && self.command(Command::Interrupt { session_id: id.clone() }).await.is_ok()
            && let Some(t) = &mut app.turn
        {
            t.interrupt_sent = true;
        }
        while let Some(text) = app.unsent_steers.first().cloned() {
            if self.command(Command::Steer { session_id: id.clone(), text: text.clone() }).await.is_err() {
                break;
            }
            app.unsent_steers.remove(0);
            app.steers.push(text);
        }
        app.touch();
    }

    async fn command(&self, cmd: Command) -> Result<(), EngineError> {
        let (tx, _rx) = mpsc::channel(1);
        self.host.execute(cmd, tx).await.map(|_| ())
    }

    /// One terminal event. True when it asks for a connectivity probe.
    async fn on_event<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, ev: Event, quitting: &mut bool) -> std::io::Result<bool> {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release && k.code == KeyCode::Char('z') && k.modifiers.contains(KeyModifiers::CONTROL) => self.suspend(app, term)?,
            Event::Key(k) if k.kind != KeyEventKind::Release => return Ok(self.on_key(app, k, quitting).await),
            Event::Paste(s) => {
                app.editor.insert_str(&s);
                app.touch();
            }
            Event::Resize(w, h) => self.resize(app, term, w, h)?,
            _ => {}
        }
        Ok(false)
    }

    /// The terminal changed size: where the cursor is now is asked, with
    /// the key reader stopped so the answer reaches us, and the live region
    /// is rebuilt from there.
    fn resize<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, w: u16, h: u16) -> std::io::Result<()> {
        self.keys = None;
        term.resize(Size { width: w.max(1), height: h.max(1) }, cursor_row())?;
        self.keys = Some(EventStream::new());
        app.set_width(inner(w));
        Ok(())
    }

    /// Ctrl-Z: raw mode swallows the terminal's own, so the job stop is done
    /// here — the live region cleared and the terminal given back, then
    /// SIGTSTP to ourselves. The shell has the terminal until `fg`; on
    /// SIGCONT the stop returns, and the TUI takes the terminal again and
    /// redraws where the cursor now is. A turn running keeps running: the
    /// engine is in this process, stopped with it.
    fn suspend<W: Write>(&mut self, app: &mut App, term: &mut Term<W>) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            self.keys = None;
            self.presence.pause();
            term.finish()?;
            restore_terminal();
            // SAFETY: raise only sends a signal to this process.
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
            crossterm::terminal::enable_raw_mode()?;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[?2004h");
            let _ = out.write_all(term::TITLE_SAVE);
            let _ = out.flush();
            let (w, h) = crossterm::terminal::size().unwrap_or((term.size().width, term.size().height));
            term.resume(Size { width: w.max(1), height: h.max(1) }, cursor_row())?;
            self.keys = Some(EventStream::new());
            app.set_width(inner(w));
        }
        #[cfg(not(unix))]
        let _ = (app, term);
        Ok(())
    }

    async fn on_key(&mut self, app: &mut App, k: KeyEvent, quitting: &mut bool) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        app.touch();
        app.flash = None;
        // A call waiting for the person's say takes the keys that answer it
        // (R-PERM-2): y once, s for the session, p for the project, n or
        // Esc no, v to print a request that was cut to fit (its y/s/p work
        // only after). Ctrl-C still interrupts the turn, which declines it too.
        if let Some(req) = app.approvals.first().cloned()
            && !ctrl
        {
            // A key already on its way when the request came up — the
            // person was typing — is not an answer.
            if app.approval_shown.is_some_and(|t| t.elapsed() < APPROVAL_SETTLE) {
                return false;
            }
            // A request cut to fit takes no allow until it is seen whole.
            let ready = app.approval_ready();
            if k.code == KeyCode::Char('v') {
                app.expand_approval();
                return false;
            }
            let decision = match k.code {
                KeyCode::Char('y') if ready => Some(ApprovalDecision::Allow),
                KeyCode::Char('s') if ready && !req.remember.is_empty() => Some(ApprovalDecision::AllowSession),
                KeyCode::Char('p') if ready && !req.remember.is_empty() => Some(ApprovalDecision::AllowProject),
                KeyCode::Char('n') | KeyCode::Esc => Some(ApprovalDecision::Deny),
                _ => None,
            };
            if let Some(d) = decision {
                app.answered(&req.request_id);
                if self.command(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: d }).await.is_err() {
                    app.notice("that approval was already answered, or its turn is over");
                }
            }
            return false;
        }
        // The Agents overlay takes the keys that move through it: select a
        // subagent, expand its line, interrupt it alone (R-SUB-2, R-SUB-3).
        if app.overlay == Overlay::Agents && !ctrl && !alt {
            match k.code {
                KeyCode::Up | KeyCode::Down => {
                    app.agent_move(if k.code == KeyCode::Up { -1 } else { 1 });
                    return false;
                }
                KeyCode::Enter => {
                    app.agent_toggle();
                    return false;
                }
                KeyCode::Char('x') => {
                    if let Some(id) = app.agent_selected_running()
                        && let Err(e) = self.command(Command::Interrupt { session_id: id }).await
                    {
                        app.notice(&e.message);
                    }
                    return false;
                }
                _ => {}
            }
        }
        // The trust question for a routed backend. It is answered by one
        // key on an empty prompt once it has been up for APPROVAL_SETTLE:
        // `y` trusts the repository and sends what was held; `n` or Esc
        // does not, and puts it back in the prompt, to be asked again on
        // the next send. Any other key — and any key typed ahead, before
        // the settle or onto text in the prompt — is no answer, and goes to
        // the prompt as it would.
        if app.trust_question.is_some() && !ctrl && !alt && app.editor.is_empty() && self.trust_shown.is_some_and(|t| t.elapsed() >= APPROVAL_SETTLE) {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    app.trust_question = None;
                    self.needs_trust = None;
                    if let Some(t) = &self.trust
                        && let Some(e) = (t.accept)()
                    {
                        app.notice(&e);
                    }
                    self.release(app);
                    return false;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    app.trust_question = None;
                    app.notice("not trusted, so nothing ran — send the prompt again to be asked again, or pick an API model with /model");
                    self.unhold(app);
                    return false;
                }
                _ => {}
            }
        }
        // A limit's offer (R-INST-7): one keystroke, y, and never taken
        // silently — anything else declines it, and a key that is not an
        // answer still does what it does.
        if let Some(offer) = app.offer.clone()
            && !ctrl
            && !app.running()
        {
            app.offer = None;
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if self.switch(app, offer.to).await && !self.last_prompt.is_empty() {
                        let text = self.last_prompt.clone();
                        self.prompt(app, text);
                    }
                    return false;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => return false,
                _ => {}
            }
        }
        // The `/` menu, while a command is being typed: the arrows choose,
        // tab completes, enter runs a command or completes a skill, esc
        // puts the menu away. Everything else goes to the prompt.
        if app.slash_open() && !ctrl && !alt {
            let found = help::slash(app.editor.text(), &app.skills);
            match k.code {
                KeyCode::Up => {
                    app.slash_at = app.slash_at.saturating_sub(1);
                    return false;
                }
                KeyCode::Down => {
                    app.slash_at = (app.slash_at + 1).min(found.len().saturating_sub(1));
                    return false;
                }
                KeyCode::Esc => {
                    app.slash_closed = true;
                    return false;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    let Some(s) = found.get(app.slash_at.min(found.len().saturating_sub(1))) else { return false };
                    app.editor.clear();
                    app.slash_at = 0;
                    if s.skill || k.code == KeyCode::Tab {
                        // A skill takes what follows it: the menu closes on
                        // the space, and the next enter sends it.
                        app.editor.insert_str(&format!("/{} ", s.name));
                        return false;
                    }
                    app.editor.insert_str(&format!("/{}", s.name));
                    return self.submit(app).await;
                }
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => app.slash_at = 0,
                _ => {}
            }
        }
        // The help menu takes the arrows, enter and esc while it is open;
        // everything typed goes to the prompt, and filters it.
        if app.overlay == Overlay::Keys && !ctrl && !alt {
            let found = help::filter(app.editor.text());
            match k.code {
                KeyCode::Up => {
                    app.help_at = app.help_at.saturating_sub(1);
                    return false;
                }
                KeyCode::Down => {
                    app.help_at = (app.help_at + 1).min(found.len().saturating_sub(1));
                    return false;
                }
                KeyCode::Esc => {
                    app.overlay = Overlay::None;
                    app.editor.clear();
                    return false;
                }
                KeyCode::Enter => {
                    app.overlay = Overlay::None;
                    app.editor.clear();
                    if let Some(entry) = found.get(app.help_at) {
                        match entry.action {
                            help::Action::Tell => {}
                            help::Action::Model => self.open_models(app),
                            help::Action::Todos => app.overlay = Overlay::Todos,
                            help::Action::Agents => app.overlay = Overlay::Agents,
                            help::Action::Details => app.overlay = Overlay::Details,
                            help::Action::Copy => copy(app),
                            help::Action::Interrupt => {
                                if app.running() {
                                    self.interrupt(app).await;
                                }
                            }
                            help::Action::Quit => app.quit = true,
                        }
                    }
                    return false;
                }
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => app.help_at = 0,
                _ => {}
            }
        }
        // The model picker takes the arrows and enter while it is open.
        if app.overlay == Overlay::Models && !ctrl {
            match k.code {
                KeyCode::Up => {
                    app.pick_at = app.pick_at.saturating_sub(1);
                    return false;
                }
                KeyCode::Down => {
                    app.pick_at = (app.pick_at + 1).min(app.picks.len().saturating_sub(1));
                    return false;
                }
                KeyCode::Enter => {
                    app.overlay = Overlay::None;
                    if let Some(p) = app.picks.get(app.pick_at).cloned() {
                        match p.model {
                            Some(model) => {
                                self.switch(app, ModelRef { instance: p.instance, model }).await;
                            }
                            // No model known for it: the id is the person's to type.
                            None => {
                                app.editor.clear();
                                app.editor.insert_str(&format!("/model {}/", p.instance));
                            }
                        }
                    }
                    return false;
                }
                _ => {}
            }
        }
        let e = &mut app.editor;
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if app.running() {
                    if *quitting || app.turn.as_ref().is_some_and(|t| t.want_interrupt) {
                        // A second Ctrl-C does not wait for the first — but
                        // still leaves through the front door: the terminal
                        // restored, the resume line printed, the session
                        // recorded, then exit 130.
                        self.abandoned = true;
                        return false;
                    }
                    self.interrupt(app).await;
                } else if !app.editor.is_empty() {
                    app.editor.clear();
                } else if self.held.is_some() || app.trust_question.is_some() {
                    // What is waiting on the route or the trust question
                    // comes back unsent; a second Ctrl-C then quits.
                    app.trust_question = None;
                    self.unhold(app);
                    app.notice("not sent — it is back in the prompt");
                } else {
                    app.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if e.is_empty() {
                    if app.running() {
                        *quitting = true;
                        self.interrupt(app).await;
                    } else {
                        app.quit = true;
                    }
                } else {
                    e.delete();
                }
            }
            KeyCode::Esc => {
                if app.overlay != Overlay::None {
                    app.overlay = Overlay::None;
                } else if app.running() {
                    self.interrupt(app).await;
                }
            }
            KeyCode::Char('y') if ctrl => copy(app),
            KeyCode::Char('o') if ctrl => app.overlay = if app.overlay == Overlay::Details { Overlay::None } else { Overlay::Details },
            KeyCode::Char('t') if ctrl => app.overlay = if app.overlay == Overlay::Todos { Overlay::None } else { Overlay::Todos },
            KeyCode::Char('g') if ctrl => app.overlay = if app.overlay == Overlay::Agents { Overlay::None } else { Overlay::Agents },
            KeyCode::F(1) => app.toggle_help(),
            KeyCode::Char('?') if e.is_empty() && !ctrl && !alt => app.toggle_help(),
            KeyCode::Enter if alt => e.insert('\n'),
            KeyCode::Char('j') if ctrl => e.insert('\n'),
            KeyCode::Enter => {
                if e.enter() {
                    return self.submit(app).await;
                }
            }
            KeyCode::Char('a') if ctrl => e.home(),
            KeyCode::Char('e') if ctrl => e.end(),
            KeyCode::Char('b') if ctrl => e.left(),
            KeyCode::Char('f') if ctrl => e.right(),
            KeyCode::Char('b') if alt => e.word_left(),
            KeyCode::Char('f') if alt => e.word_right(),
            KeyCode::Char('u') if ctrl => e.kill_to_start(),
            KeyCode::Char('k') if ctrl => e.kill_to_end(),
            KeyCode::Char('w') if ctrl => e.kill_word(),
            KeyCode::Char('h') if ctrl => e.backspace(),
            KeyCode::Backspace if alt || ctrl => e.kill_word(),
            KeyCode::Backspace => e.backspace(),
            KeyCode::Delete => e.delete(),
            KeyCode::Left if ctrl || alt => e.word_left(),
            KeyCode::Right if ctrl || alt => e.word_right(),
            KeyCode::Left => e.left(),
            KeyCode::Right => e.right(),
            KeyCode::Home => e.home(),
            KeyCode::End => e.end(),
            KeyCode::Up => e.up(),
            KeyCode::Down => e.down(),
            KeyCode::Tab => e.insert_str("    "),
            KeyCode::Char(c) if !ctrl => e.insert(c),
            _ => {}
        }
        if !app.editor.text().starts_with('/') {
            app.slash_closed = false;
        }
        false
    }

    fn open_models(&self, app: &mut App) {
        let instances: Vec<(String, &'static str)> = self.host.registry().instances.values().map(|i| (i.name.clone(), i.kind)).collect();
        app.open_picker(&instances);
    }

    async fn interrupt(&mut self, app: &mut App) {
        if let Some(t) = &mut app.turn {
            t.want_interrupt = true;
        }
        self.flush_requests(app).await;
    }

    /// Enter: a prompt when idle, steering while a turn runs. True when a
    /// connectivity probe is owed first — the notice is up and the person
    /// is trying again.
    async fn submit(&mut self, app: &mut App) -> bool {
        let text = app.editor.text().trim().to_string();
        if text.is_empty() {
            return false;
        }
        match text.as_str() {
            "/exit" | "/quit" => {
                app.editor.clear();
                app.quit = true;
                return false;
            }
            "/help" => {
                app.editor.clear();
                app.toggle_help();
                return false;
            }
            "/model" => {
                app.editor.clear();
                self.open_models(app);
                return false;
            }
            t if t.starts_with("/model ") => {
                app.editor.clear();
                // A bare id stays on the session's instance when that can
                // run it, else goes to the one instance ready here that
                // can; with several, the refusal lists them. The session's
                // instance is one it ran on, or the person named — never
                // one routed for it, which would make krowk's pick the
                // tie-break it refuses to make.
                let current = self.chosen.clone().or_else(|| app.session_id.as_ref().and(app.model.clone()));
                let cwd = self.runs_in.clone();
                // Routed as the loop goes on: its vendors' checks never hold
                // up keys, signals or a resize.
                match self.host.registry().read_model(&t["/model ".len()..]) {
                    Ok(asked) => {
                        let host = self.host;
                        self.model_route = Some(Box::pin(async move { host.route_model(Some(&asked), current.as_ref(), &cwd).await }));
                    }
                    Err(e) => app.notice(&format!("/model: {e}")),
                }
                return false;
            }
            _ => {}
        }
        let text = app.editor.take().trim_end().to_string();
        app.overlay = Overlay::None;
        app.slash_closed = false;
        if app.running() {
            app.unsent_steers.push(text);
            self.flush_requests(app).await;
            return false;
        }
        self.prompt(app, text);
        app.offline.is_some()
    }
}

/// Ctrl-Y: the last answer to the clipboard, on the next frame.
fn copy(app: &mut App) {
    if app.answer.trim().is_empty() {
        app.flash = Some("nothing to copy yet".into());
    } else {
        app.copy = true;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn r_perf_2_the_turn_clock_ticks_at_the_spinner_rate_and_never_in_the_past() {
        use tokio::time::Instant;
        let t0 = Instant::now();
        let tick = super::app::TICK;
        for ms in [0u64, 1, 124, 125, 126, 999, 1000, 5_001] {
            let now = t0 + std::time::Duration::from_millis(ms);
            let next = super::next_tick(t0, now);
            assert!(next > now, "at {ms} ms the next tick is in the future");
            assert!(next - now <= tick, "at {ms} ms it is at most one frame away");
        }
    }

    #[test]
    fn a_cursor_report_is_read_past_whatever_came_before_it() {
        assert_eq!(super::parse_cursor_report(b"\x1b[12;40R"), Some(11));
        assert_eq!(super::parse_cursor_report(b"ab\x1b[A\x1b[3;1R"), Some(2), "a key typed meanwhile is skipped");
        assert_eq!(super::parse_cursor_report(b"\x1b[3;1"), None, "not whole yet");
    }
}
