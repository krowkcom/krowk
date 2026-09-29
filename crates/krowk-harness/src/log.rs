//! The session log: the source of truth for a native session (R-LOG-1).
//!
//! Each session is a directory in krowk's home, beside krowk.db:
//!
//! ```text
//! ~/.krowk/sessions/<session-id>/events.jsonl   the log
//! ~/.krowk/sessions/<session-id>/context.jsonl  each turn's system prompt and tools
//! ```
//!
//! `events.jsonl` is append-only: one `LogEvent` per line, never rewritten.
//! Each event's `parentId` is the event appended before it on its branch —
//! the head — so the file is a tree written in time order, and the branch a
//! session continues from is found by walking parents back from its head.
//! The head is the last line until forks exist to name another.
//!
//! One writer per session: the log is locked while a turn appends to it, so
//! a second `krowk -p --resume` of the same session is refused rather than
//! interleaved. Directories are 0700 and files 0600 — a log holds what an
//! agent was told and read, secrets included.

use crate::protocol::{ContextRecord, Item, LogBody, LogEvent, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const EVENTS_FILE: &str = "events.jsonl";
pub const CONTEXT_FILE: &str = "context.jsonl";

/// `sessions/` in krowk's home, where krowk.db is too.
pub fn sessions_dir(env: &dyn Fn(&str) -> String) -> Result<PathBuf, krowk_api::Error> {
    Ok(krowk_api::home::dir(env)?.join(krowk_api::home::SESSIONS))
}

/// An open session log, holding its lock.
pub struct SessionLog {
    pub session_id: String,
    pub dir: PathBuf,
    /// Shared with the blocking pool, where the async appends write (see
    /// `off`).
    events: Arc<File>,
    context: Arc<File>,
    /// The event the next append hangs from.
    head: Option<String>,
}

#[derive(Debug)]
pub enum LogError {
    /// No session by that id.
    NotFound(String),
    /// Another process is appending to it.
    Busy(String),
    Io(String),
}

impl LogError {
    pub fn message(&self) -> &str {
        match self {
            LogError::NotFound(m) | LogError::Busy(m) | LogError::Io(m) => m,
        }
    }
}

fn io(what: impl std::fmt::Display, e: std::io::Error) -> LogError {
    LogError::Io(format!("{what}: {e}"))
}

impl SessionLog {
    /// A new session: its directory, and the root event, whose id is the
    /// session's id.
    pub fn create(sessions: &Path, cwd: &Path, krowk_version: &str) -> Result<(SessionLog, LogEvent), LogError> {
        SessionLog::create_child(sessions, cwd, krowk_version, None, None)
    }

    /// A new session spawned by `parent` — a subagent — or a top-level one
    /// when there is none. The root names the parent, which is how a budget
    /// finds what a session's subagents spent. `agent` is the definition the
    /// subagent runs, when it runs one.
    pub fn create_child(sessions: &Path, cwd: &Path, krowk_version: &str, parent: Option<&str>, agent: Option<&str>) -> Result<(SessionLog, LogEvent), LogError> {
        let session_id = krowk_store::new_id();
        let dir = sessions.join(&session_id);
        private_dir(&dir).map_err(|e| io(format_args!("create {}", dir.display()), e))?;
        let mut log = SessionLog::open_files(session_id.clone(), dir)?;
        let root = LogEvent {
            id: session_id.clone(),
            parent_id: None,
            session_id,
            time_ms: krowk_store::now_ms(),
            body: LogBody::SessionStarted {
                cwd: cwd.display().to_string(),
                krowk_version: krowk_version.into(),
                protocol_version: PROTOCOL_VERSION,
                parent_session_id: parent.map(String::from),
                agent: agent.map(String::from),
            },
        };
        log.write(&root)?;
        log.head = Some(root.id.clone());
        Ok((log, root))
    }

    /// An existing session, and every event in its log, in file order.
    pub fn open(sessions: &Path, session_id: &str) -> Result<(SessionLog, Vec<LogEvent>), LogError> {
        let dir = sessions.join(session_id);
        if !valid_id(session_id) || !dir.join(EVENTS_FILE).is_file() {
            return Err(LogError::NotFound(format!("no krowk session {session_id:?} in {}", sessions.display())));
        }
        let mut log = SessionLog::open_files(session_id.to_string(), dir)?;
        let events = read_events(&log.dir.join(EVENTS_FILE))?;
        log.head = events.last().map(|e| e.id.clone());
        Ok((log, events))
    }

    fn open_files(session_id: String, dir: PathBuf) -> Result<SessionLog, LogError> {
        let events = append_private(&dir.join(EVENTS_FILE)).map_err(|e| io(format_args!("open {}", dir.join(EVENTS_FILE).display()), e))?;
        match events.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(LogError::Busy(format!("session {session_id} is running in another krowk — wait for its turn to finish")));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(io(format_args!("lock {}", dir.join(EVENTS_FILE).display()), e)),
        }
        let context = append_private(&dir.join(CONTEXT_FILE)).map_err(|e| io(format_args!("open {}", dir.join(CONTEXT_FILE).display()), e))?;
        Ok(SessionLog { session_id, dir, events: Arc::new(events), context: Arc::new(context), head: None })
    }

    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// Appends one event under the head, and makes it the head.
    pub fn append(&mut self, body: LogBody) -> Result<LogEvent, LogError> {
        let ev = LogEvent { id: krowk_store::new_id(), parent_id: self.head.clone(), session_id: self.session_id.clone(), time_ms: krowk_store::now_ms(), body };
        self.write(&ev)?;
        self.head = Some(ev.id.clone());
        Ok(ev)
    }

    /// One line, written whole: an append of a single buffer to a file
    /// opened O_APPEND lands in one piece.
    fn write(&mut self, ev: &LogEvent) -> Result<(), LogError> {
        let mut line = serde_json::to_string(ev).expect("an event serializes");
        line.push('\n');
        put(&self.events, &line, "append to the session log")
    }

    pub fn record_context(&mut self, rec: &ContextRecord) -> Result<(), LogError> {
        let mut line = serde_json::to_string(rec).expect("a context record serializes");
        line.push('\n');
        put(&self.context, &line, "append to the session's context record")
    }

    /// Flushed to the disk at the end of a turn, not per event: an event is
    /// a write, a turn is a sync.
    pub fn sync(&self) -> Result<(), LogError> {
        self.events.sync_data().map_err(|e| io("sync the session log", e))?;
        self.context.sync_data().map_err(|e| io("sync the session's context record", e))
    }

    // The same, off the async runtime's thread in the host daemon
    // (`off_thread`): it runs every session on one thread, and an fsync on a
    // busy disk, or a directory made, would stall every other session's
    // stream and heartbeats with it (R-LAG-9). Each is awaited before its
    // event is sent anywhere, so no client ever sees an event the log does
    // not have. A process with one session and no heartbeats (`krowk -p`,
    // the TUI on its own host) does them where it is: a blocking pool's
    // thread there would outlive the work and wake the idle process.

    /// `create_child`, off the thread.
    pub async fn create_child_off(sessions: &Path, cwd: &Path, krowk_version: &str, parent: Option<&str>, agent: Option<&str>) -> Result<(SessionLog, LogEvent), LogError> {
        let (sessions, cwd, v, parent, agent) = (sessions.to_path_buf(), cwd.to_path_buf(), krowk_version.to_string(), parent.map(String::from), agent.map(String::from));
        off(move || SessionLog::create_child(&sessions, &cwd, &v, parent.as_deref(), agent.as_deref())).await
    }

    /// `open`, off the thread.
    pub async fn open_off(sessions: &Path, session_id: &str) -> Result<(SessionLog, Vec<LogEvent>), LogError> {
        let (sessions, id) = (sessions.to_path_buf(), session_id.to_string());
        off(move || SessionLog::open(&sessions, &id)).await
    }

    /// `append`, its write off the thread.
    pub async fn append_off(&mut self, body: LogBody) -> Result<LogEvent, LogError> {
        let ev = LogEvent { id: krowk_store::new_id(), parent_id: self.head.clone(), session_id: self.session_id.clone(), time_ms: krowk_store::now_ms(), body };
        let mut line = serde_json::to_string(&ev).expect("an event serializes");
        line.push('\n');
        let f = self.events.clone();
        off(move || put(&f, &line, "append to the session log")).await?;
        self.head = Some(ev.id.clone());
        Ok(ev)
    }

    /// `record_context`, its write off the thread.
    pub async fn record_context_off(&mut self, rec: &ContextRecord) -> Result<(), LogError> {
        let mut line = serde_json::to_string(rec).expect("a context record serializes");
        line.push('\n');
        let f = self.context.clone();
        off(move || put(&f, &line, "append to the session's context record")).await
    }

    /// `sync`, off the thread.
    pub async fn sync_off(&self) -> Result<(), LogError> {
        let (e, c) = (self.events.clone(), self.context.clone());
        off(move || {
            e.sync_data().map_err(|x| io("sync the session log", x))?;
            c.sync_data().map_err(|x| io("sync the session's context record", x))
        })
        .await
    }
}

impl SessionLog {
    /// `sync` at a turn's end, on the blocking pool, not waited for: the
    /// turn's result goes out as it did while the sync runs. It syncs the
    /// files through descriptors of its own, so the session's lock goes
    /// with this log as before and the next turn is never refused as busy
    /// while it runs; a sync that fails is said on stderr, since the turn it
    /// would have failed has ended.
    ///
    /// The daemon waits for it on its way out (`synced`), not the turn:
    /// tokio drops blocking tasks still queued when its runtime goes.
    pub fn sync_behind(&self) {
        self.sync_behind_on(tokio::runtime::Handle::try_current().ok().filter(|_| OFF_THREAD.load(std::sync::atomic::Ordering::Relaxed)));
    }

    fn sync_behind_on(&self, rt: Option<tokio::runtime::Handle>) {
        let (e, c, id) = (self.dir.join(EVENTS_FILE), self.dir.join(CONTEXT_FILE), self.session_id.clone());
        let Some(rt) = rt else {
            if let Err(err) = self.sync() {
                eprintln!("session {id}: {}", err.message());
            }
            return;
        };
        QUEUED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let handle = rt.spawn_blocking(move || {
            if let Err(err) = File::open(&e).and_then(|f| f.sync_data()).and_then(|()| File::open(&c)).and_then(|f| f.sync_data()) {
                eprintln!("session {id}: the log could not be synced: {err}");
            }
            DONE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let mut pending = SYNCS.lock().unwrap_or_else(|p| p.into_inner());
        pending.retain(|h| !h.is_finished());
        pending.push(handle);
    }
}

/// The turns' syncs still on the blocking pool: few at a time, each let go
/// once done.
static SYNCS: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> = std::sync::Mutex::new(Vec::new());

/// Syncs handed to the blocking pool, and those that have run: a sync the
/// runtime dropped unrun is the difference.
static QUEUED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DONE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Waits for every sync `sync_behind` has handed the blocking pool, those
/// queued while it waits included: the host daemon's last step before its
/// runtime goes.
pub async fn synced() {
    loop {
        let pending = std::mem::take(&mut *SYNCS.lock().unwrap_or_else(|p| p.into_inner()));
        if pending.is_empty() {
            return;
        }
        for h in pending {
            let _ = h.await;
        }
    }
}

/// How many syncs have been handed to the blocking pool.
pub fn queued_syncs() -> u64 {
    QUEUED.load(std::sync::atomic::Ordering::SeqCst)
}

/// How many syncs handed to the blocking pool have not run.
pub fn pending_syncs() -> u64 {
    QUEUED.load(std::sync::atomic::Ordering::SeqCst).saturating_sub(DONE.load(std::sync::atomic::Ordering::SeqCst))
}

static OFF_THREAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends the session log's blocking work to the blocking pool from here on:
/// the host daemon's choice, as it starts.
pub fn off_thread() {
    OFF_THREAD.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Runs `f` on the blocking pool in the host daemon, else where it is.
async fn off<T: Send + 'static>(f: impl FnOnce() -> Result<T, LogError> + Send + 'static) -> Result<T, LogError> {
    if !OFF_THREAD.load(std::sync::atomic::Ordering::Relaxed) {
        return f();
    }
    tokio::task::spawn_blocking(f).await.map_err(|e| LogError::Io(format!("the session log's writer failed: {e}")))?
}

/// `read_events`, off the thread in the host daemon.
pub async fn read_events_off(path: &Path) -> Result<Vec<LogEvent>, LogError> {
    let path = path.to_path_buf();
    off(move || read_events(&path)).await
}

/// One line appended whole: a single buffer written to a file opened
/// O_APPEND lands in one piece.
fn put(f: &File, line: &str, what: &str) -> Result<(), LogError> {
    slow_disk();
    let mut f = f;
    f.write_all(line.as_bytes()).map_err(|e| io(what, e))
}

/// For tests (`test-hooks`): how long, in µs, each append and each read of
/// a log takes on top of its own time — a disk that stalls. Whichever
/// thread does the write waits it out, so a write left on the host
/// daemon's thread shows on its lateness probe (R-LAG-9).
#[cfg(feature = "test-hooks")]
static SLOW_DISK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// For tests (`test-hooks`): makes every log append and read take `d`
/// longer until the guard goes.
#[cfg(feature = "test-hooks")]
#[must_use = "the disk is slow only while the guard lives"]
pub fn simulate_slow_disk(d: std::time::Duration) -> SlowDisk {
    SLOW_DISK.store(d.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    SlowDisk(())
}

/// The disk is slow while this lives (`simulate_slow_disk`).
#[cfg(feature = "test-hooks")]
pub struct SlowDisk(());

#[cfg(feature = "test-hooks")]
impl Drop for SlowDisk {
    fn drop(&mut self) {
        SLOW_DISK.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

fn slow_disk() {
    #[cfg(feature = "test-hooks")]
    {
        let us = SLOW_DISK.load(std::sync::atomic::Ordering::Relaxed);
        if us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(us));
        }
    }
}

/// Every event in a log file. A line that does not parse — the torn tail of
/// a crash, or an event type this build does not know — is an error, not a
/// skip: a log read with a hole in it would continue the wrong branch.
pub fn read_events(path: &Path) -> Result<Vec<LogEvent>, LogError> {
    slow_disk();
    let f = File::open(path).map_err(|e| io(format_args!("open {}", path.display()), e))?;
    let mut out = Vec::new();
    for (n, line) in BufReader::new(f).lines().enumerate() {
        let line = line.map_err(|e| io(format_args!("read {}", path.display()), e))?;
        if line.trim().is_empty() {
            continue;
        }
        let ev: LogEvent = serde_json::from_str(&line).map_err(|e| LogError::Io(format!("{} line {}: {e}", path.display(), n + 1)))?;
        out.push(ev);
    }
    Ok(out)
}

/// The branch ending at `head`, root first.
pub fn branch<'a>(events: &'a [LogEvent], head: &str) -> Vec<&'a LogEvent> {
    let by_id: HashMap<&str, &LogEvent> = events.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut out = Vec::new();
    let mut at = by_id.get(head).copied();
    while let Some(ev) = at {
        out.push(ev);
        // A cycle is impossible by construction; the bound makes it harmless.
        if out.len() > events.len() {
            break;
        }
        at = ev.parent_id.as_deref().and_then(|p| by_id.get(p).copied());
    }
    out.reverse();
    out
}

/// Session ids are UUIDv7s krowk minted: anything else never names a
/// directory, so no id can walk out of the sessions directory.
pub fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() && !b.is_ascii_uppercase() })
        && &id[14..15] == "7"
        && matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b')
}

/// Every session directory with a log, as (id, events path).
pub fn list(sessions: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(sessions) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().into_owned();
            let path = e.path().join(EVENTS_FILE);
            (valid_id(&id) && path.is_file()).then_some((id, path))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// A session as the TUI's `/sessions` lists it, read from the head of its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recent {
    pub id: String,
    /// The first thing the person asked in it.
    pub prompt: String,
    /// When its log was last written, in ms since the epoch.
    pub last_ms: i64,
}

/// How far into a log `recent` looks for its first prompt.
const HEAD_LINES: usize = 64;

/// The sessions started in `cwd` that can be continued, the most recently
/// written first, at most `most` of them. A subagent's belongs to its
/// parent, and one nobody prompted has nothing to continue: neither is
/// listed. Only each log's head is read, never the whole.
pub fn recent(sessions: &Path, cwd: &Path, most: usize) -> Vec<Recent> {
    let mut found: Vec<(i64, String, PathBuf)> = list(sessions)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, path)| {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            let ms = modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
            Some((ms, id, path))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    found.into_iter().filter_map(|(last_ms, id, path)| head_prompt(&path, cwd).map(|prompt| Recent { id, prompt, last_ms })).take(most).collect()
}

/// The first prompt of a top-level session started in `cwd`; none for any
/// other, or one with no prompt in its head.
fn head_prompt(path: &Path, cwd: &Path) -> Option<String> {
    let mut lines = BufReader::new(File::open(path).ok()?).lines().map_while(Result::ok).filter(|l| !l.trim().is_empty());
    let root: LogEvent = serde_json::from_str(&lines.next()?).ok()?;
    match root.body {
        LogBody::SessionStarted { cwd: started, parent_session_id: None, .. } if Path::new(&started) == cwd => {}
        _ => return None,
    }
    lines.take(HEAD_LINES).filter_map(|l| serde_json::from_str::<LogEvent>(&l).ok()).find_map(|ev| match ev.body {
        LogBody::ItemCompleted { item: Item::UserText { text }, .. } if !text.starts_with(crate::todo::REMINDER) => Some(text),
        _ => None,
    })
}

pub(crate) fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

fn append_private(path: &Path) -> std::io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Item, LogBody};

    /// A turn's sync handed to a blocking pool that is busy is still run
    /// before the daemon's runtime goes: `synced` waits for it, where a
    /// runtime dropped would have dropped it unrun — a `krowk host stop`
    /// right after a turn leaves the turn on the disk (R-LAG-9's syncs off
    /// the thread, made durable).
    #[test]
    fn r_lag_9_a_stop_right_after_a_turn_waits_for_its_sync() {
        let dir = std::env::temp_dir().join(format!("krowk-harness-log-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (log, _) = SessionLog::create(&dir, Path::new("/repo"), "dev").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().max_blocking_threads(1).build().unwrap();
        // The pool's one thread is busy, so the sync waits in its queue.
        let busy = rt.spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(150)));
        log.sync_behind_on(Some(rt.handle().clone()));
        let done = DONE.load(std::sync::atomic::Ordering::SeqCst);
        let started = std::time::Instant::now();
        rt.block_on(synced());
        assert!(started.elapsed() >= std::time::Duration::from_millis(100), "waited for the sync queued behind the busy pool");
        assert!(DONE.load(std::sync::atomic::Ordering::SeqCst) > done, "and the sync ran");
        drop((busy, rt));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_id_is_a_lowercase_uuidv7_with_the_rfc_variant() {
        assert!(valid_id("0199a3c4-5b6d-7e8f-9a0b-1c2d3e4f5a6b"));
        for bad in ["0199a3c4-5b6d-7e8f-0a0b-1c2d3e4f5a6b", "0199a3c4-5b6d-7e8f-fa0b-1c2d3e4f5a6b", "0199a3c4-5b6d-4e8f-9a0b-1c2d3e4f5a6b", "0199A3C4-5B6D-7E8F-9A0B-1C2D3E4F5A6B"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn r_log_1_events_carry_uuidv7_ids_and_a_parent_chain_from_the_root() {
        let dir = std::env::temp_dir().join(format!("krowk-harness-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut log, root) = SessionLog::create(&dir, Path::new("/repo"), "dev").unwrap();
        assert_eq!(root.id, root.session_id, "the root's id is the session's");
        assert!(root.parent_id.is_none());
        let a = log.append(LogBody::ItemCompleted { turn_id: "t".into(), item_id: "i".into(), item: Item::UserText { text: "hi".into() } }).unwrap();
        let b = log.append(LogBody::ItemCompleted { turn_id: "t".into(), item_id: "j".into(), item: Item::AssistantText { text: "yo".into() } }).unwrap();
        assert_eq!((a.parent_id.as_deref(), b.parent_id.as_deref()), (Some(root.id.as_str()), Some(a.id.as_str())));
        // One writer: a second open is refused while the first holds the lock.
        assert!(matches!(SessionLog::open(&dir, &root.id), Err(LogError::Busy(_))));
        drop(log);
        // A process forked by a neighbouring test holds a copy of the locked
        // descriptor until it execs, so the lock can outlive the drop by a
        // moment: the reopen retries rather than racing it.
        let reopen = || SessionLog::open(&dir, &root.id);
        let mut opened = reopen();
        for _ in 0..100 {
            if !matches!(opened, Err(LogError::Busy(_))) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            opened = reopen();
        }
        let (_log, events) = opened.unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| valid_id(&e.id)));
        let chain: Vec<&str> = branch(&events, &b.id).iter().map(|e| e.id.as_str()).collect();
        assert_eq!(chain, [root.id.as_str(), a.id.as_str(), b.id.as_str()]);
        assert_eq!(list(&dir).unwrap().len(), 1);
        assert!(matches!(SessionLog::open(&dir, "../../etc"), Err(LogError::NotFound(_))));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join(&root.id)), 0o700);
            assert_eq!(mode(&dir.join(&root.id).join(EVENTS_FILE)), 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_lists_this_directorys_prompted_sessions_newest_first() {
        let dir = std::env::temp_dir().join(format!("krowk-harness-recent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let said = |text: &str| LogBody::ItemCompleted { turn_id: "t".into(), item_id: "i".into(), item: Item::UserText { text: text.into() } };
        let (mut old, _) = SessionLog::create(&dir, Path::new("/repo"), "dev").unwrap();
        old.append(said(&format!("{} the todo list", crate::todo::REMINDER))).unwrap();
        old.append(said("fix the parser")).unwrap();
        let (mut new, _) = SessionLog::create(&dir, Path::new("/repo"), "dev").unwrap();
        new.append(said("add a flag")).unwrap();
        let (_unprompted, _) = SessionLog::create(&dir, Path::new("/repo"), "dev").unwrap();
        let (mut elsewhere, _) = SessionLog::create(&dir, Path::new("/other"), "dev").unwrap();
        elsewhere.append(said("not here")).unwrap();
        let (mut child, _) = SessionLog::create_child(&dir, Path::new("/repo"), "dev", Some(&new.session_id), None).unwrap();
        child.append(said("a subagent's task")).unwrap();
        // Written last, the older session is the most recent.
        let touched = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        File::options().append(true).open(old.dir.join(EVENTS_FILE)).unwrap().set_modified(touched).unwrap();
        let found = recent(&dir, Path::new("/repo"), 10);
        let listed: Vec<(&str, &str)> = found.iter().map(|r| (r.id.as_str(), r.prompt.as_str())).collect();
        assert_eq!(listed, [(old.session_id.as_str(), "fix the parser"), (new.session_id.as_str(), "add a flag")]);
        assert!(found[0].last_ms > found[1].last_ms);
        assert_eq!(recent(&dir, Path::new("/repo"), 1).len(), 1, "at most `most`");
        assert!(recent(&dir.join("none"), Path::new("/repo"), 10).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
