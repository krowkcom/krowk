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
//! - `syntax` — code in colour, in the terminal's own sixteen.
//! - `settings` — the status line's configuration (R-TUI-2).
//! - `device` — the `<user>/<host>` the status line opens with, read once.
//! - `pr` — the branch and its pull request, for the status line's second row.
//! - `net` — the connectivity probe behind the offline notice (R-OFF-1).
//! - `connect` — `/connect` and `/disconnect`: the harness's sign-in, asked
//!   through an overlay, and the first-run card.
//! - `link` — where the sessions run: the host daemon over its socket, by
//!   default, or a host in this process.
//! - `synced` — a session another machine runs, followed through sync:
//!   the third place a session runs, behind the same `link`.
//!
//! The loop is event-driven end to end (R-PERF-2): it sleeps in one
//! `select!` until a key, a frame of the stream, the turn's end or a
//! deadline it set itself wakes it, and it sets deadlines only while there
//! is something to time — a frame owed (at most one per 1/60 s, R-PERF-4),
//! a running turn's clock, an interrupt not yet taken, a probe. Idle, with
//! nothing running and the API reachable, it has none, and wakes for
//! nothing but a key.

pub mod app;
pub mod ask;
pub mod card;
pub mod clipboard;
pub mod connect;
pub mod link;
pub mod device;
pub mod pr;
pub mod presence;
pub mod editor;
pub mod help;
pub mod look;
pub mod net;
pub mod paste;
pub mod settings;
pub mod syntax;
#[cfg(unix)]
pub mod synced;
mod table;
pub mod term;

/// No sync where there is no daemon or relay (unix-only for now): the
/// types the TUI names, with nothing to build them.
#[cfg(not(unix))]
pub mod synced {
    use krowk_harness::engine::EngineError;
    use krowk_harness::protocol::{Command, LogEvent, RunResult, StreamLine};
    use tokio::sync::{broadcast, mpsc, oneshot};

    pub enum Options {}
    pub enum SyncLink {}
    pub enum Event {
        Host(bool),
        Path(String),
        Note(String),
        Turn(mpsc::Receiver<StreamLine>, oneshot::Receiver<Result<Option<RunResult>, EngineError>>),
    }
    pub struct Opened {
        pub link: SyncLink,
        pub id: String,
        pub history: Vec<LogEvent>,
        pub title: String,
        pub host: Option<String>,
    }
    pub async fn open(o: Options) -> Result<Opened, String> {
        match o {}
    }
    impl SyncLink {
        pub async fn execute(&self, _: Command, _: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
            match *self {}
        }
        pub fn watch(&self) -> broadcast::Receiver<StreamLine> {
            match *self {}
        }
        pub fn events(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
            match *self {}
        }
    }
}

use app::{App, Mark, Overlay};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editor::Editor;
use futures_core::Stream;
use krowk_harness::engine::EngineError;
use krowk_harness::host::{Host, HostConfig, Pricer};
use krowk_harness::instances::Asked;
use krowk_harness::log;
use krowk_harness::protocol::{ApprovalDecision, ApprovalRequest, BudgetLimits, Command, Effort, ModelRef, PermissionMode, RunResult, StreamLine, TurnStatus};
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
use tokio::sync::{broadcast, mpsc};
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
/// A control command sent and not answered in time: it was written to the
/// daemon, which takes it, so it is not sent again.
const SLOW: &str = "host_slow";
/// How long a control command sent from a key is waited for.
const COMMAND_WAIT: Duration = Duration::from_secs(2);
/// How long a Ctrl-C or Ctrl-D that would leave krowk waits for the second
/// that does.
const QUIT_CONFIRM: Duration = Duration::from_millis(1500);
/// How many earlier sessions `/sessions` lists.
const RESUMABLE: usize = 30;

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
    /// Sync is set up here and the person's device list has no recovery
    /// kit: the status line says so until one is made (canon, devices.md →
    /// Skippable, with a reminder).
    pub no_recovery_kit: bool,
    /// A newer release: a header row when it is due, and always a line in
    /// the details overlay (canon, harness.md → Update notice).
    pub update: Option<app::Update>,
    pub version: String,
    /// krowk's config.json, which `/connect` writes definitions into; none
    /// (no home directory) and `/connect` says so.
    pub config: Option<PathBuf>,
    /// The host daemon to run sessions in, started when none runs; none
    /// runs them in this process.
    pub daemon: Option<Daemon>,
    /// Brings a session's rows in krowk.db up to date from its log. Run on
    /// a thread of its own after each turn, while the person reads the
    /// answer, so leaving has little or nothing left to write.
    pub project: Option<Project>,
    /// A session another machine runs, to follow through sync (`krowk sync
    /// attach`, `krowk --resume` of a synced id): its history is drawn
    /// first, and prompts, approvals and interrupts go to its host.
    /// `resume`, `daemon` and `route` are left unset with it.
    pub sync: Option<synced::Options>,
}

/// How to reach the host daemon (`krowk_harness::daemon::ensure`).
pub struct Daemon {
    pub env: Box<dyn Fn(&str) -> String>,
    pub version: String,
    /// Starts it, detached, when none answers.
    pub spawn: Box<dyn Fn() -> Result<Option<std::process::Child>, String> + Send + Sync>,
}

pub type Project = Arc<dyn Fn(&str) + Send + Sync>;

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
    /// The sessions `/sessions` moved away from, which the caller projects
    /// too.
    pub left: Vec<String>,
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
        Err(e) => return Outcome { session_id: None, left: Vec::new(), abandoned: false, error: Some(format!("the async runtime could not start: {e}")) },
    };
    if let Err(e) = crossterm::terminal::enable_raw_mode() {
        return Outcome { session_id: None, left: Vec::new(), abandoned: false, error: Some(format!("the terminal could not be put in raw mode: {e}")) };
    }
    let hook = std::panic::take_hook();
    // A panic ends the TUI when it aborts the process (the release profile)
    // or is the TUI's own thread's; one on another thread that unwinds — a
    // sign-in's, a readiness check's — is that thread's alone, and the TUI
    // goes on with the terminal as it had it.
    let tui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if cfg!(panic = "abort") || std::thread::current().id() == tui_thread {
            restore_terminal();
        }
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
    let _ = out.write_all(term::KEYS_POP);
    let _ = out.write_all(b"\x1b[?2004l\x1b[?25h");
    let _ = out.flush();
}

type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<RunResult>, EngineError>> + 'a>>;
type ProbeFuture = Pin<Box<dyn Future<Output = bool>>>;
type RouteFuture<'a> = Pin<Box<dyn Future<Output = Result<ModelRef, EngineError>> + 'a>>;
type ChecksFuture = Pin<Box<dyn Future<Output = Vec<krowk_harness::readiness::Report>>>>;
type PrFuture = Pin<Box<dyn Future<Output = (String, Option<pr::Pr>)>>>;
type PasteFuture = Pin<Box<dyn Future<Output = paste::Pasted>>>;
type PasteJob = Box<dyn FnOnce() -> paste::Pasted + Send>;

async fn session(opts: Options) -> Outcome {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[?2004h");
    let _ = stdout.write_all(term::KEYS_PUSH);
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
    let started_in = opts.host.cwd.clone();
    let mut replayed_to: Option<String> = None;
    if let Some(id) = &opts.resume {
        match read_session(&sessions_dir, id) {
            Ok(events) => {
                replayed_to = events.last().map(|e| e.id.clone());
                runs_in = replay(&mut app, id, &events, &opts.host.registry).unwrap_or(runs_in);
                app.say(&format!("resumed session {id}"), app::dim());
            }
            Err(e) => return Outcome { session_id: None, left: Vec::new(), abandoned: false, error: Some(e) },
        }
    }
    // A synced session: its history as the chunks hold it, replayed as a
    // resumed session's log is, before the first frame.
    let mut opened = None;
    if let Some(o) = opts.sync {
        match synced::open(o).await {
            Ok(s) => {
                replay(&mut app, &s.id, &s.history, &opts.host.registry);
                app.session_id = Some(s.id.clone());
                app.log_dir = None;
                // Said once the relay says whether the host is there
                // (`App::say_attached`), in place of the header: a session
                // opening here, not one picked up mid-way.
                let title = app::clean(&s.title);
                // The name goes on the status line, which draws its text as given.
                app.sync = Some(app::Synced { name: s.host.as_deref().map(app::clean).unwrap_or_else(|| "the host".into()), title, ..app::Synced::default() });
                opened = Some(s.link);
            }
            Err(e) => return Outcome { session_id: None, left: Vec::new(), abandoned: false, error: Some(format!("the synced session could not be attached: {e}")) },
        }
    }
    // The model shown before the first turn names one: the one given
    // (`--model`, or the configured default that names its instance), else
    // the session's last. One still to be routed is shown once it is.
    let shown = opts.model.clone().or_else(|| app.model.clone());
    let target = shown.as_ref().and_then(|m| opts.host.registry.get(&m.instance).ok()).and_then(|i| Target::for_url(&i.base_url, &|k| std::env::var(k).unwrap_or_default()));
    app.model = shown;
    app.device = device::name(&|k| std::env::var(k).unwrap_or_default());
    app.no_recovery_kit = opts.no_recovery_kit;
    app.update = opts.update;
    app.skills = krowk_harness::compat::skills::discover(&opts.host.permissions, &opts.host.cwd).into_iter().filter(|k| k.user_invocable).map(|k| (k.name, k.description)).collect();
    app.vendor_instances = opts.host.registry.instances.values().filter(|i| i.backend.is_some()).map(|i| i.name.clone()).collect();
    let branch = pr::branch(&opts.host.cwd);
    let effort = opts.effort.and_then(|e| serde_json::to_value(e).ok()).and_then(|v| v.as_str().map(String::from));
    if app.sync.is_none() {
        app.header(&home_relative(&opts.host.cwd), &branch, effort.as_deref());
    }
    app.branch = branch;
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
        Err(e) => return Outcome { session_id: None, left: Vec::new(), abandoned: false, error: Some(format!("the terminal could not be drawn on: {e}")) },
    };
    let credentials = opts.host.credentials.clone();
    let permissions_cfg = opts.host.permissions.clone();
    let local = Host::new(opts.host);
    // The daemon first, when there is one to run the sessions in: a
    // session there outlives this terminal (R-HOST-1). One that cannot be
    // reached leaves them here, and says so.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut live: Vec<String> = Vec::new();
    let host = match (opened, opts.daemon) {
        (Some(client), _) => link::Link::Synced { host: local, client },
        (None, None) => link::Link::Local(local),
        #[cfg(not(unix))]
        (None, Some(_)) => link::Link::Local(local),
        #[cfg(unix)]
        (None, Some(d)) => match krowk_harness::daemon::remote::Remote::connect(d.env, started_in.clone(), d.version.clone(), true, d.spawn).await {
            Ok(client) => {
                if client.krowk_version() != d.version {
                    app.note(&format!("the host daemon (pid {}) runs krowk {}, and this is {} — `krowk host stop` once its sessions are done", client.pid(), client.krowk_version(), d.version));
                }
                // The sessions of this directory still running there, which
                // `/sessions <id>` follows again.
                if let Ok(st) = client.status().await {
                    live = st.sessions.into_iter().filter(|s| s.running).map(|s| s.session_id).collect();
                }
                link::Link::Remote { host: local, client }
            }
            Err(e) => {
                app.note(&format!("the host daemon could not be reached ({}) — this session runs in this process, and ends with it", e.message));
                link::Link::Local(local)
            }
        },
    };
    for id in &live {
        if opts.resume.as_deref() != Some(id.as_str()) && started_at(&sessions_dir, id).is_some_and(|cwd| cwd == runs_in) {
            app.note(&format!("session {id} is still running here — `/sessions {id}` follows it"));
        }
    }
    // Routed now, while the first frame is drawn: the vendors it asks
    // (a Node start for `claude`) never hold the prompt up.
    let routing: Option<RouteFuture<'_>> = opts.route.map(|r| {
        let (host, cwd) = (&host, runs_in.clone());
        Box::pin(async move { host.route_model(r.asked.as_ref(), r.current.as_ref(), &cwd).await }) as RouteFuture<'_>
    });
    let effort_label = effort.clone();
    let paths = opts.config.clone().map(|config| connect::Paths { config, credentials: credentials.clone() });
    let mut ui = Ui {
        paths,
        permissions_cfg,
        credentials,
        data_dir: sessions_dir.parent().map(PathBuf::from),
        sessions_dir,
        auth: None,
        suspended: false,
        checks: None,
        pr: None,
        pasting: None,
        pastes: Default::default(),
        looked_in: None,
        first_run: false,
        first_run_pending: false,
        host: &host,
        watch: host.watch(),
        sync_events: host.synced().and_then(|s| s.events()),
        live: live.clone(),
        model: opts.model,
        chosen: opts.chosen,
        routing,
        model_route: None,
        held: None,
        needs_trust: None,
        trust_shown: None,
        settings_shown: None,
        trust: opts.trust,
        effort_label,
        runs_in: runs_in.clone(),
        started_in,
        permission_mode: opts.permission_mode, toolset: opts.toolset, effort: opts.effort, budget: opts.budget, target, keys: None, turn: None, rx: None, abandoned: false, quit_armed: None, last_prompt: String::new(), presence: presence::Presence::from_env(&|k| std::env::var(k).unwrap_or_default()), project: opts.project };
    // A resumed session still running in the daemon is followed from where
    // its log left off: the turn so far, then live.
    if let Some(id) = opts.resume.clone()
        && live.contains(&id)
    {
        ui.reattach(&mut app, &id, replayed_to.clone());
    }
    let result = ui.run(&mut app, &mut term).await;
    ui.drain(&mut app);
    // A turn still running is let go first: its future holds its backend's
    // lock, and would hold a shutdown waiting on it forever.
    ui.turn = None;
    ui.rx = None;
    // The screen is left first, so leaving shows at once whatever is still
    // to be let go.
    let _ = term.finish();
    ui.presence.finish();
    let mut out = term.into_inner();
    if let Some(id) = &app.session_id {
        let _ = write!(out, "\x1b[2mresume this session with: krowk --resume {id}\x1b[0m\r\n");
    }
    let _ = out.flush();
    // A backend's process (Claude Code, Codex) is let go cleanly, and
    // whatever it started with it — unless the person left without waiting
    // (a second Ctrl-C, SIGTERM), when every backend's process group is
    // killed at once instead.
    if ui.abandoned {
        krowk_harness::group::kill_all();
    } else {
        host.shutdown().await;
    }
    app.left.retain(|id| app.session_id.as_ref() != Some(id));
    Outcome { session_id: app.session_id.clone(), left: std::mem::take(&mut app.left), error: result.err().map(|e| e.to_string()), abandoned: ui.abandoned }
}

struct Ui<'h> {
    host: &'h link::Link,
    /// Sessions running in the daemon when the TUI opened: `/sessions`
    /// follows one of them rather than only replaying its log.
    live: Vec<String>,
    /// Where `/connect` reads and writes; none without a config directory.
    paths: Option<connect::Paths>,
    /// krowk's provider credentials file, for the readiness marks.
    credentials: PathBuf,
    /// krowk's data directory, where a vendor is asked outside any
    /// repository (`readiness::neutral_dir`).
    data_dir: Option<PathBuf>,
    /// Where session logs live: what `/sessions` lists and replays.
    sessions_dir: PathBuf,
    /// A `/connect` or `/disconnect` running on its own thread: what it
    /// asks and tells, and its end.
    auth: Option<mpsc::UnboundedReceiver<connect::Msg>>,
    /// What the harness reads permission settings from: for `/settings`,
    /// which file's `defaultMode` a new session would start in.
    permissions_cfg: krowk_harness::permissions::Config,
    /// The terminal is a vendor login's (`claude auth login`) until it
    /// says it is done: nothing is drawn meanwhile.
    suspended: bool,
    /// The vendors' readiness checks behind the pickers' marks, off the
    /// runtime's thread: slow ones never hold up a key.
    checks: Option<ChecksFuture>,
    /// The branch checked out and its pull request, being read.
    pr: Option<PrFuture>,
    /// A paste being read and made ready, off the runtime's thread (the
    /// clipboard, or the files dropped), and the editor's mark for where it
    /// goes; the ones asked for after it, waiting their turn.
    pasting: Option<(Option<u64>, PasteFuture)>,
    pastes: std::collections::VecDeque<(u64, PasteJob)>,
    /// Where the agent was at work when that was last read.
    looked_in: Option<PathBuf>,
    /// The flow running is the first-run card's.
    first_run: bool,
    /// The marks being checked decide whether the first-run card opens.
    first_run_pending: bool,
    /// What the host says between turns: a backend's agents, and a turn it
    /// began by itself, which the TUI runs (`continue`).
    watch: broadcast::Receiver<StreamLine>,
    /// A synced session's news besides its lines: a host there or not, the
    /// path, a turn begun elsewhere.
    sync_events: Option<mpsc::UnboundedReceiver<synced::Event>>,
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
    /// When `/settings` opened: a key before `APPROVAL_SETTLE` has passed
    /// was typed ahead, and changes nothing.
    settings_shown: Option<std::time::Instant>,
    /// The trust question, for a route that lands on a backend.
    trust: Option<TrustAsk>,
    /// The effort as the header shows it.
    effort_label: Option<String>,
    /// Where the session runs, where a vendor is asked once trusted.
    runs_in: PathBuf,
    /// Where krowk was started, where a new session would start: a resumed
    /// one runs in its own directory.
    started_in: PathBuf,
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
    /// The key (`c` or `d`, with Ctrl) that would have left krowk, and
    /// when: the same key again within `QUIT_CONFIRM` leaves, any other
    /// key forgets it.
    quit_armed: Option<(char, Instant)>,
    /// The last prompt sent: what a yes to a limit's offer sends again on
    /// the instance it moves to (R-INST-7).
    last_prompt: String,
    /// The window title and herdr's agent state.
    presence: presence::Presence,
    project: Option<Project>,
}

/// Two signals as one stream, registered once for the TUI's life.
///
/// SIGTERM and SIGHUP (`Signals::hangups`) ask the TUI to stop the way
/// Ctrl-D does: the running turn is interrupted and its end waited for, the
/// terminal restored, the session recorded; a second one does not wait.
/// With the sessions in the host daemon it stops at once, and the turn
/// runs on there.
///
/// SIGINT and SIGQUIT (`Signals::interrupts`) reach krowk only while it has
/// given the terminal up — raw mode off — to a vendor's login or a key's
/// command: Ctrl-C there is the person's to that command, and must not end
/// krowk with it. A handler, not an ignore, so the command, which starts
/// with every handled signal back at its default, still stops on it.
/// Nothing on Windows, where none of them is sent this way.
struct Signals {
    #[cfg(unix)]
    a: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    b: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn hangups() -> Signals {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Signals { a: signal(SignalKind::terminate()).ok(), b: signal(SignalKind::hangup()).ok() }
        }
        #[cfg(not(unix))]
        Signals {}
    }

    fn interrupts() -> Signals {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Signals { a: signal(SignalKind::interrupt()).ok(), b: signal(SignalKind::quit()).ok() }
        }
        #[cfg(not(unix))]
        Signals {}
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
                _ = one(&mut self.a) => {}
                _ = one(&mut self.b) => {}
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

/// The next of a synced session's events, or never; none once its link is
/// gone.
async fn recv_sync(rx: &mut Option<mpsc::UnboundedReceiver<synced::Event>>) -> Option<synced::Event> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// The next of a sign-in's messages, or never; none once its thread is gone.
async fn recv_auth(rx: &mut Option<mpsc::UnboundedReceiver<connect::Msg>>) -> Option<connect::Msg> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// The next frame the host sent between turns; one missed while the loop
/// was busy is only a list that a later frame repeats whole.
async fn watched(rx: &mut broadcast::Receiver<StreamLine>) -> StreamLine {
    loop {
        match rx.recv().await {
            Ok(l) => return l,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
        }
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

/// A probe of whether `t` can be reached.
fn probe(t: Target) -> ProbeFuture {
    Box::pin(async move { net::reachable(&t).await })
}

/// When the next frame is drawn, and when the last was.
#[derive(Default)]
struct Frames {
    at: Option<Instant>,
    last: Option<Instant>,
}

/// When what the event loop waits on besides its sources falls due.
struct Due {
    tick: Option<Instant>,
    stall: Option<Instant>,
    retry: Option<Instant>,
    quit: Option<Instant>,
}

/// Whether the model's provider can be reached, as the loop finds out:
/// the probe running, when the next is due, how many failed in a row,
/// and when the stream last brought anything.
struct Reach {
    probe: Option<ProbeFuture>,
    probe_at: Option<Instant>,
    failures: u32,
    last_activity: Instant,
    stall_quiet_until: Option<Instant>,
}

impl Reach {
    fn online(&mut self, app: &mut App) {
        app.set_online();
        self.failures = 0;
        self.probe_at = None;
    }

    /// A probe's answer.
    fn probed(&mut self, app: &mut App, ok: bool, target: Option<&Target>) {
        if ok {
            self.online(app);
        } else {
            app.set_offline(target.map(Target::label).unwrap_or_default());
            self.probe_at = Some(Instant::now() + net::retry_after(self.failures));
            self.failures += 1;
        }
    }
}

impl<'h> Ui<'h> {
    /// What the host already sent, read before leaving: keys go ahead of
    /// the stream in the loop, so a second Ctrl-C pressed just after a
    /// prompt can beat the session's start to the TUI, and the resume line
    /// would name no session although the host has recorded one.
    fn drain(&mut self, app: &mut App) {
        if let Some(rx) = self.rx.as_mut() {
            while let Ok(line) = rx.try_recv() {
                app.on_line(&line);
            }
        }
    }

    async fn run<W: Write>(&mut self, app: &mut App, term: &mut Term<W>) -> std::io::Result<()> {
        self.keys = Some(EventStream::new());
        let mut frames = Frames::default();
        // One probe at start, so a machine that is offline says so before
        // anybody types into it.
        let mut reach = Reach { probe: self.target.clone().map(probe), probe_at: None, failures: 0, last_activity: Instant::now(), stall_quiet_until: None };
        let mut quitting = false;
        let mut hangups = Signals::hangups();
        let mut interrupts = Signals::interrupts();
        self.first_frame(app, term, &mut frames)?;
        loop {
            let due = self.due(app, &reach);
            let wake = [frames.at, due.tick, due.stall, due.retry, reach.probe_at, due.quit].into_iter().flatten().min();
            tokio::select! {
                biased;
                // Ctrl-C or Ctrl-\ on the terminal a vendor's login has: that
                // command's, not krowk's (with the TUI's own terminal raw,
                // they arrive as keys instead). One can land a moment after
                // the terminal is back, so none stops the TUI; SIGTERM and
                // SIGHUP are what ask it to from outside.
                _ = interrupts.recv() => {}
                _ = hangups.recv() => {
                    if self.on_hangup(app, &mut quitting).await {
                        return Ok(());
                    }
                }
                ev = next_key(&mut self.keys) => match ev {
                    Some(Ok(ev)) => {
                        if self.on_event(app, term, ev, &mut quitting).await? {
                            reach.probe_at = Some(Instant::now());
                        }
                    }
                    // The terminal is gone: nobody is left to answer.
                    Some(Err(_)) | None => return Ok(()),
                },
                line = recv(&mut self.rx) => self.on_stream_line(app, &line, &mut reach).await,
                line = watched(&mut self.watch) => {
                    app.on_line(&line);
                    self.go_on(app);
                }
                e = recv_sync(&mut self.sync_events) => self.on_sync(app, e),
                r = finish(&mut self.turn) => {
                    if self.on_turn_end(app, r, &mut reach, quitting).await {
                        return Ok(());
                    }
                }
                r = finish(&mut self.model_route) => self.on_model_route(app, r).await,
                m = recv_auth(&mut self.auth) => self.on_auth(app, term, m)?,
                p = finish_paste(&mut self.pasting) => self.on_pasted(app, p),
                (branch, found) = finish(&mut self.pr) => self.on_pr(app, branch, found),
                reports = finish(&mut self.checks) => {
                    self.checks = None;
                    self.marked(app, reports);
                }
                r = finish(&mut self.routing) => self.on_routing(app, r, &mut reach),
                ok = finish(&mut reach.probe) => {
                    reach.probe = None;
                    reach.probed(app, ok, self.target.as_ref());
                }
                _ = until(wake) => self.on_wake(app, term, &due, &mut frames, &mut reach).await?,
            }
            if self.abandoned || (app.quit && self.turn.is_none()) {
                return Ok(());
            }
            self.paint(app, term, &mut frames)?;
        }
    }

    /// The first frame, and what is asked behind it.
    fn first_frame<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, frames: &mut Frames) -> std::io::Result<()> {
        self.draw(app, term)?;
        frames.last.replace(Instant::now());
        // A model known at start needs nothing routed; whether anything
        // here can run it at all is asked now, behind the first frame.
        // A synced session runs on its host's models: nothing here to ask
        // about, and no first-run card over it on a machine with none.
        if self.routing.is_none() && self.host.synced().is_none() {
            self.startup_sweep(app);
        }
        self.look_for_pr(app);
        Ok(())
    }

    /// After whatever woke the loop: a frame for what changed, drawn when
    /// due.
    fn paint<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, frames: &mut Frames) -> std::io::Result<()> {
        if app.take_dirty() && frames.at.is_none() {
            let now = Instant::now();
            frames.at = Some(frames.last.map_or(now, |t| (t + FRAME).max(now)));
        }
        // A frame that is due is drawn here too, whichever branch woke
        // the loop: a stream that never pauses must not starve the screen.
        self.draw_due(app, term, frames, Instant::now())
    }

    /// Deadlines, only for what is actually pending.
    fn due(&self, app: &App, reach: &Reach) -> Due {
        let now = Instant::now();
        Due {
            tick: app.turn.as_ref().map(|t| next_tick(Instant::from_std(t.started), now)),
            stall: (app.waiting_on_model() && reach.probe.is_none() && app.offline.is_none() && self.target.is_some())
                .then(|| (reach.last_activity + STALL).max(reach.stall_quiet_until.unwrap_or(now))),
            retry: app.turn.as_ref().filter(|t| (t.want_interrupt && !t.interrupt_sent) || !app.unsent_steers.is_empty()).map(|_| now + INTERRUPT_RETRY),
            quit: self.quit_armed.map(|(_, t)| t + QUIT_CONFIRM),
        }
    }

    /// SIGHUP or SIGTERM; true when the TUI stops now.
    async fn on_hangup(&mut self, app: &mut App, quitting: &mut bool) -> bool {
        // A turn in the daemon is the daemon's: the terminal
        // going takes nothing with it, and it runs on for
        // `krowk --resume` to follow (R-HOST-1).
        if self.host.remote().is_some() || self.host.synced().is_some() {
            return true;
        }
        if !app.running() || *quitting {
            self.abandoned = app.running();
            return true;
        }
        *quitting = true;
        self.interrupt(app).await;
        false
    }

    /// A line of the turn's stream.
    async fn on_stream_line(&mut self, app: &mut App, line: &StreamLine, reach: &mut Reach) {
        reach.last_activity = Instant::now();
        if matches!(line, StreamLine::Live(krowk_harness::protocol::LiveEvent::ItemDelta { .. })) {
            // Bytes are arriving from the model: it is reachable,
            // and a retry probe still scheduled is moot.
            reach.online(app);
        }
        app.on_line(line);
        self.follow(app);
        self.follow_the_agent(app);
        self.flush_requests(app).await;
    }

    /// The turn's end, `r` as the host answered it; true when the TUI
    /// stops now, `quitting`.
    async fn on_turn_end(&mut self, app: &mut App, r: Result<Option<RunResult>, EngineError>, reach: &mut Reach, quitting: bool) -> bool {
        self.turn = None;
        if let Some(n) = self.host.take_note() {
            app.note(&n);
        }
        // What the host sent before answering is already queued.
        if let Some(mut rx) = self.rx.take() {
            while let Ok(line) = rx.try_recv() {
                app.on_line(&line);
            }
        }
        self.follow(app);
        if let (Some(project), Some(id)) = (&self.project, &app.session_id) {
            let (project, id) = (project.clone(), id.clone());
            std::thread::spawn(move || project(&id));
        }
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
        let refused_images = matches!(&r, Err(e) if e.code == "model_reads_no_images" || e.code == "bad_image");
        // A `continue` whose turn a prompt already ran is no
        // news.
        if let Err(e) = r
            && e.code != "nothing_pending"
        {
            app.error(&e.info());
        }
        if network && reach.probe.is_none() {
            reach.probe_at = Some(Instant::now());
        }
        self.look_for_pr(app);
        if quitting {
            return true;
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
        // A prompt refused for its images goes back, images and
        // all, to switch the model or drop them.
        if refused_images {
            app.editor.restore(&self.last_prompt);
        }
        self.forget_images(app);
        self.go_on(app);
        false
    }

    /// The model asked for at start, routed or not; a probe follows a
    /// route that took.
    fn on_routing(&mut self, app: &mut App, r: Result<ModelRef, EngineError>, reach: &mut Reach) {
        self.routing = None;
        if self.routed(app, r) && reach.probe.is_none()
            && let Some(t) = self.target.clone()
        {
            reach.probe = Some(probe(t));
        }
    }

    /// A `/model <id>` routed, or not.
    async fn on_model_route(&mut self, app: &mut App, r: Result<ModelRef, EngineError>) {
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

    /// The branch and its pull request, as read.
    fn on_pr(&mut self, app: &mut App, branch: String, found: Option<pr::Pr>) {
        self.pr = None;
        if app.branch != branch || app.pr != found {
            app.branch = branch;
            app.pr = found;
            app.touch();
        }
        // The agent moved on while this was read.
        self.follow_the_agent(app);
    }

    /// The loop woke for a deadline: whichever are due are seen to.
    async fn on_wake<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, due: &Due, frames: &mut Frames, reach: &mut Reach) -> std::io::Result<()> {
        let now = Instant::now();
        self.draw_due(app, term, frames, now)?;
        if due.tick.is_some_and(|t| t <= now) {
            app.touch();
        }
        let stalled = due.stall.is_some_and(|t| t <= now);
        if (stalled || reach.probe_at.is_some_and(|t| t <= now))
            && reach.probe.is_none()
            && let Some(t) = self.target.clone()
        {
            // A stall that proves reachable is not asked about
            // again for a while: a model can think in silence.
            if stalled {
                reach.stall_quiet_until = Some(now + STALL * 4);
            }
            reach.probe_at = None;
            reach.probe = Some(probe(t));
        }
        if due.retry.is_some_and(|t| t <= now) {
            self.flush_requests(app).await;
        }
        // The second press never came: its hint goes.
        if due.quit.is_some_and(|t| t <= now) {
            self.quit_armed = None;
            app.flash = None;
            app.touch();
        }
        Ok(())
    }

    /// Draws the frame due by `now`, if one is.
    fn draw_due<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, frames: &mut Frames, now: Instant) -> std::io::Result<()> {
        if frames.at.is_some_and(|t| t <= now) {
            frames.at = None;
            self.draw(app, term)?;
            frames.last = Some(now);
        }
        Ok(())
    }

    fn draw<W: Write>(&mut self, app: &mut App, term: &mut Term<W>) -> std::io::Result<()> {
        // A vendor's login has the terminal: what is owed waits for it.
        if self.suspended {
            return Ok(());
        }
        // The resize event can trail the resize itself; a frame drawn for
        // the old size in between would land in the wrong rows. The size is
        // one ioctl away, so every frame asks.
        if let Ok((w, h)) = crossterm::terminal::size()
            && (w.max(1), h.max(1)) != (term.size().width, term.size().height)
        {
            self.resize(app, term, w, h)?;
        }
        // Scrollback's lines first: what they let go of (held tool blocks)
        // is then not in the live region too.
        let lines = app.take_pending();
        let (mut rows, mut caret) = app.view(std::time::Instant::now());
        // A live region taller than the terminal keeps its bottom: the
        // prompt and the status bar, over whatever is streaming.
        let skip = rows.len().saturating_sub(usize::from(term.size().height));
        rows.drain(..skip);
        caret.1 = caret.1.saturating_sub(skip as u16);
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
        if std::mem::take(&mut app.wipe) {
            term.wipe()?;
        }
        if let Some((what, text)) = app.copy.take() {
            // As written, tabs and all, but no escape or bidi control
            // reaches the place it is pasted.
            // The joiner that makes one emoji of several is kept too.
            let text: String = text.chars().filter(|&c| matches!(c, '\n' | '\t' | '\u{200D}') || (!c.is_control() && !card::is_bidi(c))).collect();
            app.flash = Some(if text.len() > clipboard::MAX {
                format!("{what} is too long to copy ({} KB)", text.len() / 1024)
            } else {
                term.clipboard(&text)?;
                clipboard::system(&text);
                format!("copied {what} ({} lines)", text.lines().count())
            });
        }
        term.steady(app.running())?;
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
                app.say_switched(&format!("now on {m}{when}"));
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

    /// `/settings`: config.json's settings as they are now, in an overlay.
    fn open_settings(&mut self, app: &mut App) {
        let Some(paths) = &self.paths else {
            app.notice("/settings: no config.json to save to — krowk has no home directory");
            return;
        };
        match krowk_harness::connect::read_config(&paths.config) {
            Ok(raw) => {
                self.show_settings(app, &raw);
                app.setting_at = 0;
                app.overlay = Overlay::Settings;
                self.settings_shown = Some(std::time::Instant::now());
            }
            Err(e) => app.notice(&format!("/settings: {e}")),
        }
    }

    /// config.json's settings as the overlay shows them, and what a file
    /// read after it overrides.
    fn show_settings(&self, app: &mut App, raw: &serde_json::Map<String, serde_json::Value>) {
        app.default_mode = settings::default_mode(raw);
        app.default_mode_overridden = settings::overridden(&self.permissions_cfg, raw, &self.started_in).map(|m| (m, settings::claude_file(&self.permissions_cfg)));
        app.touch();
    }

    /// The chosen setting one along its values, saved at once; at the end,
    /// nothing. The default permission mode leaves the session in the mode
    /// it runs in — `/mode` changes that — and the content width applies
    /// from the next frame.
    fn step_setting(&mut self, app: &mut App, by: isize) {
        let Some(paths) = &self.paths else { return };
        let width = if app.setting_at == 1 { app.content_width().step(by) } else { None };
        let mode = if app.setting_at == 0 { settings::step_default(app.default_mode.as_deref(), by) } else { None };
        if width.is_none() && mode.is_none() {
            return;
        }
        // A `/connect` writes config.json too, from its own thread: one at
        // a time, so neither loses what the other wrote.
        if self.auth.is_some() {
            app.flash = Some("a /connect is running — change settings once it is done".into());
            return;
        }
        let saved = match (mode, width) {
            (Some(m), _) => settings::set_default_mode(&paths.config, m),
            (_, Some(w)) => settings::set_content_width(&paths.config, w).inspect(|_| app.set_content_width(w)),
            _ => return,
        };
        match saved {
            Ok(raw) => self.show_settings(app, &raw),
            Err(e) => {
                app.overlay = Overlay::None;
                app.notice(&format!("/settings: {e}"));
            }
        }
    }

    /// Every prompt from here on runs in `m`. A turn already running keeps
    /// the mode it started in, its gate built with it; so does a turn a
    /// backend begins by itself, in the mode its process is in.
    fn set_mode(&mut self, app: &mut App, m: PermissionMode) {
        self.permission_mode = m;
        app.permission_mode = m.name().into();
        let when = if app.running() { " once this turn is over" } else { "" };
        app.gap_say(&format!("permission mode {}{when}", m.name()));
    }

    /// The model routed at start, taken: shown, and followed by the
    /// connectivity probe. On a backend in a repository nobody has trusted,
    /// the trust question is kept for when a prompt is sent — asked now
    /// only if one is already held; nothing else asks it, so a key typed as
    /// the route lands is never its answer. A prompt held for the route
    /// goes now, or once the question is answered. True when the probe's
    /// target changed.
    fn routed(&mut self, app: &mut App, r: Result<ModelRef, EngineError>) -> bool {
        // The header's `Update:` row waits to go under the routed model's;
        // with no such row coming, it goes now.
        if r.is_err() || self.model.is_some() {
            app.header_update();
        }
        let m = match r {
            // Nothing here can run a model: the first run. Connecting one
            // is offered instead of the failure the first prompt would be.
            Err(e) if e.code == "none_ready" && self.model.is_none() && app.overlay == Overlay::None && self.auth.is_none() => {
                self.unhold(app);
                self.open_flow(app, connect::Job::Connect(None), true);
                return false;
            }
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
        if self.owe_trust(app, &m) {
            return true;
        }
        self.release(app);
        true
    }

    /// On a backend in a repository nobody has trusted, the trust question
    /// is kept for when a prompt is sent — asked now only if one is already
    /// held. True when it is owed.
    fn owe_trust(&mut self, app: &mut App, m: &ModelRef) -> bool {
        let backend = self.host.registry().get(&m.instance).ok().filter(|i| i.backend.is_some()).map(|i| i.vendor);
        if let (Some(vendor), Some(t)) = (backend, &self.trust)
            && !(t.trusted)()
        {
            match &t.refuses {
                Some(why) => app.notice(&format!("{m} runs {vendor}, which is started only in a trusted repository, and {} cannot be trusted for good — {why}. Run `krowk -p --trust` there for one run, or pick an API model with /model.", home_relative(&t.root))),
                None => {
                    let runs = krowk_harness::trust::what_runs(&t.root);
                    let has = if runs.is_empty() { "Nothing of that kind is there now.".to_string() } else { format!("It has {}.", runs.join(", ")) };
                    let q = format!("{m} runs {vendor}, which runs a repository's own hooks and MCP servers without asking. {has} Trust {}? `y` trusts it, `n` or `esc` does not", home_relative(&t.root).replace('`', "'"));
                    self.needs_trust = Some((m.clone(), q));
                    if self.held.is_some() {
                        self.ask_trust(app);
                    }
                    return true;
                }
            }
        }
        false
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

    /// A synced session's news: the status line's host and path, a note,
    /// or a turn begun elsewhere, followed as the TUI's own — so Esc
    /// interrupts it, as `reattach` does a daemon's.
    fn on_sync(&mut self, app: &mut App, e: Option<synced::Event>) {
        let Some(e) = e else {
            self.sync_events = None;
            return;
        };
        let status = app.sync.get_or_insert_with(app::Synced::default);
        match e {
            synced::Event::Host(h) => {
                status.host = Some(h);
                app.say_attached();
            }
            synced::Event::Path(p) => status.path = Some(p),
            synced::Event::Note(n) => app.notice(&n),
            synced::Event::Turn(rx, done) => {
                if self.turn.is_some() {
                    return;
                }
                self.turn = Some(Box::pin(async move { done.await.unwrap_or(Ok(None)) }));
                self.rx = Some(rx);
                app.start_turn(std::time::Instant::now());
                if let Some(t) = &mut app.turn {
                    t.prompt_seen = true;
                }
            }
        }
        app.touch();
    }

    /// What a viewer of a synced session cannot do: the host runs it on its
    /// own model and settings, and remote prompts no further than its
    /// default mode (plan, if the session is in plan). Said, and true, when
    /// this is one.
    fn synced_refuses(&self, app: &mut App, what: &str) -> bool {
        if self.host.synced().is_none() {
            return false;
        }
        app.notice(&format!("{what} is the host's to change: this session runs on another machine, and prompts from here run in its default mode (plan, if it is in plan)"));
        true
    }

    /// Follows `id`'s turn running in the daemon as though this TUI had
    /// sent it: what its log has after what was replayed, the turn so far,
    /// then live, to its result.
    /// `after` is the last event the replay drew: what the log gained since
    /// comes from the daemon, never read here a second time.
    fn reattach(&mut self, app: &mut App, id: &str, after: Option<String>) {
        let Some(client) = self.host.remote() else { return };
        let (tx, rx) = mpsc::channel(1024);
        let id = id.to_string();
        app.gap_say(&format!("session {id} is running in the host daemon — following it"));
        self.turn = Some(Box::pin(async move { client.follow(&id, after.as_deref(), tx).await }));
        self.rx = Some(rx);
        app.start_turn(std::time::Instant::now());
        // Its prompt is in the log already: an interrupt or steering goes
        // straight to it.
        if let Some(t) = &mut app.turn {
            t.prompt_seen = true;
        }
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
        app.echo(&text);
        let (tx, rx) = mpsc::channel(1024);
        let images = app.images_for(&text);
        let cmd = Command::Prompt { session_id: app.session_id.clone(), text, images, model: self.model.clone(), permission_mode: self.permission_mode, toolset: self.toolset.clone(), effort: self.effort, budget: self.budget };
        self.turn = Some(Box::pin(self.host.execute(cmd, tx)));
        self.rx = Some(rx);
        app.start_turn(std::time::Instant::now());
    }

    /// A turn the backend began by itself runs as soon as none of the
    /// TUI's own does. A prompt sent first runs it ahead of itself, and a
    /// `continue` left over is then refused quietly (`nothing_pending`).
    fn go_on(&mut self, app: &mut App) {
        if self.turn.is_some() || !std::mem::take(&mut app.unprompted) {
            return;
        }
        let Some(session_id) = app.session_id.clone() else { return };
        let (tx, rx) = mpsc::channel(1024);
        self.turn = Some(Box::pin(self.host.execute(Command::Continue { session_id, budget: self.budget }, tx)));
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
            && self.command(Command::Interrupt { session_id: id.clone() }).await.is_ok_or_slow()
            && let Some(t) = &mut app.turn
        {
            t.interrupt_sent = true;
        }
        while let Some(text) = app.unsent_steers.first().cloned() {
            let images = app.images_for(&text);
            if !self.command(Command::Steer { session_id: id.clone(), text: text.clone(), images }).await.is_ok_or_slow() {
                break;
            }
            app.unsent_steers.remove(0);
            app.steers.push(text);
        }
        app.touch();
    }

    /// A control command — interrupt, steering, an approval — waited for
    /// only so long: over the socket its answer never queues behind the
    /// turn's frames, but the keys are not held up on a daemon that does not
    /// answer. One not taken is asked again (`flush_requests`).
    async fn command(&self, cmd: Command) -> Result<(), EngineError> {
        let (tx, _rx) = mpsc::channel(1);
        match tokio::time::timeout(COMMAND_WAIT, self.host.execute(cmd, tx)).await {
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(EngineError::new(SLOW, "the host daemon did not answer in time; the command was sent")),
        }
    }

    /// One terminal event. True when it asks for a connectivity probe.
    async fn on_event<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, ev: Event, quitting: &mut bool) -> std::io::Result<bool> {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release && k.code == KeyCode::Char('z') && k.modifiers.contains(KeyModifiers::CONTROL) => self.suspend(app, term)?,
            Event::Key(k) if k.kind != KeyEventKind::Release => return Ok(self.on_key(app, legacy(k), quitting).await),
            Event::Paste(s) => {
                // With `/connect`'s overlay up, a paste is its question's
                // answer or nothing: a key pasted early, before the question
                // is up or while it is a pick, never reaches the prompt, a
                // turn or the history.
                // The agent's questions take a paste as the person's own
                // answer, once they have settled.
                if let Some(a) = app.asking.as_mut() {
                    if app.approval_shown.is_none_or(|t| t.elapsed() >= APPROVAL_SETTLE) {
                        a.paste(&s);
                    }
                    app.touch();
                    return Ok(false);
                }
                match app.flow.as_mut().filter(|_| app.overlay == Overlay::Connect) {
                    Some(f) => {
                        if f.settled(APPROVAL_SETTLE) {
                            f.type_str(&s);
                        }
                    }
                    // Settings takes no text: a paste is dropped.
                    None if app.overlay == Overlay::Settings => {}
                    // What some terminals send for a clipboard holding only
                    // an image: the clipboard is read for it.
                    None if s.is_empty() => self.start_paste(app, paste::from_clipboard),
                    // A file dragged onto the terminal: read now, before a
                    // screenshot's temporary file is gone.
                    None => match paste::dropped(&s) {
                        Some(paths) => self.start_paste(app, move || paste::from_files(&paths, Some(s))),
                        None => app.editor.insert_str(&s),
                    },
                }
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
            self.give_up(term)?;
            // SAFETY: raise only sends a signal to this process.
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
            self.take_back(app, term)?;
        }
        #[cfg(not(unix))]
        let _ = (app, term);
        Ok(())
    }

    /// The terminal handed over as a shell expects it: the live region
    /// cleared (its top is where the next output lands), raw mode and
    /// bracketed paste off, the key reader stopped so nothing here reads
    /// what the person types next.
    fn give_up<W: Write>(&mut self, term: &mut Term<W>) -> std::io::Result<()> {
        self.keys = None;
        self.presence.pause();
        term.finish()?;
        restore_terminal();
        Ok(())
    }

    /// And taken back: the live region starts again on the row the cursor
    /// is on now, at the size the window is now.
    fn take_back<W: Write>(&mut self, app: &mut App, term: &mut Term<W>) -> std::io::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?2004h");
        let _ = out.write_all(term::KEYS_PUSH);
        let _ = out.write_all(term::TITLE_SAVE);
        let _ = out.flush();
        let (w, h) = crossterm::terminal::size().unwrap_or((term.size().width, term.size().height));
        term.resume(Size { width: w.max(1), height: h.max(1) }, cursor_row())?;
        self.keys = Some(EventStream::new());
        app.set_width(inner(w));
        Ok(())
    }

    /// A key while the agent's questions are shown: it picks or types,
    /// and the last answer, or a decline, is sent.
    async fn on_question_key(&mut self, app: &mut App, req: &ApprovalRequest, k: KeyEvent) {
        let Some(done) = app.asking.as_mut().and_then(|a| a.key(k)) else { return };
        let (decision, answers) = match done {
            ask::Done::Answered(answers) => (ApprovalDecision::Allow, answers),
            ask::Done::Declined => (ApprovalDecision::Deny, Vec::new()),
        };
        app.answered(&req.request_id);
        if let Err(e) = self.command(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision, answers }).await {
            app.notice(if e.code == SLOW { "the host daemon is slow to answer — the answers were sent, and the turn goes on once it takes them" } else { "those questions were already answered, or their turn is over" });
        }
    }

    async fn on_key(&mut self, app: &mut App, k: KeyEvent, quitting: &mut bool) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        app.touch();
        app.flash = None;
        let armed = self.quit_armed.take().filter(|(_, t)| t.elapsed() < QUIT_CONFIRM).map(|(c, _)| c);
        // A call waiting for the person's say takes the keys that answer it
        // (R-PERM-2): y once, s for the session, p for the project, n or
        // Esc no, v to print a request that was cut to fit (its y/s/p work
        // only after); the agent's questions take every key but Ctrl's.
        // Ctrl-C still interrupts the turn, which declines it too.
        // Not while `/connect`'s text question is being typed into: an
        // account name with a `p` in it would allow a call for the project.
        let typing_answer = app.overlay == Overlay::Connect && app.flow.as_ref().is_some_and(|f| f.typing());
        if let Some(req) = app.approvals.first().cloned()
            && !ctrl
            && !typing_answer
        {
            // A key already on its way when the request came up — the
            // person was typing — is not an answer.
            if app.approval_shown.is_some_and(|t| t.elapsed() < APPROVAL_SETTLE) {
                return false;
            }
            // The agent's questions: the keys pick and type answers, and
            // the last answer sends them all.
            if app.asking.is_some() {
                self.on_question_key(app, &req, k).await;
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
                KeyCode::Char('p') if ready && !req.remember.is_empty() && self.host.synced().is_none() => Some(ApprovalDecision::AllowProject),
                KeyCode::Char('n') | KeyCode::Esc => Some(ApprovalDecision::Deny),
                _ => None,
            };
            if let Some(d) = decision {
                app.answered(&req.request_id);
                match self.command(Command::Approve { session_id: req.session_id.clone(), request_id: req.request_id.clone(), decision: d, answers: Vec::new() }).await {
                    Ok(()) => {}
                    // Sent, and not answered in time: the daemon has it.
                    Err(e) if e.code == SLOW => app.notice("the host daemon is slow to answer — the approval was sent, and the turn goes on once it takes it"),
                    Err(_) => app.notice("that approval was already answered, or its turn is over"),
                }
            }
            return false;
        }
        // `/connect`'s overlay: the arrows and enter answer a pick, what is
        // typed or pasted answers a text question (and nothing of it reaches
        // the prompt), esc or Ctrl-C cancels the question — or, while
        // nothing is asked, only hides the overlay: the sign-in goes on.
        if app.overlay == Overlay::Connect
            && let Some(f) = app.flow.as_mut()
        {
            let typing = f.typing();
            // A key already on its way when the question came up — the
            // person was typing — answers nothing.
            let settled = f.settled(APPROVAL_SETTLE);
            match k.code {
                KeyCode::Esc => {
                    f.answer(None);
                    app.overlay = Overlay::None;
                }
                KeyCode::Char('c') if ctrl => {
                    f.answer(None);
                    app.overlay = Overlay::None;
                }
                // Leaves krowk whatever the prompt under the overlay holds,
                // as it does on an empty prompt: pressed twice, and with a
                // turn running, stopping that turn first, as there.
                KeyCode::Char('d') if ctrl => self.ask_to_leave(app, 'd', armed, quitting).await,
                KeyCode::Up if !typing => f.step(-1),
                KeyCode::Down if !typing => f.step(1),
                KeyCode::Enter if settled => f.enter(),
                KeyCode::Backspace if typing => f.backspace(),
                KeyCode::Char('u') if ctrl && typing => f.clear_input(),
                KeyCode::Char(c) if typing && settled && !ctrl && !alt => {
                    f.type_str(c.encode_utf8(&mut [0; 4]));
                }
                // Nothing else: while the overlay is up, nothing typed
                // reaches the prompt, and Enter never sends it.
                _ => {}
            }
            return false;
        }
        self.on_prompt_key(app, k, quitting, armed).await
    }

    /// Ctrl-C or Ctrl-D asked to leave krowk: the first press asks for a
    /// second, the second (the same key, within `QUIT_CONFIRM`) leaves,
    /// stopping a running turn first. One stray key is not a lost session.
    async fn ask_to_leave(&mut self, app: &mut App, key: char, armed: Option<char>, quitting: &mut bool) {
        if armed != Some(key) {
            self.quit_armed = Some((key, Instant::now()));
            app.flash = Some(format!("Press `Ctrl-{}` again to exit", key.to_ascii_uppercase()));
        } else if app.running() && self.host.synced().is_some() {
            // The turn is the host's: leaving the viewer leaves it running.
            self.turn = None;
            self.rx = None;
            app.quit = true;
        } else if app.running() {
            *quitting = true;
            self.interrupt(app).await;
        } else {
            app.quit = true;
        }
    }

    /// A key no question or overlay took: the prompt's, the menus' and the
    /// commands'. `armed`: the key before this one that asked for a second
    /// to leave krowk.
    async fn on_prompt_key(&mut self, app: &mut App, k: KeyEvent, quitting: &mut bool, armed: Option<char>) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Shift-enter is alt-enter: a new line wherever alt-enter makes one.
        // Only a terminal that took KEYS_PUSH tells the two enters apart.
        let alt = k.modifiers.contains(KeyModifiers::ALT) || (k.code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::SHIFT));
        let plain = !ctrl && !alt;
        if let Some(done) = self.settings_key(app, k, ctrl, alt) {
            return done;
        }
        if app.overlay == Overlay::Agents && plain && let Some(done) = self.agents_key(app, k).await {
            return done;
        }
        if let Some(done) = self.trust_key(app, k, plain) {
            return done;
        }
        if let Some(done) = self.offer_key(app, k, ctrl).await {
            return done;
        }
        if app.slash_open() && plain && let Some(done) = self.slash_key(app, k).await {
            return done;
        }
        if app.overlay == Overlay::Keys && plain && let Some(done) = self.help_key(app, k).await {
            return done;
        }
        if app.overlay == Overlay::Copy && plain && let Some(done) = self.copy_key(app, k) {
            return done;
        }
        if app.overlay == Overlay::Modes && plain && let Some(done) = self.modes_key(app, k) {
            return done;
        }
        if app.overlay == Overlay::Sessions && plain && let Some(done) = self.sessions_key(app, k) {
            return done;
        }
        if app.overlay == Overlay::Models && plain && let Some(done) = self.models_key(app, k).await {
            return done;
        }
        if let Some(done) = self.command_key(app, k, (ctrl, alt), quitting, armed).await {
            return done;
        }
        if !app.editor.text().starts_with('/') {
            app.slash_closed = false;
        }
        false
    }

    /// Settings takes every key while it is open, ahead of the trust
    /// question and a limit's offer, which are no one's answer here: ↑
    /// and ↓ choose a setting, ← and → change it, enter, esc or Ctrl-C
    /// close it, Ctrl-D still leaves krowk, and nothing reaches the prompt.
    /// Choosing goes one way and stops at the end, so a key held down,
    /// whose repeats most terminals send as presses, chooses the same
    /// value again; and a key typed ahead, before it was up to be seen,
    /// does nothing. Ctrl-C on a running turn interrupts it, as anywhere.
    /// Like each `…_key` here: Some with what `on_prompt_key` answers when
    /// it took the key, None when the key goes on.
    fn settings_key(&mut self, app: &mut App, k: KeyEvent, ctrl: bool, alt: bool) -> Option<bool> {
        if app.overlay != Overlay::Settings || (ctrl && k.code == KeyCode::Char('d')) || (ctrl && k.code == KeyCode::Char('c') && app.running()) {
            return None;
        }
        let settled = self.settings_shown.is_some_and(|t| t.elapsed() >= APPROVAL_SETTLE);
        match k.code {
            KeyCode::Esc => app.overlay = Overlay::None,
            KeyCode::Char('c') if ctrl => app.overlay = Overlay::None,
            KeyCode::Enter if settled && !ctrl && !alt => app.overlay = Overlay::None,
            KeyCode::Up if !ctrl && !alt => app.setting_at = 0,
            KeyCode::Down if !ctrl && !alt => app.setting_at = 1,
            KeyCode::Left | KeyCode::Right if settled && !ctrl && !alt => self.step_setting(app, if k.code == KeyCode::Left { -1 } else { 1 }),
            _ => {}
        }
        app.touch();
        Some(false)
    }

    /// The Agents overlay takes the keys that move through it: select a
    /// subagent, expand its line, interrupt it alone (R-SUB-2, R-SUB-3).
    async fn agents_key(&mut self, app: &mut App, k: KeyEvent) -> Option<bool> {
        match k.code {
            KeyCode::Up | KeyCode::Down => app.agent_move(if k.code == KeyCode::Up { -1 } else { 1 }),
            KeyCode::Enter => app.agent_toggle(),
            KeyCode::Char('x') => {
                if let Some(id) = app.agent_selected_running()
                    && let Err(e) = self.command(Command::Interrupt { session_id: id }).await
                {
                    app.notice(&e.message);
                }
            }
            _ => return None,
        }
        Some(false)
    }

    /// The trust question for a routed backend. It is answered by one
    /// key on an empty prompt once it has been up for APPROVAL_SETTLE:
    /// `y` trusts the repository and sends what was held; `n` or Esc
    /// does not, and puts it back in the prompt, to be asked again on
    /// the next send. Any other key — and any key typed ahead, before
    /// the settle or onto text in the prompt — is no answer, and goes to
    /// the prompt as it would.
    fn trust_key(&mut self, app: &mut App, k: KeyEvent, plain: bool) -> Option<bool> {
        if app.trust_question.is_none() || !plain || !app.editor.is_empty() || !self.trust_shown.is_some_and(|t| t.elapsed() >= APPROVAL_SETTLE) {
            return None;
        }
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
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.trust_question = None;
                app.notice("not trusted, so nothing ran — send the prompt again to be asked again, or pick an API model with /model");
                self.unhold(app);
            }
            _ => return None,
        }
        Some(false)
    }

    /// A limit's offer (R-INST-7): one keystroke, y, and never taken
    /// silently — anything else declines it, and a key that is not an
    /// answer still does what it does.
    async fn offer_key(&mut self, app: &mut App, k: KeyEvent, ctrl: bool) -> Option<bool> {
        let offer = app.offer.clone()?;
        if ctrl || app.running() {
            return None;
        }
        app.offer = None;
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if self.switch(app, offer.to).await && !self.last_prompt.is_empty() {
                    let text = self.last_prompt.clone();
                    self.prompt(app, text);
                }
                Some(false)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => Some(false),
            _ => None,
        }
    }

    /// The `/` menu, while a command is being typed: the arrows choose,
    /// tab completes, enter runs a command or completes a skill, esc
    /// puts the menu away. Everything else goes to the prompt.
    async fn slash_key(&mut self, app: &mut App, k: KeyEvent) -> Option<bool> {
        let found = help::slash(app.editor.text(), &app.skills);
        match k.code {
            KeyCode::Up => app.slash_at = app.slash_at.saturating_sub(1),
            KeyCode::Down => app.slash_at = (app.slash_at + 1).min(found.len().saturating_sub(1)),
            KeyCode::Esc => app.slash_closed = true,
            // An unlisted command runs as typed.
            KeyCode::Enter if app.slash_at == 0 && help::unlisted(app.editor.text()) => return Some(self.submit(app).await),
            KeyCode::Tab | KeyCode::Enter => {
                let Some(s) = found.get(app.slash_at.min(found.len().saturating_sub(1))) else { return Some(false) };
                app.editor.clear();
                app.slash_at = 0;
                if s.skill || k.code == KeyCode::Tab {
                    // A skill takes what follows it: the menu closes on
                    // the space, and the next enter sends it.
                    app.editor.insert_str(&format!("/{} ", s.name));
                    return Some(false);
                }
                app.editor.insert_str(&format!("/{}", s.name));
                return Some(self.submit(app).await);
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => {
                app.slash_at = 0;
                return None;
            }
            _ => return None,
        }
        Some(false)
    }

    /// The help menu takes the arrows, enter and esc while it is open;
    /// everything typed goes to the prompt, and filters it.
    async fn help_key(&mut self, app: &mut App, k: KeyEvent) -> Option<bool> {
        let found = help::filter(app.editor.text());
        match k.code {
            KeyCode::Up => app.help_at = app.help_at.saturating_sub(1),
            KeyCode::Down => app.help_at = (app.help_at + 1).min(found.len().saturating_sub(1)),
            KeyCode::Esc => {
                app.overlay = Overlay::None;
                app.editor.clear();
            }
            KeyCode::Enter => {
                app.overlay = Overlay::None;
                app.editor.clear();
                if let Some(entry) = found.get(app.help_at) {
                    self.help_action(app, entry).await;
                }
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => {
                app.help_at = 0;
                return None;
            }
            _ => return None,
        }
        Some(false)
    }

    /// What a help menu entry chosen with enter does.
    async fn help_action(&mut self, app: &mut App, entry: &help::Entry) {
        match entry.action {
            help::Action::Tell => {}
            help::Action::Model | help::Action::Mode | help::Action::New | help::Action::Sessions if self.synced_refuses(app, entry.title) => {}
            help::Action::Model => self.open_models(app),
            help::Action::Mode => app.open_mode_picker(),
            help::Action::Settings => self.open_settings(app),
            help::Action::Connect => self.open_flow(app, connect::Job::Connect(None), false),
            help::Action::Disconnect => self.open_flow(app, connect::Job::Disconnect(None), false),
            help::Action::New => self.new_session(app),
            help::Action::Sessions => self.open_resume(app),
            help::Action::Todos => app.overlay = Overlay::Todos,
            help::Action::Agents => app.overlay = Overlay::Agents,
            help::Action::Details => app.overlay = Overlay::Details,
            help::Action::Copy => copy(app),
            help::Action::PasteImage => self.start_paste(app, paste::from_clipboard),
            help::Action::Interrupt => {
                if app.running() {
                    self.interrupt(app).await;
                }
            }
            help::Action::Quit => app.quit = true,
        }
    }

    /// The Ctrl-Y picker takes the arrows and enter while it is open.
    fn copy_key(&mut self, app: &mut App, k: KeyEvent) -> Option<bool> {
        let n = app.copy_list_len();
        match k.code {
            KeyCode::Up => app.copy_at = app.copy_at.saturating_sub(1),
            KeyCode::Down => app.copy_at = (app.copy_at + 1).min(n.saturating_sub(1)),
            KeyCode::Enter => app.copy_chosen(app.copy_at),
            _ => return None,
        }
        Some(false)
    }

    /// The mode picker takes the arrows and enter while it is open.
    fn modes_key(&mut self, app: &mut App, k: KeyEvent) -> Option<bool> {
        match k.code {
            KeyCode::Up => app.mode_at = app.mode_at.saturating_sub(1),
            KeyCode::Down => app.mode_at = (app.mode_at + 1).min(PermissionMode::NAMES.len() - 1),
            KeyCode::Enter => {
                app.overlay = Overlay::None;
                if let Some(m) = PermissionMode::NAMES.get(app.mode_at).and_then(|n| PermissionMode::parse(n)) {
                    self.set_mode(app, m);
                }
            }
            _ => return None,
        }
        Some(false)
    }

    /// `/sessions` takes the arrows and enter while it is open.
    fn sessions_key(&mut self, app: &mut App, k: KeyEvent) -> Option<bool> {
        match k.code {
            KeyCode::Up => app.resume_at = app.resume_at.saturating_sub(1),
            KeyCode::Down => app.resume_at = (app.resume_at + 1).min(app.resumable.len().saturating_sub(1)),
            KeyCode::Enter => {
                app.overlay = Overlay::None;
                if let Some(r) = app.resumable.get(app.resume_at).cloned() {
                    self.resume(app, &r.id);
                }
            }
            _ => return None,
        }
        Some(false)
    }

    /// The model picker takes the arrows, enter and esc while it is open;
    /// everything typed goes to the prompt, and filters it.
    async fn models_key(&mut self, app: &mut App, k: KeyEvent) -> Option<bool> {
        let found = app.found_picks().len();
        match k.code {
            KeyCode::Up => app.pick_at = app.pick_at.min(found.saturating_sub(1)).saturating_sub(1),
            KeyCode::Down => app.pick_at = (app.pick_at + 1).min(found.saturating_sub(1)),
            KeyCode::Esc => {
                app.overlay = Overlay::None;
                app.editor.clear();
            }
            KeyCode::Enter => return Some(self.pick_model(app, found).await),
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => {
                app.pick_at = 0;
                return None;
            }
            _ => return None,
        }
        Some(false)
    }

    /// Enter in the model picker, `found` its matches: the one chosen, or
    /// with none, what was typed routed as `/model` would.
    async fn pick_model(&mut self, app: &mut App, found: usize) -> bool {
        app.overlay = Overlay::None;
        let typed = app.editor.text().trim().to_string();
        let chosen = app.found_picks().get(app.pick_at.min(found.saturating_sub(1))).map(|p| (*p).clone());
        app.editor.clear();
        // Nothing listed matches: what was typed is a model to
        // route, as `/model` would.
        if chosen.is_none() && !typed.is_empty() {
            app.editor.insert_str(&format!("/model {typed}"));
            return self.submit(app).await;
        }
        if let Some(p) = chosen {
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
        false
    }

    /// The keys that are commands rather than editing: Ctrl-C, Ctrl-D,
    /// esc, the overlays' and enter. The rest edit the prompt.
    async fn command_key(&mut self, app: &mut App, k: KeyEvent, (ctrl, alt): (bool, bool), quitting: &mut bool, armed: Option<char>) -> Option<bool> {
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if self.ctrl_c(app, quitting, armed).await {
                    return Some(false);
                }
            }
            KeyCode::Char('d') if ctrl => {
                if app.editor.is_empty() {
                    self.ask_to_leave(app, 'd', armed, quitting).await;
                } else {
                    app.editor.delete();
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
            KeyCode::Char('v') if ctrl || alt => self.start_paste(app, paste::from_clipboard),
            KeyCode::Char('o') if ctrl => app.overlay = if app.overlay == Overlay::Details { Overlay::None } else { Overlay::Details },
            KeyCode::Char('t') if ctrl => app.overlay = if app.overlay == Overlay::Todos { Overlay::None } else { Overlay::Todos },
            KeyCode::Char('g') if ctrl => app.overlay = if app.overlay == Overlay::Agents { Overlay::None } else { Overlay::Agents },
            KeyCode::F(1) => app.toggle_help(),
            KeyCode::Char('?') if app.editor.is_empty() && !ctrl && !alt => app.toggle_help(),
            KeyCode::Enter if alt => app.editor.insert('\n'),
            KeyCode::Enter => {
                if app.editor.enter() {
                    return Some(self.submit(app).await);
                }
            }
            _ => edit_key(&mut app.editor, k, ctrl, alt),
        }
        None
    }

    /// Ctrl-C: interrupts a running turn, clears the prompt, gives back
    /// what waits on the route or the trust question, or asks to leave.
    /// True when a second one on a running turn leaves krowk now.
    async fn ctrl_c(&mut self, app: &mut App, quitting: &mut bool, armed: Option<char>) -> bool {
        if app.running() {
            if *quitting || app.turn.as_ref().is_some_and(|t| t.want_interrupt) {
                // A second Ctrl-C does not wait for the first — but
                // still leaves through the front door: the terminal
                // restored, the resume line printed, the session
                // recorded, then exit 130.
                self.abandoned = true;
                return true;
            }
            self.interrupt(app).await;
        } else if !app.editor.is_empty() {
            app.editor.clear();
        } else if self.held.is_some() || app.trust_question.is_some() {
            // What is waiting on the route or the trust question
            // comes back unsent; Ctrl-C twice more then quits.
            app.trust_question = None;
            self.unhold(app);
            app.notice("not sent — it is back in the prompt");
        } else {
            self.ask_to_leave(app, 'c', armed, quitting).await;
        }
        false
    }

    /// `/sessions`: the sessions started where this one runs, to continue
    /// one of them here. Only between turns — the running one would go on
    /// writing to a session no longer shown.
    fn open_resume(&mut self, app: &mut App) {
        if !self.can_resume(app) {
            return;
        }
        let mut sessions = log::recent(&self.sessions_dir, &self.runs_in, RESUMABLE + 1);
        sessions.retain(|r| app.session_id.as_ref() != Some(&r.id));
        sessions.truncate(RESUMABLE);
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        app.open_resume(sessions, now_ms);
    }

    /// Continues session `id` in place of the one shown: its conversation
    /// replayed under what is on screen, and the next prompt sent to it, on
    /// the model it last ran on.
    fn resume(&mut self, app: &mut App, id: &str) {
        if !self.can_resume(app) {
            return;
        }
        if app.session_id.as_deref() == Some(id) {
            app.notice(&format!("session {id} is the one shown"));
            return;
        }
        // Read before anything is let go: a log that cannot be read leaves
        // the session shown as it was.
        let events = match read_session(&self.sessions_dir, id) {
            Ok(events) => events,
            Err(e) => return app.notice(&e),
        };
        // Here, only a session of here: the trust question, the skills and
        // the header are this directory's. A subagent's belongs to its
        // parent, which continues it.
        match events.first().map(|e| &e.body) {
            Some(krowk_harness::protocol::LogBody::SessionStarted { parent_session_id: Some(parent), .. }) => {
                return app.notice(&format!("session {id} is a subagent of {parent} — continue {parent} instead"));
            }
            Some(krowk_harness::protocol::LogBody::SessionStarted { cwd, .. }) if std::path::Path::new(cwd) != self.runs_in => {
                return app.notice(&format!("session {id} was started in {} — continue it there, with krowk --resume {id}", home_relative(std::path::Path::new(cwd))));
            }
            _ => {}
        }
        self.leave(app);
        app.forget_session();
        app.model = None;
        app.gap_say(&format!("continuing session {id}"));
        replay(app, id, &events, &self.host.registry());
        if self.live.iter().any(|l| l == id) {
            self.live.retain(|l| l != id);
            self.reattach(app, id, events.last().map(|e| e.id.clone()));
        }
        // Where that session's agent was at work, not this one's.
        self.look_for_pr(app);
        // Its own model from here, not the one the last session was on; one
        // that never ran a turn goes on with the model the next prompt had.
        self.chosen = None;
        let m = app.model.clone();
        self.settle_on(app, m);
        if let Some(m) = self.model.clone() {
            self.owe_trust(app, &m);
        }
    }

    /// `/new`: a fresh session in place of the one shown, on the same
    /// model, on a cleared screen; the next prompt starts it. The one left
    /// can be continued with `/sessions`. Not after a resume at start of a
    /// session begun elsewhere: the trust and settings were that
    /// directory's, and a new session would run here.
    fn new_session(&mut self, app: &mut App) {
        if !krowk_harness::connect::same_dir(&self.runs_in, &self.started_in) {
            return app.notice(&format!("this krowk runs a session of {} — start krowk again for a new one", home_relative(&self.runs_in)));
        }
        if !self.can_resume(app) {
            return;
        }
        self.leave(app);
        // The one the next prompt goes to, else the one shown — the
        // resumed session's at start — rather than one routed afresh.
        let m = self.model.clone().or_else(|| app.model.clone());
        self.settle_on(app, m);
        app.branch = pr::branch(&self.runs_in);
        app.start_over(&home_relative(&self.runs_in), self.effort_label.as_deref());
        // A lookup under way was for where the session left worked.
        self.pr = None;
        self.look_for_pr(app);
        if let Some(m) = self.model.clone() {
            self.owe_trust(app, &m);
        }
    }

    /// Session `m`'s model, where there is one, as the next prompt's and
    /// the one shown; with none, the next prompt's stays. The trust
    /// question owed for the last one is let go: the caller asks it again
    /// for this one (`owe_trust`) once the screen is its own.
    fn settle_on(&mut self, app: &mut App, m: Option<ModelRef>) {
        self.needs_trust = None;
        app.trust_question = None;
        if let Some(m) = m {
            self.retarget(&m);
            self.model = Some(m);
        }
        app.model = self.model.clone();
    }

    /// The session shown, left for another: kept to be listed on the way
    /// out, and its last prompt no longer the title's.
    fn leave(&mut self, app: &mut App) {
        // The daemon stops sending it here: no frame of it is taken for the
        // next session's.
        if let Some(old) = &app.session_id {
            self.host.leave(old);
        }
        if let Some(old) = app.session_id.clone().filter(|old| !app.left.contains(old)) {
            app.left.push(old);
        }
        self.last_prompt.clear();
    }

    /// Whether another session may take the one shown's place now: not
    /// while a turn runs, nor while a prompt waits for the model or the
    /// trust question, nor a `/model` is being routed — each was meant for
    /// the session shown. The model routed at start is not: a session
    /// resumed meanwhile has its own, which the route then leaves alone.
    fn can_resume(&self, app: &mut App) -> bool {
        let waits = if app.running() {
            "the running turn to finish — `esc` interrupts it"
        } else if app.offer.is_some() {
            "the question above — `y` or `n`"
        } else if self.held.is_some() {
            "the prompt waiting to be sent — `ctrl-c` takes it back"
        } else if self.model_route.is_some() {
            "the /model switch to finish"
        } else if app.backend_agents_running() {
            "the agents running in the background — `ctrl-g` lists them"
        } else {
            return true;
        };
        app.notice_keys(&format!("that waits for {waits}"));
        false
    }

    fn open_models(&mut self, app: &mut App) {
        let instances: Vec<(String, &'static str)> = self.host.registry().instances.values().map(|i| (i.name.clone(), i.kind)).collect();
        app.open_picker(&instances);
        self.check_marks(app);
    }

    /// Starts `/connect` or `/disconnect` on a thread of its own, with its
    /// overlay; `first_run` is the card that opens when nothing here can
    /// run a model. One at a time: asked again, the running one is shown.
    fn open_flow(&mut self, app: &mut App, job: connect::Job, first_run: bool) {
        let Some(paths) = self.paths.clone() else {
            app.notice("there is no home directory to keep a connection in — set HOME, or KROWK_HOME to an absolute path");
            return;
        };
        if self.auth.is_some() {
            app.overlay = Overlay::Connect;
            if let Some(f) = app.flow.as_mut() {
                f.shown();
            }
            return;
        }
        if app.running() {
            app.notice_keys("that waits for the running turn to finish — `esc` interrupts it");
            return;
        }
        let (title, prefer) = match &job {
            connect::Job::Connect(_) => ("Connect a provider", None),
            // Alone, it starts on the session's instance.
            connect::Job::Disconnect(t) => ("Disconnect", t.is_none().then(|| self.model.as_ref().map(|m| m.instance.clone())).flatten()),
        };
        let intro = match first_run {
            true => vec![
                "Nothing here can run a model yet — connect a provider to start.".to_string(),
                "A subscription signs in with the vendor's own login; an API key is kept in krowk's credentials file, never in config.json.".to_string(),
            ],
            false => Vec::new(),
        };
        app.flow = Some(connect::Flow::new(title, intro, prefer));
        app.overlay = Overlay::Connect;
        self.first_run = first_run;
        self.auth = Some(connect::start(job, paths));
    }

    /// One message from the sign-in's thread.
    fn on_auth<W: Write>(&mut self, app: &mut App, term: &mut Term<W>, m: Option<connect::Msg>) -> std::io::Result<()> {
        use connect::{Msg, Note};
        app.touch();
        let Some(m) = m else {
            // Gone without a word: it panicked.
            self.auth = None;
            app.flow = None;
            if app.overlay == Overlay::Connect {
                app.overlay = Overlay::None;
            }
            if self.suspended {
                self.suspended = false;
                self.take_back(app, term)?;
            }
            app.notice("the sign-in stopped before it finished — `krowk status` says what is connected");
            return Ok(());
        };
        match m {
            Msg::Ask { ask, reply } => match app.flow.as_mut() {
                Some(f) => {
                    f.asked(ask, reply);
                    // Up, the overlay shows it. Hidden, or with another
                    // overlay open, it waits to be opened: taking the keys
                    // mid-prompt would make what is being typed its answer.
                    if app.overlay == Overlay::Connect {
                        f.shown();
                    } else {
                        app.notice("/connect is waiting for an answer — /connect opens it");
                    }
                }
                None => {
                    let _ = reply.send(None);
                }
            },
            Msg::Note(Note::Info(s)) => {
                app.gap_say(&s);
                if let Some(f) = app.flow.as_mut() {
                    f.busy = s;
                }
            }
            Msg::Note(Note::Url { message, url }) => {
                app.link(&message, &url);
                if let Some(f) = app.flow.as_mut() {
                    f.busy = "waiting for the sign-in in your browser — its link is above".into();
                }
            }
            Msg::Note(Note::Code { url, code, message }) => {
                let code = card::clean(&code);
                app.link(&format!("Open this page and enter the code {code} {message}:"), &url);
                if let Some(f) = app.flow.as_mut() {
                    f.busy = format!("enter the code {code} at the page linked above — waiting…");
                }
            }
            Msg::Suspend { ready } => {
                // What is owed to scrollback goes first: krowk's words
                // before the vendor's.
                self.draw(app, term)?;
                self.give_up(term)?;
                self.suspended = true;
                let _ = ready.send(());
            }
            Msg::Resume => {
                if self.suspended {
                    self.suspended = false;
                    self.take_back(app, term)?;
                }
            }
            Msg::Done(done, registry) => {
                if self.suspended {
                    self.suspended = false;
                    self.take_back(app, term)?;
                }
                self.auth = None;
                self.finished(app, done, registry);
            }
        }
        Ok(())
    }

    /// A sign-in's end: the instances read again for every turn from now
    /// on, what it did in the words `krowk connect` uses, and — when the
    /// session had nothing ready to run on, or this is the first connection
    /// (it became the default) — the session moved onto it.
    fn finished(&mut self, app: &mut App, done: Result<connect::Done, EngineError>, registry: Option<Box<krowk_harness::instances::Registry>>) {
        // Whether the session had nothing to run on: its instance marked not
        // ready, or — never marked — not ready by what needs no process.
        let stuck = match &self.model {
            None => true,
            Some(m) => match app.marks.get(&m.instance) {
                Some(mark) => matches!(mark, Mark::Not(_)),
                None => self.host.registry().get(&m.instance).ok().and_then(|i| krowk_harness::readiness::local(i, &self.credentials)).is_some_and(|r| matches!(Mark::of(&r), Mark::Not(_))),
            },
        };
        let first_run = std::mem::take(&mut self.first_run);
        app.flow = None;
        if app.overlay == Overlay::Connect {
            app.overlay = Overlay::None;
        }
        // Only a connection or a sign-out that happened replaces a process,
        // and only the one of the instance it names: a cancelled or failed
        // flow leaves every session's running process as it was. The
        // instances are taken as the files now say either way — a connect
        // that failed after it wrote its definition or key has still
        // written them.
        let changed = match &done {
            Ok(connect::Done::Connected(c)) => Some(c.instance.clone()),
            Ok(connect::Done::Disconnected(d)) => Some(d.instance.clone()),
            Ok(connect::Done::Renamed(r)) => Some(r.from.clone()),
            Err(_) => None,
        };
        let reread = registry.is_some();
        if let Some(r) = registry {
            app.vendor_instances = r.instances.values().filter(|i| i.backend.is_some()).map(|i| i.name.clone()).collect();
            match &done {
                // The same instance, renamed: its backend process goes on.
                Ok(connect::Done::Renamed(n)) => self.host.set_registry_renamed(*r, &n.from, &n.to),
                _ => self.host.set_registry(*r, changed.as_deref()),
            }
        }
        if changed.is_some() {
            // Whatever was marked before may have changed, a check still
            // out included: its answers are dropped with it.
            app.marks.clear();
            self.checks = None;
        }
        match done {
            Ok(connect::Done::Connected(c)) => {
                let s = c.summary(false);
                let mut notes = s.notes;
                if s.signed_in_already {
                    notes.push("another account is added by name: /connect, then + Add account".into());
                }
                app.done(if c.renewed { "Renewed" } else { "Connected" }, &c.instance, &s.facts, &notes);
                match krowk_harness::connect::default_model(c.definition.tag()) {
                    Some(model) if c.default_model.is_some() || stuck || first_run => self.adopt(app, ModelRef { instance: c.instance.clone(), model: model.to_string() }),
                    Some(model) => app.gap_say(&format!("/model {}/{model} runs it", c.instance)),
                    // A server's models are its own: the id is the person's to type.
                    None => {
                        app.editor.clear();
                        app.editor.insert_str(&format!("/model {}/", c.instance));
                    }
                }
            }
            Ok(connect::Done::Disconnected(d)) => app.disconnected(&d),
            Ok(connect::Done::Renamed(r)) => {
                app.done("Renamed", &format!("{} to {}", r.from, r.to), &[], &r.notes());
                // Only once the host has the new name: until then the old
                // one is the one it runs.
                if reread {
                    self.renamed(app, &r.from, &r.to);
                } else {
                    app.notice(&format!("config.json could not be read again — restart krowk to run on {}", r.to));
                }
            }
            Err(e) if e.code == "selection_cancelled" => app.gap_say(if first_run { "nothing connected — /connect when you are ready" } else { &e.message }),
            Err(e) => app.error(&e.info()),
        }
    }

    /// An instance renamed: the session, and whatever it holds of the old
    /// name, on the new one — the same instance, so nothing is asked again.
    fn renamed(&mut self, app: &mut App, from: &str, to: &str) {
        for m in [self.model.as_mut(), self.chosen.as_mut(), self.needs_trust.as_mut().map(|(m, _)| m)].into_iter().flatten() {
            if m.instance == from {
                m.instance = to.to_string();
            }
        }
        app.renamed(from, to);
    }

    /// The session moves onto `m`, as a routed model is taken: the next
    /// prompt runs there, after the trust question on a backend in a
    /// repository nobody trusted.
    fn adopt(&mut self, app: &mut App, m: ModelRef) {
        app.say_switched(&format!("now on {m}"));
        self.retarget(&m);
        self.model = Some(m.clone());
        self.chosen = Some(m.clone());
        app.model = Some(m.clone());
        self.needs_trust = None;
        app.trust_question = None;
        self.owe_trust(app, &m);
    }

    /// Marks every instance's readiness for the pickers: what needs no
    /// process now, the vendors behind it off the runtime's thread. True
    /// while vendors are being asked.
    fn check_marks(&mut self, app: &mut App) -> bool {
        let reg = self.host.registry();
        let mut ask = Vec::new();
        for i in reg.instances.values() {
            match krowk_harness::readiness::local(i, &self.credentials) {
                Some(r) => {
                    app.marks.insert(i.name.clone(), Mark::of(&r));
                }
                // A vendor is asked again each time: a sign-in elsewhere
                // shows at the next open (its "signed in" is the readiness
                // cache's for a minute), the last answer shown meanwhile.
                None => ask.push(i.clone()),
            }
        }
        if ask.is_empty() || self.checks.is_some() {
            return self.checks.is_some();
        }
        let probe = self.data_dir.as_deref().ok_or_else(|| "no data directory".to_string()).and_then(krowk_harness::readiness::neutral_dir).map(krowk_harness::readiness::Probe::at);
        let Ok(probe) = probe else {
            for i in &ask {
                app.marks.insert(i.name.clone(), Mark::Unknown);
            }
            return false;
        };
        let creds = self.credentials.clone();
        self.checks = Some(Box::pin(async move {
            tokio::task::spawn_blocking(move || krowk_harness::readiness::check_all(&ask.iter().collect::<Vec<_>>(), &creds, &probe)).await.unwrap_or_default()
        }));
        true
    }

    /// The vendors' answers, marked; and the first-run card, when they
    /// were asked for it and nothing can run.
    fn marked(&mut self, app: &mut App, reports: Vec<krowk_harness::readiness::Report>) {
        for r in reports {
            app.marks.insert(r.instance.clone(), Mark::of(&r.readiness));
        }
        app.touch();
        if std::mem::take(&mut self.first_run_pending) && app.marks.values().all(|m| matches!(m, Mark::Not(_))) && app.overlay == Overlay::None && !app.running() && self.auth.is_none() {
            self.open_flow(app, connect::Job::Connect(None), true);
        }
    }

    /// Reads the branch checked out and asks `gh` for its pull request,
    /// off the loop, unless that is under way already or the status line
    /// shows neither: where the agent is at work, or where the session
    /// runs when that is no repository (or gone, a worktree removed).
    fn look_for_pr(&mut self, app: &App) {
        let shown = |i| app.settings.status_bar && app.settings.status_items.contains(&i);
        let (branch, pr) = (shown(settings::Item::Branch), shown(settings::Item::Pr));
        if self.pr.is_some() || !(branch || pr) {
            return;
        }
        self.looked_in = app.follow.works_in.clone();
        let dirs: Vec<PathBuf> = app.follow.works_in.iter().cloned().chain([self.runs_in.clone()]).collect();
        self.pr = Some(Box::pin(async move { tokio::task::spawn_blocking(move || pr::look(&dirs, pr)).await.unwrap_or_default() }));
    }

    /// Reads the branch again once the agent is at work somewhere else: a
    /// worktree it made shows its branch without waiting for the turn.
    fn follow_the_agent(&mut self, app: &App) {
        if app.follow.works_in != self.looked_in {
            self.look_for_pr(app);
        }
    }

    /// At start, with the model known: whether anything here can run a
    /// model at all. Ready by a key, a login in krowk's own file or a
    /// server that takes none, it can, and no vendor is asked; with none of
    /// those, the vendors are asked behind the first frame, and none of
    /// them ready opens the first-run card.
    fn startup_sweep(&mut self, app: &mut App) {
        let reg = self.host.registry();
        if reg.instances.values().filter_map(|i| krowk_harness::readiness::local(i, &self.credentials)).any(|r| !matches!(Mark::of(&r), Mark::Not(_))) {
            return;
        }
        self.first_run_pending = true;
        if !self.check_marks(app) {
            self.marked(app, Vec::new());
        }
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
        let text = help::canonical(app.editor.text().trim());
        if text.is_empty() {
            return false;
        }
        match text.as_str() {
            "/exit" => {
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
                if self.synced_refuses(app, "/model") {
                    return false;
                }
                self.open_models(app);
                return false;
            }
            "/settings" => {
                app.editor.clear();
                self.open_settings(app);
                return false;
            }
            "/new" => {
                app.editor.clear();
                if self.synced_refuses(app, "/new") {
                    return false;
                }
                self.new_session(app);
                return false;
            }
            "/sessions" => {
                app.editor.clear();
                if self.synced_refuses(app, "/sessions") {
                    return false;
                }
                self.open_resume(app);
                return false;
            }
            t if t.split_whitespace().next().map(help::canonical).as_deref() == Some("/new") => {
                app.editor.clear();
                app.notice("/new takes nothing after it — send the prompt once the new session is up");
                return false;
            }
            t if t.starts_with("/sessions ") => {
                app.editor.clear();
                if self.synced_refuses(app, "/sessions") {
                    return false;
                }
                let id = t["/sessions ".len()..].trim().to_string();
                self.resume(app, &id);
                return false;
            }
            "/mode" => {
                app.editor.clear();
                if self.synced_refuses(app, "/mode") {
                    return false;
                }
                app.open_mode_picker();
                return false;
            }
            t if t.starts_with("/mode ") => {
                app.editor.clear();
                if self.synced_refuses(app, "/mode") {
                    return false;
                }
                let name = t.split_once(' ').unwrap_or_default().1.trim();
                match PermissionMode::parse(name) {
                    Some(m) => self.set_mode(app, m),
                    None => app.notice(&format!("/mode: {name} is not a permission mode — one of {}", PermissionMode::NAMES.join(", "))),
                }
                return false;
            }
            "/connect" | "/disconnect" => {
                app.editor.clear();
                let job = if text == "/connect" { connect::Job::Connect(None) } else { connect::Job::Disconnect(None) };
                self.open_flow(app, job, false);
                return false;
            }
            t if t.starts_with("/connect ") || t.starts_with("/disconnect ") => {
                app.editor.clear();
                let (cmd, target) = t.split_once(' ').unwrap_or_default();
                let target = Some(target.trim().to_string());
                self.open_flow(app, if cmd == "/connect" { connect::Job::Connect(target) } else { connect::Job::Disconnect(target) }, false);
                return false;
            }
            t if t.starts_with("/model ") => {
                app.editor.clear();
                if self.synced_refuses(app, "/model") {
                    return false;
                }
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

/// Session `id`'s log, read whole, or why it could not be.
/// A control command's outcome, where one sent and not answered in time
/// counts as sent: sending it again would steer twice.
trait Sent {
    fn is_ok_or_slow(&self) -> bool;
}

impl Sent for Result<(), EngineError> {
    fn is_ok_or_slow(&self) -> bool {
        match self {
            Ok(()) => true,
            Err(e) => e.code == SLOW,
        }
    }
}

/// Where session `id` was started: its log's first line, read alone — the
/// daemon's other sessions may be long, and the first frame waits on this.
fn started_at(sessions_dir: &std::path::Path, id: &str) -> Option<PathBuf> {
    use std::io::BufRead;
    if !log::valid_id(id) {
        return None;
    }
    let f = std::fs::File::open(sessions_dir.join(id).join(log::EVENTS_FILE)).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(f).read_line(&mut first).ok()?;
    match serde_json::from_str::<krowk_harness::protocol::LogEvent>(&first).ok()?.body {
        krowk_harness::protocol::LogBody::SessionStarted { cwd, .. } => Some(PathBuf::from(cwd)),
        _ => None,
    }
}

fn read_session(sessions_dir: &std::path::Path, id: &str) -> Result<Vec<krowk_harness::protocol::LogEvent>, String> {
    if !log::valid_id(id) {
        return Err(format!("{id:?} is not a krowk session id"));
    }
    log::read_events(&sessions_dir.join(id).join(log::EVENTS_FILE)).map_err(|e| format!("session {id} could not be read: {}", e.message()))
}

/// Session `id`'s conversation into `app`, which continues it; the
/// directory it was started in.
fn replay(app: &mut App, id: &str, events: &[krowk_harness::protocol::LogEvent], registry: &krowk_harness::instances::Registry) -> Option<PathBuf> {
    let head = events.last().map(|e| e.id.clone()).unwrap_or_default();
    app.replay(&log::branch(events, &head));
    // A turn logged on a name since renamed is on the new name.
    for old in registry.renamed.keys() {
        app.renamed(old, &registry.current(old));
    }
    app.session_id = Some(id.to_string());
    match events.first().map(|e| &e.body) {
        Some(krowk_harness::protocol::LogBody::SessionStarted { cwd, .. }) => Some(PathBuf::from(cwd)),
        _ => None,
    }
}

/// A key as a terminal without KEYS_PUSH sends it. The protocol reports
/// the control keys that used to arrive as a C0 byte — Ctrl-[ (Esc), Ctrl-M
/// (Enter), Ctrl-I (Tab) — and Ctrl-Enter and Ctrl-Esc as keys of their
/// own, which the bindings here never had.
fn legacy(k: KeyEvent) -> KeyEvent {
    if !k.modifiers.contains(KeyModifiers::CONTROL) {
        return k;
    }
    let code = match k.code {
        KeyCode::Char('[') | KeyCode::Esc => KeyCode::Esc,
        KeyCode::Char('m') | KeyCode::Enter => KeyCode::Enter,
        KeyCode::Char('i') => KeyCode::Tab,
        _ => return k,
    };
    KeyEvent { code, modifiers: k.modifiers - KeyModifiers::CONTROL, ..k }
}

/// A key that edits the prompt.
fn edit_key(e: &mut Editor, k: KeyEvent, ctrl: bool, alt: bool) {
    match k.code {
        KeyCode::Char('j') if ctrl => e.insert('\n'),
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
}

impl Ui<'_> {
    /// Reads a paste off the runtime's thread, the keys waiting on none of
    /// it: what is typed meanwhile goes on after where it will land (the
    /// editor's mark), and a paste asked for meanwhile waits its turn.
    fn start_paste(&mut self, app: &mut App, read: impl FnOnce() -> paste::Pasted + Send + 'static) {
        let mark = app.editor.mark();
        self.pastes.push_back((mark, Box::new(read)));
        self.next_paste(app);
    }

    fn next_paste(&mut self, app: &mut App) {
        if self.pasting.is_some() {
            return;
        }
        let Some((mark, read)) = self.pastes.pop_front() else { return };
        app.flash = Some("pasting…".into());
        self.pasting = Some((Some(mark), Box::pin(async move { tokio::task::spawn_blocking(read).await.unwrap_or_default() })));
    }

    /// A paste read, and the next one asked for begun.
    fn on_pasted(&mut self, app: &mut App, p: paste::Pasted) {
        let mark = self.pasting.take().and_then(|(m, _)| m);
        self.pasted(app, mark, p);
        self.next_paste(app);
    }

    /// A paste read: its images into the prompt where it was asked for,
    /// else its text, and what went wrong said.
    fn pasted(&mut self, app: &mut App, mark: Option<u64>, p: paste::Pasted) {
        app.flash = None;
        let held: std::collections::HashSet<u32> = editor::image_tokens(app.editor.text()).map(|(_, n)| n).filter(|n| app.images.contains_key(n)).collect();
        // What the host takes of one prompt: as many, and as much base64.
        let b64 = |len: usize| len.div_ceil(3) * 4;
        let bytes: usize = held.iter().filter_map(|n| app.images.get(n)).chain(&p.images).map(|i| b64(i.bytes.len())).sum();
        let too_many = held.len() + p.images.len() > krowk_harness::images::MAX_IMAGES;
        if too_many || bytes > krowk_harness::images::SENT_BYTES {
            if let Some(m) = mark {
                app.editor.unmark(m);
            }
            let why = if too_many { format!("at most {} images", krowk_harness::images::MAX_IMAGES) } else { format!("at most {} MB of images", krowk_harness::images::SENT_BYTES / (1024 * 1024)) };
            app.notice(&format!("nothing pasted — a prompt takes {why}"));
        } else if !p.images.is_empty() {
            app.attach_all(p.images, mark);
        } else if let Some(t) = &p.text {
            app.editor.place_str(mark, t);
        } else if let Some(m) = mark {
            app.editor.unmark(m);
        }
        match p.problem {
            Some(why) if p.text.is_some() => app.flash = Some(format!("pasted as text — {why}")),
            Some(why) => app.notice(&format!("nothing pasted — {why}")),
            None => {}
        }
        self.forget_images(app);
        app.touch();
    }

    /// Lets go of the pasted images nothing can still send: kept are the
    /// held prompt's, and the last prompt's while its turn runs or a
    /// limit's offer would send it again — a refused prompt comes back to
    /// the editor, images and all.
    fn forget_images(&self, app: &mut App) {
        let mut also: Vec<&str> = self.held.as_deref().into_iter().collect();
        if self.turn.is_some() || app.offer.is_some() {
            also.push(&self.last_prompt);
        }
        app.forget_images(&also);
    }
}

async fn finish_paste(f: &mut Option<(Option<u64>, PasteFuture)>) -> paste::Pasted {
    match f {
        Some((_, f)) => f.await,
        None => std::future::pending().await,
    }
}

/// Ctrl-Y: what there is to copy, or a picker of it (`App::open_copy`).
fn copy(app: &mut App) {
    app.open_copy();
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
    fn a_control_key_the_keyboard_protocol_reports_apart_is_read_as_before() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code, modifiers| super::legacy(KeyEvent::new(code, modifiers));
        assert_eq!(key(KeyCode::Char('['), KeyModifiers::CONTROL), KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Char('m'), KeyModifiers::CONTROL), KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Char('i'), KeyModifiers::CONTROL), KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Esc, KeyModifiers::CONTROL), KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Enter, KeyModifiers::CONTROL), KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT), KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), "still a new line");
        assert_eq!(key(KeyCode::Char('j'), KeyModifiers::CONTROL), KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    }

    #[test]
    fn a_cursor_report_is_read_past_whatever_came_before_it() {
        assert_eq!(super::parse_cursor_report(b"\x1b[12;40R"), Some(11));
        assert_eq!(super::parse_cursor_report(b"ab\x1b[A\x1b[3;1R"), Some(2), "a key typed meanwhile is skipped");
        assert_eq!(super::parse_cursor_report(b"\x1b[3;1"), None, "not whole yet");
    }
}
