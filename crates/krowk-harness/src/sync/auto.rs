//! Automatic sync (spec R-SYNC-3; engineering/harness.md → Sync →
//! Automatic sync).
//!
//! **The policy** — whether this device syncs its sessions without a
//! command, which directories opt out, how long a bridge may sit idle, and
//! which directory a session belongs to — is pure reads of the environment,
//! the person's own `config.json` and the files on disk: no network,
//! nothing spawned. The supervisor and `krowk sync status` (Harness P4
//! ticket 95) call it.
//!
//! **The supervisor** (`Supervisor::spawn`) runs in the host daemon's
//! process and starts the existing bridge (`host::run`, what `krowk sync
//! host` runs) for each session the moment a turn starts in it, told by the
//! daemon's turn hook (`server::Options::turns`) — never by polling.
//!
//! **Letting go** (ticket 96). A bridge with no turn running, and none
//! started for the policy's idle window, is stopped: it seals its stream
//! and gives its lease back, and the session's next turn takes it up again.
//! Each turn event of a bridged session, and each idle check, reads the
//! policy again: off stops every bridge, an opt-out that has come since
//! stops that session's. A bridge refused because another holds the lease
//! (`lease_held` — a `krowk sync host`, say) waits a minute; one whose
//! lease another device took waits five; either is tried again only at a
//! turn, and never takes the lease from anyone. A workspace without a paid
//! plan stops every bridge and makes no sync call for an hour.
//!
//! It runs on a `std::thread` and a current-thread runtime of its own,
//! never on the daemon's thread: the daemon runs every session on one
//! thread, so a blocking call there stalls them all (R-LAG-9), and a bridge
//! blocks — the `ureq` registry client, its lease, list and writer threads'
//! joins, `parent_here`'s `File::open` from `frame()`, a checkpoint's `git
//! rev-parse`. So it never touches the daemon's `State`: its only links to
//! the daemon are the socket, which each bridge connects to as a client of
//! its own, and the in-process hooks (`turns`, `bridged`). For the same
//! reason this file is not in the list
//! `daemon::tests::r_lag_9_no_blocking_io_on_the_daemons_thread` checks:
//! nothing in it runs on that thread.

use crate::daemon::client::Client as Daemon;
use crate::daemon::server::TurnEvent;
use crate::protocol::LogBody;
use super::host;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

/// How long a bridge may sit idle before it is let go, when neither
/// `KROWK_SYNC_IDLE` nor `sync.idleMinutes` says.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(10 * 60);

/// The keys `sync` takes. Any other is refused, so a misspelt opt-out
/// (`"Auto": false`, `"excludes"`) is never read as nothing said.
const KEYS: [&str; 3] = ["auto", "exclude", "idleMinutes"];

/// A window this long or longer is never: no supervisor runs for a
/// century, and a `Duration` near `u64::MAX` seconds would overflow an
/// `Instant` it is added to.
const NEVER_AFTER: u64 = 100 * 365 * 24 * 60 * 60;

const AUTO_KEY: &str = "sync.auto in config.json";
const AUTO_ENV: &str = "KROWK_SYNC_AUTO";

/// What the device has said about automatic sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Sessions are synced without a command.
    pub on: bool,
    /// What turned it off, for a person to read: `KROWK_SYNC_AUTO` or
    /// `sync.auto in config.json`. `None` when on.
    pub off_by: Option<&'static str>,
    /// `sync.exclude`: absolute directories whose sessions are never
    /// synced, nor any session under them.
    pub exclude: Vec<PathBuf>,
    /// How long a bridge may sit idle before it is let go; `None` is never.
    pub idle: Option<Duration>,
}

/// Why a directory's sessions are not synced, naming the source a person
/// can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptOut {
    /// Under this `sync.exclude` entry (as written in config.json).
    Excluded(PathBuf),
    /// This `.krowk/no-sync`, in the directory or one of its ancestors.
    Marker(PathBuf),
}

/// The policy from `KROWK_SYNC_AUTO` and `KROWK_SYNC_IDLE`, else the
/// `sync` object of `config` — which must be the person's own
/// `~/.krowk/config.json`, never a repository's: a repository can only
/// turn sync off (with `.krowk/no-sync`), never on. No config reads as `{}`:
/// on, nothing excluded, the default idle window.
///
/// A bad value is an `Err` naming the key and how to fix it. **An `Err`
/// means off**: the supervisor syncs nothing while the policy cannot be
/// read, so a typo never sends a session off the machine. Never read an
/// error as "on". Every value is checked, the config's under an env
/// variable that overrides it too, so a broken `sync.exclude` is an `Err`
/// even with `KROWK_SYNC_AUTO=0`: still off, named by the error.
pub fn policy(env: &dyn Fn(&str) -> String, config: Option<&Value>) -> Result<Policy, String> {
    let sync = match config {
        None => None,
        Some(Value::Object(c)) => match c.get("sync") {
            None => None,
            Some(Value::Object(o)) => Some(o),
            // `"sync": false` must not read as "absent, so on".
            Some(v) => return Err(format!("sync in config.json is {}, not an object — write it as {{\"auto\": false}} to turn automatic sync off, or remove it", kind(v))),
        },
        Some(v) => return Err(format!("config.json is {}, not an object — fix it, or remove it", kind(v))),
    };
    if let Some(k) = sync.and_then(|o| o.keys().find(|k| !KEYS.contains(&k.as_str()))) {
        return Err(format!("sync.{k} in config.json is not a setting — sync takes {}; fix the name, or remove it", KEYS.join(", ")));
    }
    let key = |k: &str| sync.and_then(|o| o.get(k));

    let configured = match key("auto") {
        None | Some(Value::Bool(true)) => (true, None),
        Some(Value::Bool(false)) => (false, Some(AUTO_KEY)),
        Some(v) => return Err(format!("{AUTO_KEY} is {v}, not true or false — set it to false to turn automatic sync off, or remove it")),
    };
    let (on, off_by) = match env(AUTO_ENV).trim() {
        "" => configured,
        s => match s.to_ascii_lowercase().as_str() {
            "0" | "false" | "off" => (false, Some(AUTO_ENV)),
            "1" | "true" | "on" => (true, None),
            _ => return Err(format!("{AUTO_ENV} is {s:?}, not a switch — set it to 0, false or off to turn automatic sync off, to 1, true or on to turn it on, or unset it")),
        },
    };

    let exclude = match key("exclude") {
        None => Vec::new(),
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|e| match e.as_str() {
                Some(s) if Path::new(s).is_absolute() => Ok(PathBuf::from(s)),
                Some(s) => Err(format!("sync.exclude in config.json has {s:?}, which is not an absolute path — write it in full, from / (not ~ or relative)")),
                None => Err(format!("sync.exclude in config.json has {e}, which is not a path — every entry is an absolute directory, as a string")),
            })
            .collect::<Result<_, _>>()?,
        Some(v) => return Err(format!("sync.exclude in config.json is {v}, not a list — write it as [\"/absolute/dir\", …], or remove it")),
    };

    let configured = match key("idleMinutes") {
        None => DEFAULT_IDLE.as_secs(),
        // Under a second would round to 0, which is never: refused rather
        // than turned into the opposite of what it says (the rule
        // `daemon::idle_window` applies to host.idleMinutes).
        Some(v) => v.as_f64().filter(|m| *m == 0.0 || *m * 60.0 >= 1.0).map(|m| (m * 60.0).round() as u64).ok_or_else(|| format!("sync.idleMinutes in config.json is {v}, not a number of minutes of at least one second (0 never lets go) — fix it, or remove it for {} minutes", DEFAULT_IDLE.as_secs() / 60))?,
    };
    let secs = match env("KROWK_SYNC_IDLE").trim() {
        "" => configured,
        s => s.parse::<u64>().map_err(|_| format!("KROWK_SYNC_IDLE is {s:?}, not a number of seconds — set it to one (0 never lets go), or unset it"))?,
    };

    Ok(Policy { on, off_by, exclude, idle: (secs > 0 && secs < NEVER_AFTER).then(|| Duration::from_secs(secs)) })
}

impl Policy {
    /// Whether sessions in `cwd` are kept off sync, and by what: under a
    /// `sync.exclude` entry, matched by whole components (`/work/a` does
    /// not take `/work/ab`), or below a `.krowk/no-sync` — a file or a
    /// directory — in `cwd` or any ancestor. The `.krowk` that is krowk's
    /// own home (`krowk_home`) is not a repository's: a `no-sync` there
    /// opts out nothing (the device's switch is `sync.auto: false`).
    ///
    /// Paths are compared both as given and canonicalized (each falling
    /// back to itself when it cannot be), so a symlink on either side
    /// never lets a directory slip out of an opt-out. A marker that cannot
    /// be looked up for any reason but its absence (`EACCES` on a
    /// `.krowk`, say) counts as there. `cwd` is absolute, as `session_dir`
    /// gives it; a relative one would be read against this process's own
    /// directory. Blocking: a few `stat`s per ancestor.
    pub fn opted_out(&self, cwd: &Path, krowk_home: Option<&Path>) -> Option<OptOut> {
        debug_assert!(cwd.is_absolute(), "a session's directory is absolute: {}", cwd.display());
        let cwds = forms(cwd);
        for entry in &self.exclude {
            if forms(entry).iter().any(|e| cwds.iter().any(|c| c.starts_with(e))) {
                return Some(OptOut::Excluded(entry.clone()));
            }
        }
        let home = krowk_home.map(canonical);
        for c in &cwds {
            for dir in c.ancestors() {
                let dot = dir.join(".krowk");
                if home.as_ref().is_some_and(|h| canonical(&dot) == *h) {
                    continue;
                }
                let marker = dot.join("no-sync");
                match std::fs::symlink_metadata(&marker) {
                    // `.krowk` a file is no marker either.
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {}
                    _ => return Some(OptOut::Marker(marker)),
                }
            }
        }
        None
    }
}

/// What a JSON value is, for an error that should not repeat all of it.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The path as given and canonicalized, once when they are the same.
fn forms(p: &Path) -> Vec<PathBuf> {
    let c = canonical(p);
    if c == p { vec![c] } else { vec![c, p.to_path_buf()] }
}

/// A session's own directory, from its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDir {
    pub cwd: PathBuf,
    /// A subagent's session (its start names a parent).
    pub child: bool,
}

/// The directory session `id` runs in: the `cwd` of the `session.started`
/// its log in `sessions` begins with — never the directory of a client
/// that prompted it, since a resumed session keeps its own. `None` for an
/// id that is no session id, no log, a first line that does not parse, an
/// event of another kind or session, or a `cwd` that is not absolute.
/// **`None` is a directory unknown, so not synced**: a caller never reads
/// it as "not opted out".
///
/// Reads the first line only, so a `session.moved` later in the log (a
/// handoff, which syncs the session by a command of its own) is not
/// read here. Blocking: run it off any async runtime.
pub fn session_dir(sessions: &Path, id: &str) -> Option<SessionDir> {
    use std::io::BufRead;
    if !crate::log::valid_id(id) {
        return None;
    }
    let f = std::fs::File::open(sessions.join(id).join(crate::log::EVENTS_FILE)).ok()?;
    let first = std::io::BufReader::new(f).lines().next()?.ok()?;
    let e = serde_json::from_str::<crate::protocol::LogEvent>(&first).ok()?;
    match e.body {
        LogBody::SessionStarted { cwd, parent_session_id, .. } if e.session_id == id && Path::new(&cwd).is_absolute() => Some(SessionDir { cwd: PathBuf::from(cwd), child: parent_session_id.is_some() }),
        _ => None,
    }
}

/// Builds a bridge's options for `session`, whose own directory is `cwd`:
/// the keys, the signed registry client, the relay, direct paths. The
/// CLI's (ticket 95); a test's own. Blocking (it may ask the registry): the
/// supervisor runs it on the blocking pool. Errors are `(code, message)`.
pub type Build = Box<dyn Fn(&str, &Path) -> Result<host::Options, (String, String)> + Send + Sync>;

/// What the supervisor needs from its owner.
pub struct Config {
    /// The daemon's socket (`daemon::socket`), which each bridge connects
    /// to. Never started from here (`daemon::ensure` is not called): a
    /// supervisor whose daemon is exiting must not start the next one.
    pub socket: PathBuf,
    /// This machine's session logs (`log::sessions_dir`).
    pub sessions: PathBuf,
    /// Said in each bridge's hello: the daemon's own version.
    pub version: String,
    /// krowk's own home (`~/.krowk`), whose `no-sync` is no repository's.
    pub krowk_home: Option<PathBuf>,
    pub build: Build,
    /// The policy, read again each time a bridge would start, at each turn
    /// event of a bridged session and at each idle check (`policy` over
    /// `config.json`). An `Err` is off.
    pub policy: Box<dyn Fn() -> Result<Policy, String> + Send + Sync>,
    /// Whether sync is set up here; why not otherwise. File reads only.
    pub ready: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
    /// The sessions bridged now, shared with the daemon's status
    /// (`server::Options::bridged`).
    pub bridged: Arc<Mutex<BTreeSet<String>>>,
    /// How long sync stays off after a workspace's refusal for want of a
    /// paid plan (`PAID_PAUSE`; a test makes it short).
    pub paid_pause: Duration,
}

/// How long automatic sync makes no sync call after the registry says the
/// workspace has no paid plan (`sync_requires_paid_plan`). Not kept across
/// a restart: a restarted daemon asks again at its first turn.
pub const PAID_PAUSE: Duration = Duration::from_secs(60 * 60);

/// How long a session waits when another bridge holds its lease as its
/// bridge starts — a `krowk sync host` here or elsewhere, or a lease left
/// by a host that crashed, which lapses within its TTL.
pub const LEASE_HELD_WAIT: Duration = Duration::from_secs(60);

/// How long a session waits when another device took its lease while it
/// was bridged.
pub const LEASE_LOST_WAIT: Duration = Duration::from_secs(5 * 60);

/// The registry's refusal of a workspace without a paid plan, as a bridge's
/// error carries it: `build`'s `(code, _)`, and `take`'s message, both
/// name the code in brackets.
const PAID: &str = "(sync_requires_paid_plan)";

/// A failed bridge's first wait before it is started again; each failure
/// after a run no longer than its wait doubles it, up to `BACKOFF_MOST`.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(5);
pub const BACKOFF_MOST: Duration = Duration::from_secs(5 * 60);

/// How long the bridges are given to end when the `Handle` is dropped
/// rather than stopped.
const DROPPED_GRACE: Duration = Duration::from_secs(10);

/// The daemon's turn hook (`server::Options::turns`) and the supervisor's
/// end of it (`Supervisor::spawn`): unbounded, so the daemon's thread never
/// waits on the supervisor. Here so the daemon's owner (`krowk host serve`)
/// links no async runtime of its own to make it.
pub fn turn_hook() -> (mpsc::UnboundedSender<TurnEvent>, mpsc::UnboundedReceiver<TurnEvent>) {
    mpsc::unbounded_channel()
}

/// Starts a bridge for each session a turn starts in, as its policy allows.
pub struct Supervisor;

/// The running supervisor. Dropping it stops the bridges too, without
/// waiting for them.
pub struct Handle {
    stop: Option<oneshot::Sender<Duration>>,
    /// Said (or dropped) as the supervisor's thread ends.
    done: std::sync::mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Starts the supervisor on a thread of its own, `krowk-sync`, with a
    /// runtime of its own, listening to the daemon's turn hook `turns`.
    /// Nothing runs on the daemon's thread (see the module's doc).
    pub fn spawn(c: Config, turns: mpsc::UnboundedReceiver<TurnEvent>) -> Handle {
        let (stop, stop_rx) = oneshot::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("krowk-sync".into()).spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => {
                    let bridged = c.bridged.clone();
                    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(Sup::new(c).run(turns, stop_rx))));
                    // A check or build still on the blocking pool (a dead
                    // disk, a registry that never answers) is not waited for,
                    // even after a panic: a runtime merely dropped would.
                    rt.shutdown_background();
                    if ran.is_err() {
                        eprintln!("krowk: automatic sync stopped: its supervisor panicked; `krowk sync host` still syncs a session");
                        lock(&bridged).clear();
                    }
                }
                Err(e) => eprintln!("krowk: automatic sync could not start: the async runtime did not: {e}"),
            }
            let _ = done_tx.send(());
        });
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("krowk: automatic sync could not start: no thread for it: {e}");
                None
            }
        };
        Handle { stop: Some(stop), done, thread }
    }
}

impl Handle {
    /// Stops every bridge — each ends its stream and gives its lease back
    /// (`Hosting::stop`) — and waits for them up to `grace`. A bridge still
    /// going then is left to the process's exit, its lease to lapse.
    pub fn stop(mut self, grace: Duration) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(grace);
        }
        // `JoinHandle::join` has no timeout: the thread says it is done.
        // It waits `grace` for the bridges itself; a moment more for its end.
        match self.done.recv_timeout(grace + Duration::from_secs(1)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => eprintln!("krowk: automatic sync did not stop in {}; leaving it", human(grace)),
        }
    }
}

/// Why a session is not bridged.
enum No {
    /// Nothing is, here: the switch, the policy, the setup.
    Device(String),
    /// This session is not.
    Session(String),
}

/// Whether `session` may be bridged, in which directory, and the idle
/// window it read: the policy on, sync set up, the session's directory
/// known, not a subagent's, and not opted out — in that order, the first no
/// ending it. Blocking.
fn check(c: &Config, session: &str) -> Result<(PathBuf, Option<Duration>), No> {
    let p = match (c.policy)() {
        Ok(p) if p.on => p,
        Ok(p) => return Err(No::Device(format!("automatic sync is off ({}); a session syncs only with `krowk sync host`", p.off_by.unwrap_or("turned off")))),
        // Fail closed: a policy that cannot be read is off.
        Err(e) => return Err(No::Device(format!("automatic sync is off: {e}"))),
    };
    (c.ready)().map_err(|e| No::Device(format!("automatic sync is not set up here: {e}")))?;
    let dir = session_dir(&c.sessions, session).ok_or_else(|| No::Session("its log does not say which directory it runs in".into()))?;
    if dir.child {
        return Err(No::Session("it is a subagent's, which rides its parent's bridge".into()));
    }
    match p.opted_out(&dir.cwd, c.krowk_home.as_deref()) {
        None => Ok((dir.cwd, p.idle)),
        Some(OptOut::Excluded(e)) => Err(No::Session(format!("{} is under {}, in sync.exclude", dir.cwd.display(), e.display()))),
        Some(OptOut::Marker(m)) => Err(No::Session(format!("{} opts {} out", m.display(), dir.cwd.display()))),
    }
}

/// A session's checks, answered: its directory and idle window, or why not.
type Checked = (String, Result<(PathBuf, Option<Duration>), No>);

/// What opted a directory out, for the line a stopped bridge leaves.
fn source(o: &OptOut) -> String {
    match o {
        OptOut::Excluded(e) => format!("{} in sync.exclude", e.display()),
        OptOut::Marker(m) => m.display().to_string(),
    }
}

/// A rescan's answer: the policy read again, and for each bridged session
/// whose directory was looked at, whether it opts out now.
type Rescan = (Result<Policy, String>, Vec<(String, Option<OptOut>)>);

/// The policy read again, and each of `dirs` looked at under it when it
/// is on. Blocking.
fn rescan(c: &Config, dirs: Vec<(String, PathBuf)>) -> Rescan {
    let p = (c.policy)();
    let outs = match &p {
        Ok(p) if p.on => dirs.into_iter().map(|(s, cwd)| {
            let out = p.opted_out(&cwd, c.krowk_home.as_deref());
            (s, out)
        }).collect(),
        _ => Vec::new(),
    };
    (p, outs)
}

/// When a bridge with no turn running, quiet since `quiet`, is let go
/// under the idle window `idle`: never while a turn runs, while it is
/// stopping already, or with no window. Pure: the clock is the caller's.
fn idle_at(quiet: Instant, running: bool, stopping: bool, idle: Option<Duration>) -> Option<Instant> {
    match idle {
        Some(idle) if !running && !stopping => quiet.checked_add(idle),
        _ => None,
    }
}

/// The fix a paid-plan refusal carries in a bridge's `error`: after the
/// code in `take`'s message, before it in `build`'s; the registry's own
/// words otherwise.
fn paid_fix(error: &str) -> String {
    let fix = match error.find(PAID) {
        Some(i) if error[i + PAID.len()..].starts_with(": ") => error[i + PAID.len() + 2..].trim(),
        Some(i) if error[i + PAID.len()..].trim().is_empty() => error[..i].trim(),
        _ => "",
    };
    if fix.is_empty() { "upgrade this workspace to Pro at https://app.krowk.com".into() } else { fix.into() }
}

/// One bridge: its options built, its own daemon client connected in the
/// session's own directory — a background one, so it never keeps the
/// daemon up, and answering, so viewers and the phone can approve — and
/// `host::run` until `stop` or its end.
async fn bridge(c: Arc<Config>, session: String, cwd: PathBuf, stop: watch::Receiver<bool>) -> Result<(), String> {
    let built = {
        let (c, session, cwd) = (c.clone(), session.clone(), cwd.clone());
        tokio::task::spawn_blocking(move || (c.build)(&session, &cwd)).await
    };
    let o = match built {
        Ok(o) => o.map_err(|(code, message)| format!("{message} ({code})"))?,
        // A panic in `build` is this bridge's, and only its.
        Err(e) => match e.try_into_panic() {
            Ok(p) => std::panic::resume_unwind(p),
            Err(e) => return Err(e.to_string()),
        },
    };
    if *stop.borrow() {
        return Ok(());
    }
    let daemon = Daemon::connect_as(&c.socket, &cwd, &c.version, true, true).await.map_err(|e| match e {
        crate::daemon::client::ConnectError::Absent => "the host daemon does not answer".to_string(),
        crate::daemon::client::ConnectError::Failed(e) => format!("the host daemon refused it: {}", e.message),
    })?;
    if *stop.borrow() {
        return Ok(());
    }
    lock(&c.bridged).insert(session.clone());
    // Kept for the bridge's life: dropped, `on_demand` would close and
    // `run` spin on it.
    let (_checkpoint, on_demand) = mpsc::unbounded_channel();
    host::run(o, Arc::new(daemon), stop, on_demand).await
}

fn lock(b: &Mutex<BTreeSet<String>>) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
    b.lock().unwrap_or_else(|e| e.into_inner())
}

/// A session the supervisor has: its checks running, or its bridge.
enum Has {
    /// Its checks run, begun under this policy generation.
    Checking(u64),
    Bridging {
        stop: watch::Sender<bool>,
        since: Instant,
        /// The session's own directory, looked at again by each rescan.
        cwd: PathBuf,
        /// When its last turn ended, or the bridge started.
        quiet: Instant,
        /// Told to stop by the supervisor (idle, opted out, off, the paid
        /// plan): its end is no failure.
        stopping: bool,
        /// A turn started while it was stopping: taken up again once it
        /// has stopped.
        again: bool,
    },
}

/// The supervisor's own state, on its own thread.
struct Sup {
    c: Arc<Config>,
    has: HashMap<String, Has>,
    checks: JoinSet<Checked>,
    bridges: JoinSet<Result<(), String>>,
    of_task: HashMap<tokio::task::Id, String>,
    /// Sessions backing off.
    backing: HashMap<String, Wait>,
    /// Each failed session's last wait, for the next.
    waits: HashMap<String, Duration>,
    /// Sessions whose turn runs now, as the daemon's hook tells.
    running: HashSet<String>,
    /// The idle window the policy said last.
    idle: Option<Duration>,
    /// A rescan reading the policy again; at most one at a time.
    rescans: JoinSet<Rescan>,
    rescan_again: bool,
    /// Counts the rescans answered: a check begun under an older one is
    /// read again once bridged.
    generation: u64,
    /// Until when nothing is bridged: the workspace has no paid plan.
    paused: Option<Instant>,
    /// The last thing said of the device, and of each session, so a reason
    /// is said once per change rather than once per turn.
    said: Option<String>,
    said_of: HashMap<String, String>,
    /// The same for a session left to another's lease, forgotten only once
    /// its own bridge has held it (and been let go).
    said_lease: HashMap<String, String>,
    /// The daemon's hook closed: it is gone, and nothing starts again.
    gone: bool,
}

/// Sessions whose last reason is remembered, past which the oldest are
/// forgotten (and may be said again).
const SAID_MOST: usize = 4096;

/// How long a rescan may take to read the policy and the bridged sessions'
/// directories (a hung network mount, a dead disk): past it, the policy
/// counts as unreadable, so off, and every bridge stops.
const RESCAN_MOST: Duration = Duration::from_secs(10);

/// A session backing off.
struct Wait {
    /// When its wait runs out.
    at: Instant,
    /// Another holds its lease: started again only for a turn.
    lazy: bool,
    /// When its last turn ended (or its bridge started).
    quiet: Instant,
    /// A turn started in it while it waited.
    wanted: bool,
}

/// Whether a session whose wait ran out at `now` is started again: when a
/// turn runs in it or started while it waited; when it waited for another's
/// lease, only then; otherwise also while its last turn is within the idle
/// window, so sync resumes by itself (R-OFF-2) for a session in use, and a
/// session nobody uses makes no call until its next turn. Pure.
fn wanted_again(w: &Wait, running: bool, idle: Option<Duration>, now: Instant) -> bool {
    running || w.wanted || (!w.lazy && idle_at(w.quiet, false, false, idle).is_none_or(|at| now < at))
}

/// Whether a bridge's error is the registry's refusal of a workspace
/// without a paid plan: the code in brackets, as `take`'s, a refused
/// renewal's and `build`'s errors name it. A handoff's end names a device,
/// which could say anything, so it never is.
fn unpaid(error: &str) -> bool {
    !error.starts_with(host::HANDED_OVER) && error.contains(PAID)
}

/// How a person reads a pause: an hour, or what `human` says.
fn for_how_long(d: Duration) -> String {
    if d == Duration::from_secs(3600) { "an hour".into() } else { human(d) }
}

impl Sup {
    fn new(c: Config) -> Sup {
        Sup {
            c: Arc::new(c),
            has: HashMap::new(),
            checks: JoinSet::new(),
            bridges: JoinSet::new(),
            of_task: HashMap::new(),
            backing: HashMap::new(),
            waits: HashMap::new(),
            running: HashSet::new(),
            idle: None,
            rescans: JoinSet::new(),
            rescan_again: false,
            generation: 0,
            paused: None,
            said: None,
            said_of: HashMap::new(),
            said_lease: HashMap::new(),
            gone: false,
        }
    }

    async fn run(mut self, mut turns: mpsc::UnboundedReceiver<TurnEvent>, mut stop: oneshot::Receiver<Duration>) {
        let mut hooked = true;
        let grace = loop {
            // One timer, for the first of the backoffs and the idle
            // deadlines — none at all unless something backs off or is
            // bridged (R-PERF-2).
            let next = self.next_due();
            let due = async move {
                match next {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                // A dropped handle stops it too.
                g = &mut stop => break g.unwrap_or(DROPPED_GRACE),
                t = turns.recv(), if hooked => match t {
                    Some(TurnEvent { session, running }) => self.turn(session, running),
                    // The daemon is gone: the bridges end by themselves
                    // (`sweep` sees their connection closed), and nothing
                    // is started again — there is no daemon to follow.
                    None => {
                        hooked = false;
                        self.gone = true;
                        self.backing.clear();
                        self.waits.clear();
                        self.running.clear();
                    }
                },
                Some(r) = self.checks.join_next(), if !self.checks.is_empty() => self.checked(r),
                Some(r) = self.bridges.join_next_with_id(), if !self.bridges.is_empty() => self.ended(r),
                Some(r) = self.rescans.join_next(), if !self.rescans.is_empty() => self.rescanned(r),
                () = due => self.due(),
            }
        };
        self.stop_all(grace).await;
    }

    /// The first backoff to run out, or the first bridge to sit idle past
    /// the window — that one not while a rescan reads the policy, whose
    /// answer lets it go.
    fn next_due(&self) -> Option<Instant> {
        let idle = if self.rescans.is_empty() { self.idle_deadlines().min() } else { None };
        self.backing.values().map(|w| w.at).chain(idle).min()
    }

    fn idle_deadlines(&self) -> impl Iterator<Item = Instant> + '_ {
        self.has.iter().filter_map(|(s, h)| match h {
            Has::Bridging { quiet, stopping, .. } => idle_at(*quiet, self.running.contains(s), *stopping, self.idle),
            Has::Checking(_) => None,
        })
    }

    fn due(&mut self) {
        self.retry();
        let now = Instant::now();
        if self.idle_deadlines().any(|at| at <= now) {
            self.rescan();
        }
    }

    /// The daemon's hook: a turn of `session` started (`running`) or ended.
    fn turn(&mut self, session: String, running: bool) {
        if running {
            self.running.insert(session.clone());
        } else {
            self.running.remove(&session);
        }
        match self.has.get_mut(&session) {
            Some(Has::Bridging { quiet, stopping, again, .. }) => {
                *quiet = Instant::now();
                if !*stopping {
                    self.rescan();
                } else if running {
                    *again = true;
                }
            }
            // Read again before it is bridged: an opt-out may have come
            // since its check read the policy.
            Some(Has::Checking(g)) => *g = u64::MAX,
            None => match self.backing.get_mut(&session) {
                // Its wait runs: a turn meanwhile is why to start it again.
                Some(w) if running => w.wanted = true,
                Some(w) => w.quiet = Instant::now(),
                None if running => self.start(session),
                None => {}
            },
        }
    }

    /// Whether sync is off for want of a paid plan; the pause forgotten
    /// once it has run out.
    fn paused(&mut self) -> bool {
        match self.paused {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                self.paused = None;
                false
            }
            None => false,
        }
    }

    /// A turn started in `session`, or its wait ran out: checked, then
    /// bridged — unless it is already, backs off, or sync is paused.
    fn start(&mut self, session: String) {
        if self.has.contains_key(&session) || self.backing.contains_key(&session) || self.paused() {
            return;
        }
        self.has.insert(session.clone(), Has::Checking(self.generation));
        let c = self.c.clone();
        self.checks.spawn(async move {
            let s = session.clone();
            let r = tokio::task::spawn_blocking(move || check(&c, &s)).await;
            // A check that panicked is a no: fail closed.
            (session, r.unwrap_or_else(|e| Err(No::Session(format!("its checks failed: {e}")))))
        });
    }

    fn checked(&mut self, r: Result<Checked, JoinError>) {
        // Only `abort_all` ends a check task without its answer, but a
        // session must never be left checking for good.
        let Ok((session, r)) = r else {
            self.has.retain(|_, h| !matches!(h, Has::Checking(_)));
            return;
        };
        // A rescan that stopped something answered while it was checked —
        // sync off, or a directory opted out — or a turn event came: its
        // answer may be out of date, so it is checked again rather than
        // bridged on it.
        let stale = matches!(self.has.get(&session), Some(Has::Checking(g)) if *g != self.generation);
        match r {
            // Paused since the check began: nothing is sent.
            Ok(_) if self.paused() => {
                self.has.remove(&session);
            }
            Ok(_) if stale => {
                self.has.remove(&session);
                self.start(session);
            }
            Ok((cwd, idle)) => {
                self.said = None;
                self.said_of.remove(&session);
                self.idle = idle;
                let (stop, stop_rx) = watch::channel(false);
                let task = self.bridges.spawn(bridge(self.c.clone(), session.clone(), cwd.clone(), stop_rx));
                self.of_task.insert(task.id(), session.clone());
                let now = Instant::now();
                self.has.insert(session, Has::Bridging { stop, since: now, cwd, quiet: now, stopping: false, again: false });
            }
            Err(no) => {
                self.has.remove(&session);
                self.waits.remove(&session);
                match no {
                    No::Device(why) => {
                        if self.said.as_ref() != Some(&why) {
                            eprintln!("krowk: {why}");
                            self.said = Some(why);
                        }
                    }
                    No::Session(why) => {
                        if self.said_of.get(&session) != Some(&why) {
                            eprintln!("krowk: session {session} is not synced automatically: {why}");
                            if self.said_of.len() >= SAID_MOST {
                                self.said_of.clear();
                            }
                            self.said_of.insert(session, why);
                        }
                    }
                }
            }
        }
    }

    /// The policy read again, off the runtime, with every bridged
    /// session's directory: at a bridged session's turn event, and when a
    /// bridge's idle deadline comes. One at a time; one asked for meanwhile
    /// runs after it.
    fn rescan(&mut self) {
        if !self.rescans.is_empty() {
            self.rescan_again = true;
            return;
        }
        let dirs: Vec<(String, PathBuf)> = self
            .has
            .iter()
            .filter_map(|(s, h)| match h {
                Has::Bridging { cwd, stopping: false, .. } => Some((s.clone(), cwd.clone())),
                _ => None,
            })
            .collect();
        let c = self.c.clone();
        self.rescans.spawn(async move {
            // A rescan that panicked reads as a policy that does not: off.
            match tokio::time::timeout(RESCAN_MOST, tokio::task::spawn_blocking(move || rescan(&c, dirs))).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => (Err(format!("its policy could not be read again: {e}")), Vec::new()),
                Err(_) => (Err(format!("its policy and the sessions' directories took over {} to read", human(RESCAN_MOST))), Vec::new()),
            }
        });
    }

    fn rescanned(&mut self, r: Result<Rescan, JoinError>) {
        let (policy, outs) = r.unwrap_or_else(|e| (Err(format!("its policy could not be read again: {e}")), Vec::new()));
        // Checks begun before it are out of date only when it stops something.
        if !matches!(&policy, Ok(p) if p.on) || outs.iter().any(|(_, o)| o.is_some()) {
            self.generation += 1;
        }
        match policy {
            Ok(p) if p.on => {
                self.idle = p.idle;
                for (session, out) in outs {
                    if let Some(out) = out
                        && self.stop_one(&session)
                    {
                        eprintln!("krowk: stopped syncing session {session}: opted out by {}", source(&out));
                    }
                }
            }
            // Fail closed: off, or a policy that does not read, stops all.
            Ok(p) => self.stop_every(&format!("automatic sync is off ({})", p.off_by.unwrap_or("turned off"))),
            Err(e) => self.stop_every(&format!("automatic sync is off: {e}")),
        }
        self.release_idle(Instant::now());
        if std::mem::take(&mut self.rescan_again) {
            self.rescan();
        }
    }

    /// Every bridge with no turn running, idle past the window at `now`,
    /// told to stop: its next turn bridges it again.
    fn release_idle(&mut self, now: Instant) {
        let Some(idle) = self.idle else { return };
        let due: Vec<String> = self
            .has
            .iter()
            .filter(|(s, h)| matches!(h, Has::Bridging { quiet, stopping, .. } if idle_at(*quiet, self.running.contains(*s), *stopping, Some(idle)).is_some_and(|at| at <= now)))
            .map(|(s, _)| s.clone())
            .collect();
        for s in due {
            if self.stop_one(&s) {
                eprintln!("krowk: stopped syncing session {s}: nothing ran in it for {}; its next turn syncs it again", human(idle));
            }
        }
    }

    /// `session`'s bridge told to stop — it seals its stream and gives the
    /// lease back (`Hosting::stop`) — unless it is stopping already.
    fn stop_one(&mut self, session: &str) -> bool {
        match self.has.get_mut(session) {
            Some(Has::Bridging { stop, stopping, .. }) if !*stopping => {
                *stopping = true;
                let _ = stop.send(true);
                true
            }
            _ => false,
        }
    }

    /// Every bridge told to stop, `why` said once if any was.
    fn stop_every(&mut self, why: &str) {
        let sessions: Vec<String> = self.has.keys().cloned().collect();
        let stopped = sessions.iter().filter(|s| self.stop_one(s)).count();
        if stopped > 0 {
            eprintln!("krowk: stopped syncing {stopped} session(s): {why}");
        }
    }

    /// The workspace has no paid plan: every bridge stopped, and nothing
    /// bridged, so no sync call made, for `paid_pause`.
    fn pause(&mut self, error: &str) {
        if !self.paused() {
            eprintln!("krowk: automatic sync is off for {}: this workspace has no paid plan ({})", for_how_long(self.c.paid_pause), paid_fix(error));
            self.paused = Some(Instant::now() + self.c.paid_pause);
        }
        let sessions: Vec<String> = self.has.keys().cloned().collect();
        for s in sessions {
            self.stop_one(&s);
        }
        self.backing.clear();
        self.waits.clear();
    }

    /// A bridge ended: by itself, or as the supervisor told it to (idle,
    /// opted out, off, the paid plan) — `stop_all` waits for its own.
    fn ended(&mut self, r: Result<(tokio::task::Id, Result<(), String>), JoinError>) {
        let (id, r) = match r {
            Ok((id, r)) => (id, r),
            Err(e) => (e.id(), Err(panicked(e))),
        };
        let Some(session) = self.of_task.remove(&id) else { return };
        let (since, quiet, stopping, again) = match self.has.remove(&session) {
            Some(Has::Bridging { since, quiet, stopping, again, .. }) => (since, quiet, stopping, again),
            _ => (Instant::now(), Instant::now(), false, false),
        };
        lock(&self.c.bridged).remove(&session);
        if let Err(e) = &r
            && unpaid(e)
        {
            self.pause(e);
            return;
        }
        if stopping {
            // A turn that started as it stopped takes it up again (unless
            // what stopped it still stands: `check` says).
            self.waits.remove(&session);
            self.said_lease.remove(&session);
            if (again || self.running.contains(&session)) && !self.gone {
                self.start(session);
            }
            return;
        }
        match r {
            // The device list moved on: built again at once, so the new list
            // is verified and its user key taken up (as `krowk sync host`).
            Err(e) if e.starts_with(host::LIST_MOVED) => {
                eprintln!("krowk: automatic sync of session {session} starts again: {e}");
                self.start(session);
            }
            // The session moved to another device: its sync is there now.
            Err(e) if e.starts_with(host::HANDED_OVER) => {
                eprintln!("krowk: automatic sync of session {session} ended: {e}");
                self.waits.remove(&session);
            }
            // Another bridge holds the lease — a `krowk sync host`, or a
            // crashed host's, which lapses: no error, and never taken from it.
            Err(e) if e.starts_with(host::NOT_GRANTED) && e.contains("(lease_held)") => self.wait_for_lease(session, quiet, LEASE_HELD_WAIT, "another bridge holds its lease"),
            Err(e) if e.starts_with(host::LEASE_LOST) => self.wait_for_lease(session, quiet, LEASE_LOST_WAIT, "another device took its lease"),
            // Any other end — an error, a panic, an end it gave no reason
            // for — is this session's alone: it backs off.
            r => {
                let why = r.err().unwrap_or_else(|| "it ended with no error".into());
                let ran = since.elapsed();
                let wait = match self.waits.get(&session) {
                    Some(&w) if ran <= w => (w * 2).min(BACKOFF_MOST),
                    _ => BACKOFF_FIRST,
                };
                if self.gone {
                    eprintln!("krowk: automatic sync of session {session} stopped: {why}");
                    return;
                }
                eprintln!("krowk: automatic sync of session {session} stopped: {why}; trying again in {}", human(wait));
                self.waits.insert(session.clone(), wait);
                self.backing.insert(session, Wait { at: Instant::now() + wait, lazy: false, quiet, wanted: false });
            }
        }
    }

    /// `session` left alone for `wait` — its lease is another's — and
    /// then taken up at its next turn, or at once if one ran meanwhile.
    fn wait_for_lease(&mut self, session: String, quiet: Instant, wait: Duration, why: &str) {
        self.waits.remove(&session);
        if self.gone {
            return;
        }
        // Said once while the reason stands, not at every try.
        let line = format!("{why}; it is left to it, and tried again at a turn after {}", human(wait));
        if self.said_lease.get(&session) != Some(&line) {
            eprintln!("krowk: automatic sync skips session {session}: {line}");
            if self.said_lease.len() >= SAID_MOST {
                self.said_lease.clear();
            }
            self.said_lease.insert(session.clone(), line);
        }
        self.backing.insert(session, Wait { at: Instant::now() + wait, lazy: true, quiet, wanted: false });
    }

    /// Every session whose wait ran out, started again if it is still
    /// wanted (`wanted_again`); otherwise left for its next turn.
    fn retry(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self.backing.iter().filter(|(_, w)| w.at <= now).map(|(s, _)| s.clone()).collect();
        for s in due {
            let Some(w) = self.backing.remove(&s) else { continue };
            if wanted_again(&w, self.running.contains(&s), self.idle, now) {
                self.start(s);
            } else {
                self.waits.remove(&s);
            }
        }
    }

    /// Every bridge told to stop, and waited for up to `grace`.
    async fn stop_all(mut self, grace: Duration) {
        self.checks.abort_all();
        self.rescans.abort_all();
        for h in self.has.values() {
            if let Has::Bridging { stop, .. } = h {
                let _ = stop.send(true);
            }
        }
        let deadline = Instant::now() + grace;
        while !self.bridges.is_empty() {
            match tokio::time::timeout_at(deadline, self.bridges.join_next_with_id()).await {
                Ok(Some(r)) => {
                    let id = match &r {
                        Ok((id, _)) => *id,
                        Err(e) => e.id(),
                    };
                    if let Some(s) = self.of_task.remove(&id) {
                        lock(&self.c.bridged).remove(&s);
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    eprintln!("krowk: automatic sync left {} bridge(s) that did not stop in {}", self.bridges.len(), human(grace));
                    break;
                }
            }
        }
        // Whatever is left is no longer bridged: the runtime goes with it.
        let mut b = lock(&self.c.bridged);
        for s in self.of_task.values() {
            b.remove(s);
        }
    }
}

/// Why a bridge's task ended without its own answer: what it panicked with.
fn panicked(e: JoinError) -> String {
    match e.try_into_panic() {
        Ok(p) => match p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()) {
            Some(m) => format!("its bridge panicked: {m}"),
            None => "its bridge panicked".into(),
        },
        Err(e) => e.to_string(),
    }
}

/// A wait for a person: `5 s`, `1 min 20 s`, `5 min`.
fn human(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 60, s % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env<'a>(pairs: &'a [(&'a str, String)]) -> impl Fn(&str) -> String + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_default()
    }

    const NONE: [(&str, String); 0] = [];

    /// R-SYNC-3: on unless the device turns it off. R-SYNC-1: the policy is
    /// the environment and the person's own config, read with no account
    /// and nothing on the network; whether a paid account is there is the
    /// supervisor's question (ticket 94), not this one's.
    #[test]
    fn r_sync_3_the_switch_comes_from_the_env_then_config_and_is_on_by_default() {
        let p = policy(&env(&NONE), None).unwrap();
        assert_eq!((p.on, p.off_by), (true, None), "R-SYNC-3: synced without a command");
        assert!(policy(&env(&NONE), Some(&json!({}))).unwrap().on);
        assert!(policy(&env(&NONE), Some(&json!({"sync": {}}))).unwrap().on);
        assert!(policy(&env(&NONE), Some(&json!({"sync": {"auto": true}}))).unwrap().on);
        let off = json!({"sync": {"auto": false}});
        let p = policy(&env(&NONE), Some(&off)).unwrap();
        assert_eq!((p.on, p.off_by), (false, Some("sync.auto in config.json")));

        for s in ["0", "false", "off", "OFF", " False "] {
            let e = [("KROWK_SYNC_AUTO", s.to_string())];
            let p = policy(&env(&e), Some(&json!({"sync": {"auto": true}}))).unwrap();
            assert_eq!((p.on, p.off_by), (false, Some("KROWK_SYNC_AUTO")), "{s:?} turns it off, over config");
        }
        for s in ["1", "true", "on", "On"] {
            let e = [("KROWK_SYNC_AUTO", s.to_string())];
            let p = policy(&env(&e), Some(&off)).unwrap();
            assert_eq!((p.on, p.off_by), (true, None), "{s:?} turns it on, over config");
        }

        let e = [("KROWK_SYNC_AUTO", "maybe".to_string())];
        let err = policy(&env(&e), None).unwrap_err();
        assert!(err.contains("KROWK_SYNC_AUTO") && err.contains("\"maybe\"") && err.contains("off"), "{err}");
        for bad in [json!({"sync": {"auto": "no"}}), json!({"sync": {"auto": 0}}), json!({"sync": {"auto": null}})] {
            let err = policy(&env(&NONE), Some(&bad)).unwrap_err();
            assert!(err.contains("sync.auto in config.json"), "{err}");
        }
        // `"sync": false` is no object, and not read as absent (which is on).
        for bad in [json!({"sync": false}), json!({"sync": "off"}), json!({"sync": null})] {
            assert!(policy(&env(&NONE), Some(&bad)).unwrap_err().contains("sync in config.json"), "{bad}");
        }
        for bad in [json!(null), json!([]), json!(false)] {
            assert!(policy(&env(&NONE), Some(&bad)).unwrap_err().contains("config.json is"), "{bad}");
        }
        // A misspelt opt-out is refused, never read as nothing said.
        for (bad, named) in [(json!({"sync": {"Auto": false}}), "sync.Auto"), (json!({"sync": {"excludes": ["/w"]}}), "sync.excludes"), (json!({"sync": {"auto": false, "enabled": false}}), "sync.enabled")] {
            let err = policy(&env(&NONE), Some(&bad)).unwrap_err();
            assert!(err.contains(named) && err.contains("not a setting"), "{err}");
        }
        // Every value is checked, the switch's override too: still an Err, so off.
        let e = [("KROWK_SYNC_AUTO", "0".to_string())];
        assert!(policy(&env(&e), Some(&json!({"sync": {"exclude": "/w"}}))).is_err());
        let e = [("KROWK_SYNC_AUTO", "1".to_string()), ("KROWK_SYNC_IDLE", "5".to_string())];
        assert!(policy(&env(&e), Some(&json!({"sync": {"auto": "no"}}))).unwrap_err().contains("sync.auto"), "a bad value under the override is still refused");
        assert!(policy(&env(&e), Some(&json!({"sync": {"idleMinutes": 0.005}}))).unwrap_err().contains("sync.idleMinutes"));
    }

    #[test]
    fn r_sync_3_the_exclude_list_takes_absolute_paths_only() {
        assert!(policy(&env(&NONE), None).unwrap().exclude.is_empty());
        let cfg = json!({"sync": {"exclude": ["/work/secret", "/home/me/client/"]}});
        assert_eq!(policy(&env(&NONE), Some(&cfg)).unwrap().exclude, vec![PathBuf::from("/work/secret"), PathBuf::from("/home/me/client/")]);
        for (entry, named) in [(json!("work/secret"), "\"work/secret\""), (json!("~/secret"), "\"~/secret\""), (json!(""), "\"\""), (json!(7), "7"), (json!({"path": "/x"}), "{\"path\":\"/x\"}")] {
            let cfg = json!({"sync": {"exclude": ["/ok", entry]}});
            let err = policy(&env(&NONE), Some(&cfg)).unwrap_err();
            assert!(err.contains("sync.exclude in config.json") && err.contains(named), "{err}");
        }
        for not_a_list in [json!("/work/secret"), json!({"/work": true}), json!(null)] {
            let err = policy(&env(&NONE), Some(&json!({"sync": {"exclude": not_a_list}}))).unwrap_err();
            assert!(err.contains("sync.exclude in config.json") && err.contains("not a list"), "{err}");
        }
    }

    #[test]
    fn r_sync_3_the_idle_window_comes_from_the_env_then_config() {
        assert_eq!(policy(&env(&NONE), None).unwrap().idle, Some(DEFAULT_IDLE));
        assert_eq!(DEFAULT_IDLE, Duration::from_secs(600));
        let cfg = json!({"sync": {"idleMinutes": 2}});
        assert_eq!(policy(&env(&NONE), Some(&cfg)).unwrap().idle, Some(Duration::from_secs(120)));
        let e = [("KROWK_SYNC_IDLE", "3".to_string())];
        assert_eq!(policy(&env(&e), Some(&cfg)).unwrap().idle, Some(Duration::from_secs(3)), "the env wins");
        let e = [("KROWK_SYNC_IDLE", "0".to_string())];
        assert_eq!(policy(&env(&e), Some(&cfg)).unwrap().idle, None, "0 never lets go");
        assert_eq!(policy(&env(&NONE), Some(&json!({"sync": {"idleMinutes": 0}}))).unwrap().idle, None);
        for tiny in [json!(0.005), json!(-1), json!("10")] {
            let err = policy(&env(&NONE), Some(&json!({"sync": {"idleMinutes": tiny}}))).unwrap_err();
            assert!(err.contains("sync.idleMinutes in config.json") && err.contains("at least one second"), "not rounded down to never: {err}");
        }
        let e = [("KROWK_SYNC_IDLE", u64::MAX.to_string())];
        assert_eq!(policy(&env(&e), None).unwrap().idle, None, "past a century is never, and no Instant overflows");
        assert_eq!(policy(&env(&NONE), Some(&json!({"sync": {"idleMinutes": 1e300}}))).unwrap().idle, None);
        for bad in ["soon", "-1", "1.5"] {
            let e = [("KROWK_SYNC_IDLE", bad.to_string())];
            assert!(policy(&env(&e), None).unwrap_err().contains("KROWK_SYNC_IDLE"), "{bad}");
        }
    }

    #[test]
    fn r_sync_3_a_directory_opts_out_by_the_list_or_a_marker_at_any_ancestor() {
        let root = std::env::temp_dir().join(format!("krowk-sync3-optout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root = {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::canonicalize(&root).unwrap()
        };
        let mk = |p: &str| {
            let d = root.join(p);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let (secret, deep, sibling) = (mk("work/a"), mk("work/a/src/x"), mk("work/ab"));
        let (repo_sub, high_sub) = (mk("repo/crates/core"), mk("mono/team/svc/src"));
        mk("repo/.krowk");
        std::fs::write(root.join("repo/.krowk/no-sync"), "").unwrap();
        mk("mono/.krowk/no-sync"); // a directory marker, two levels above the repository at svc
        mk("mono/team/svc/.krowk");
        // krowk's own home is `home/.krowk`; its no-sync covers nothing.
        let (krowk_home, in_home) = (mk("home/.krowk"), mk("home/code/proj"));
        mk("home/.krowk/no-sync");

        let p = Policy { on: true, off_by: None, exclude: vec![secret.clone()], idle: None };
        let home = Some(krowk_home.as_path());
        assert_eq!(p.opted_out(&secret, home), Some(OptOut::Excluded(secret.clone())));
        assert_eq!(p.opted_out(&deep, home), Some(OptOut::Excluded(secret.clone())), "its subdirectories too");
        assert_eq!(p.opted_out(&sibling, home), None, "whole components: /work/a does not take /work/ab");
        assert_eq!(p.opted_out(&root.join("repo"), home), Some(OptOut::Marker(root.join("repo/.krowk/no-sync"))));
        assert_eq!(p.opted_out(&repo_sub, home), Some(OptOut::Marker(root.join("repo/.krowk/no-sync"))), "from the repository's root");
        assert_eq!(p.opted_out(&high_sub, home), Some(OptOut::Marker(root.join("mono/.krowk/no-sync"))), "from two levels above it, as a directory");
        assert_eq!(p.opted_out(&in_home, home), None, "krowk's home is no repository");
        assert_eq!(p.opted_out(&root.join("home"), home), None);
        assert_eq!(p.opted_out(&in_home, None), Some(OptOut::Marker(root.join("home/.krowk/no-sync"))), "without the home given, it is just a .krowk");

        // A `.krowk` that is a file holds no marker; one that cannot be
        // looked into counts as one.
        let (plain, shut) = (mk("plain/src"), mk("shut/src"));
        std::fs::write(root.join("plain/.krowk"), "").unwrap();
        assert_eq!(p.opted_out(&plain, home), None);
        mk("shut/.krowk");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("shut/.krowk"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::symlink_metadata(root.join("shut/.krowk/no-sync")).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        let got = p.opted_out(&shut, home);
        std::fs::set_permissions(root.join("shut/.krowk"), std::fs::Permissions::from_mode(0o700)).unwrap();
        if unreadable {
            assert_eq!(got, Some(OptOut::Marker(root.join("shut/.krowk/no-sync"))), "fails closed");
        } // else root, which reads through any mode

        // A symlink on either side does not slip out.
        let link = root.join("link-to-a");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert_eq!(p.opted_out(&link.join("src"), home), Some(OptOut::Excluded(secret.clone())), "a cwd reached through a link");
        let by_link = Policy { exclude: vec![link.clone()], ..p.clone() };
        assert_eq!(by_link.opted_out(&deep, home), Some(OptOut::Excluded(link.clone())), "an entry that is a link");
        // The entry is named as written, even where it does not exist.
        let gone = Policy { exclude: vec![root.join("nowhere")], ..p.clone() };
        assert_eq!(gone.opted_out(&root.join("nowhere/x"), home), Some(OptOut::Excluded(root.join("nowhere"))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn r_sync_3_a_sessions_directory_is_its_logs_own() {
        let sessions = std::env::temp_dir().join(format!("krowk-sync3-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sessions);
        let (root, kid, garbage, other, foreign, relative, absent) = (
            "01a0ec7b-0000-7000-8000-0000000000b1",
            "01a0ec7b-0000-7000-8000-0000000000b2",
            "01a0ec7b-0000-7000-8000-0000000000b3",
            "01a0ec7b-0000-7000-8000-0000000000b4",
            "01a0ec7b-0000-7000-8000-0000000000b5",
            "01a0ec7b-0000-7000-8000-0000000000b6",
            "01a0ec7b-0000-7000-8000-0000000000b7",
        );
        let write = |id: &str, first: String| {
            std::fs::create_dir_all(sessions.join(id)).unwrap();
            std::fs::write(sessions.join(id).join(crate::log::EVENTS_FILE), format!("{first}\n{{\"later\": true}}\n")).unwrap();
        };
        let started = |id: &str, of: &str, cwd: &str, parent: Option<&str>| {
            let e = crate::protocol::LogEvent { id: id.into(), parent_id: None, session_id: of.into(), time_ms: 1, body: LogBody::SessionStarted { cwd: cwd.into(), krowk_version: "t".into(), protocol_version: 1, parent_session_id: parent.map(String::from), agent: None } };
            serde_json::to_string(&e).unwrap()
        };
        write(root, started(root, root, "/work/repo", None));
        write(kid, started(kid, kid, "/work/repo/sub", Some(root)));
        write(garbage, "not json".into());
        let ended = crate::protocol::LogEvent { id: other.into(), parent_id: None, session_id: other.into(), time_ms: 1, body: LogBody::TodosUpdated { turn_id: "t".into(), todos: Vec::new() } };
        write(other, serde_json::to_string(&ended).unwrap());
        write(foreign, started(foreign, root, "/elsewhere", None));
        write(relative, started(relative, relative, "repo", None));

        assert_eq!(session_dir(&sessions, root), Some(SessionDir { cwd: "/work/repo".into(), child: false }));
        assert_eq!(session_dir(&sessions, kid), Some(SessionDir { cwd: "/work/repo/sub".into(), child: true }));
        for (id, why) in [(absent, "no log"), (garbage, "a garbage first line"), (other, "another kind of event"), (foreign, "another session's start"), (relative, "a relative cwd"), ("../x", "an invalid id"), ("", "an empty id")] {
            assert_eq!(session_dir(&sessions, id), None, "{why}");
        }
        let _ = std::fs::remove_dir_all(&sessions);
    }

    /// R-SYNC-3, failing closed: with the switch off, a policy that cannot
    /// be read, or sync not set up here, a session's turn builds no bridge.
    /// With all three yes it does; a build that fails backs that session
    /// off, so a second turn meanwhile builds nothing more. R-PERF-2: with
    /// nothing bridged the supervisor only waits, and `stop` is prompt.
    #[test]
    fn r_sync_3_off_unreadable_or_not_set_up_builds_nothing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let sessions = std::env::temp_dir().join(format!("krowk-sync3-sup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sessions);
        let id = "01a0ec7b-0000-7000-8000-0000000000c1";
        std::fs::create_dir_all(sessions.join(id)).unwrap();
        let e = crate::protocol::LogEvent { id: id.into(), parent_id: None, session_id: id.into(), time_ms: 1, body: LogBody::SessionStarted { cwd: sessions.display().to_string(), krowk_version: "t".into(), protocol_version: 1, parent_session_id: None, agent: None } };
        std::fs::write(sessions.join(id).join(crate::log::EVENTS_FILE), format!("{}\n", serde_json::to_string(&e).unwrap())).unwrap();
        let on = Policy { on: true, off_by: None, exclude: Vec::new(), idle: None };
        let off = Policy { on: false, off_by: Some("KROWK_SYNC_AUTO"), ..on.clone() };
        type Case = (Result<Policy, String>, Result<(), String>, usize);
        let cases: [Case; 4] = [
            (Ok(off), Ok(()), 0),
            (Err("sync.auto in config.json is \"no\"".into()), Ok(()), 0),
            (Ok(on.clone()), Err("no device key".into()), 0),
            (Ok(on), Ok(()), 1),
        ];
        for (policy, ready, want) in cases {
            let built = Arc::new(AtomicUsize::new(0));
            let n = built.clone();
            let c = Config {
                socket: sessions.join("nothing.sock"),
                sessions: sessions.clone(),
                version: "t".into(),
                krowk_home: None,
                build: Box::new(move |_, _| {
                    n.fetch_add(1, Ordering::SeqCst);
                    Err(("unbuilt".into(), "the test's build fails".into()))
                }),
                policy: Box::new(move || policy.clone()),
                ready: Box::new(move || ready.clone()),
                bridged: Arc::default(),
                paid_pause: PAID_PAUSE,
            };
            let (turns, rx) = mpsc::unbounded_channel();
            let h = Supervisor::spawn(c, rx);
            turns.send(TurnEvent { session: id.into(), running: true }).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while want > 0 && built.load(Ordering::SeqCst) < want {
                assert!(std::time::Instant::now() < deadline, "never built");
                std::thread::sleep(Duration::from_millis(10));
            }
            // A second turn: the first is backing off, or still refused.
            turns.send(TurnEvent { session: id.into(), running: true }).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(built.load(Ordering::SeqCst), want);
            let t = std::time::Instant::now();
            h.stop(Duration::from_secs(5));
            assert!(t.elapsed() < Duration::from_secs(1), "nothing to wait for: {:?}", t.elapsed());
        }
        let _ = std::fs::remove_dir_all(&sessions);
    }

    #[test]
    fn a_wait_reads_as_a_person_says_it() {
        assert_eq!(human(BACKOFF_FIRST), "5 s");
        assert_eq!(human(Duration::from_secs(80)), "1 min 20 s");
        assert_eq!(human(BACKOFF_MOST), "5 min");
        assert_eq!(for_how_long(PAID_PAUSE), "an hour");
        assert_eq!(for_how_long(Duration::from_secs(3)), "3 s");
    }

    /// R-SYNC-3: an idle bridge is let go once the window has passed since
    /// its last turn ended — never while a turn runs, however long, never
    /// twice, and never with no window. The clock is an input.
    #[test]
    fn r_sync_3_an_idle_bridge_is_let_go_only_past_the_window_with_no_turn_running() {
        let quiet = Instant::now();
        let window = Some(Duration::from_secs(600));
        let at = idle_at(quiet, false, false, window).unwrap();
        assert_eq!(at, quiet + Duration::from_secs(600));
        let due = |now: Instant, running: bool, stopping: bool, idle: Option<Duration>| idle_at(quiet, running, stopping, idle).is_some_and(|at| at <= now);
        assert!(!due(quiet + Duration::from_secs(599), false, false, window), "not before the window");
        assert!(due(quiet + Duration::from_secs(600), false, false, window), "at it");
        assert!(due(quiet + Duration::from_secs(86_400), false, false, window), "and long after");
        assert!(!due(quiet + Duration::from_secs(86_400), true, false, window), "never while a turn runs, however long");
        assert!(!due(quiet + Duration::from_secs(86_400), false, true, window), "never twice");
        assert!(!due(quiet + Duration::from_secs(86_400), false, false, None), "never with no window");
    }

    /// R-SYNC-1: the paid-plan refusal is found in either shape a bridge
    /// ends with, and its fix is the one said.
    #[test]
    fn r_sync_1_a_paid_plan_refusal_names_its_fix() {
        let fix = "syncing sessions between devices needs a paid plan — upgrade this workspace to Pro at https://app.krowk.com";
        let taken = format!("{} (sync_requires_paid_plan): {fix}", host::NOT_GRANTED);
        let built = format!("{fix} (sync_requires_paid_plan)");
        let shown = "the registry did not say whether it holds session x (sync_requires_paid_plan) — nothing was published; try again";
        let renewed = format!("the registry refused to renew the lease (sync_requires_paid_plan): {fix}");
        for e in [&taken, &built, &renewed] {
            assert!(unpaid(e), "{e}");
            assert_eq!(paid_fix(e), fix, "{e}");
        }
        assert!(unpaid(shown));
        assert!(paid_fix(shown).contains("upgrade this workspace to Pro"), "{}", paid_fix(shown));
        assert!(!unpaid(&format!("{} (lease_held): device x holds it", host::NOT_GRANTED)));
        assert!(!unpaid(&format!("{} session x to device (sync_requires_paid_plan)", host::HANDED_OVER)), "a device's name says nothing");
    }

    /// R-OFF-2 and R-SYNC-2: a session whose wait ran out is started again
    /// while it is in use — a turn running or started meanwhile, or its last
    /// within the idle window — and one waiting on another's lease only for
    /// a turn; one nobody uses waits for its next turn.
    #[test]
    fn r_off_2_a_session_backing_off_is_started_again_only_while_in_use() {
        let quiet = Instant::now();
        let idle = Some(Duration::from_secs(600));
        let w = |lazy: bool, wanted: bool| Wait { at: quiet, lazy, quiet, wanted };
        let soon = quiet + Duration::from_secs(60);
        let late = quiet + Duration::from_secs(601);
        assert!(wanted_again(&w(false, false), false, idle, soon), "used a minute ago: sync resumes by itself");
        assert!(!wanted_again(&w(false, false), false, idle, late), "nobody used it for the window: its next turn");
        assert!(wanted_again(&w(false, false), false, None, late), "no window: as before, always");
        assert!(wanted_again(&w(false, false), true, idle, late), "a turn runs");
        assert!(wanted_again(&w(false, true), false, idle, late), "a turn started meanwhile");
        assert!(!wanted_again(&w(true, false), false, idle, soon), "another's lease: only for a turn");
        assert!(!wanted_again(&w(true, false), false, None, soon));
        assert!(wanted_again(&w(true, true), false, idle, late));
        assert!(wanted_again(&w(true, false), true, idle, late));
    }
}
