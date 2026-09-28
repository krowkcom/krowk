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

use crate::protocol::{ContextRecord, LogBody, LogEvent, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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
    events: File,
    context: File,
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
        Ok(SessionLog { session_id, dir, events, context, head: None })
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
        self.events.write_all(line.as_bytes()).map_err(|e| io("append to the session log", e))
    }

    pub fn record_context(&mut self, rec: &ContextRecord) -> Result<(), LogError> {
        let mut line = serde_json::to_string(rec).expect("a context record serializes");
        line.push('\n');
        self.context.write_all(line.as_bytes()).map_err(|e| io("append to the session's context record", e))
    }

    /// Flushed to the disk at the end of a turn, not per event: an event is
    /// a write, a turn is a sync.
    pub fn sync(&self) -> Result<(), LogError> {
        self.events.sync_data().map_err(|e| io("sync the session log", e))?;
        self.context.sync_data().map_err(|e| io("sync the session's context record", e))
    }
}

/// Every event in a log file. A line that does not parse — the torn tail of
/// a crash, or an event type this build does not know — is an error, not a
/// skip: a log read with a hole in it would continue the wrong branch.
pub fn read_events(path: &Path) -> Result<Vec<LogEvent>, LogError> {
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
}
