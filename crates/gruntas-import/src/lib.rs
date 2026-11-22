//! Reading agent transcripts on this machine into krowk.db's shape.
//!
//! Each harness is a `Source`: it discovers the transcripts it knows about
//! and reads one into a `Thread`, resuming from a cursor so a sync reads only
//! what moved. Every path is resolved inside the home directory before it is
//! opened — a transcript directory is caller-controlled, and a symlink in it
//! must not turn an import into a read of anything else on the disk.

pub mod claude;
pub mod cursor;
mod home;
mod jsonl;
pub mod ledger;
pub mod opencode;
mod part;
mod turn;

pub use home::*;
pub use jsonl::*;
pub use part::*;
pub use turn::*;

use gruntas_store::Thread;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The process environment, as the CLI reads it.
pub type Env<'a> = &'a dyn Fn(&str) -> String;

pub const PROVIDER_CLAUDE: &str = "claude";
pub const PROVIDER_CURSOR: &str = "cursor";
pub const PROVIDER_OPENCODE: &str = "opencode";
pub const PROVIDER_LEDGER: &str = "ledger";

/// The worktree a session that ran in `dir` belongs to: the nearest
/// ancestor holding a `.git`, else the directory itself with vcs `none` —
/// the rule the Claude importer applies, for sources that record a cwd.
pub fn worktree_for(dir: &str) -> gruntas_store::Worktree {
    let (path, vcs) = claude::worktree_of(dir);
    gruntas_store::Worktree { name: claude::base_name(&path), path, vcs: vcs.into() }
}

/// Every source, in the order `--from all` reads them: the transcripts,
/// then the provider ledgers they are reconciled against.
pub fn sources() -> Vec<Box<dyn Source>> {
    vec![Box::new(claude::Claude), Box::new(cursor::Cursor), Box::new(opencode::Opencode), Box::new(ledger::Ledger)]
}

/// One harness's transcripts.
pub trait Source {
    fn name(&self) -> &'static str;
    /// What there is to read, found without reading it.
    fn discover(&self, env: Env) -> Result<Vec<Ref>, ImportError>;
    /// One transcript, from `cursor` on (empty is the start). Returns the
    /// thread, the cursor to resume from next time, and what was seen.
    fn read(&self, env: Env, r: &Ref, cursor: &str) -> Result<(Thread, String, ReadResult), ImportError>;
    /// Whether `cursor` says nothing moved since it was taken, so a sync can
    /// leave the transcript unread.
    fn unchanged(&self, env: Env, r: &Ref, cursor: &str) -> bool;
    /// Which ref id each subagent ref belongs to, for sources that know it
    /// without reading transcripts; a caller refreshing one session finds
    /// its children through this.
    fn parents(&self, _env: Env, refs: &[Ref]) -> std::collections::HashMap<String, String> {
        let _ = refs;
        std::collections::HashMap::new()
    }
}

/// One transcript a source found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ref {
    pub provider: String,
    pub id: String,
    pub path: String,
}

impl Ref {
    /// The import_state key: provider and id, or the path when there is no id.
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, if self.id.is_empty() { &self.path } else { &self.id })
    }
}

/// Why a transcript could not be read. The home-sandbox kinds are named so
/// the CLI can tell a refusal from an I/O failure.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportError {
    NoHome(String),
    OutsideHome(String),
    EscapingSymlink(String),
    NotRegularFile(String),
    TooLarge(String),
    UnsupportedOs(String),
    Other(String),
}

impl ImportError {
    pub fn message(&self) -> &str {
        match self {
            ImportError::NoHome(m)
            | ImportError::OutsideHome(m)
            | ImportError::EscapingSymlink(m)
            | ImportError::NotRegularFile(m)
            | ImportError::TooLarge(m)
            | ImportError::UnsupportedOs(m)
            | ImportError::Other(m) => m,
        }
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ImportError {}

impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        ImportError::Other(e.to_string())
    }
}

impl From<gruntas_store::StoreError> for ImportError {
    fn from(e: gruntas_store::StoreError) -> Self {
        ImportError::Other(e.message().to_string())
    }
}

/// Importing runs on macOS and Linux; Windows builds keep `sessions` off.
pub fn check_os() -> Result<(), ImportError> {
    if cfg!(windows) {
        return Err(ImportError::UnsupportedOs("importer: importing is not supported on this operating system".into()));
    }
    Ok(())
}

const MAX_SKIPPED_RETAINED: usize = 100;
const MAX_SKIP_REASON_BYTES: usize = 256;

/// A line a read passed over, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedLine {
    pub line: usize,
    pub offset: u64,
    pub reason: String,
}

/// What one read saw: lines, the kinds it recognised, what it did not, and
/// what it skipped — the first hundred of them kept, all of them counted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadResult {
    pub lines: usize,
    pub unknown: usize,
    pub unknown_types: BTreeMap<String, usize>,
    pub classified: BTreeMap<String, usize>,
    pub skipped_count: usize,
    pub skipped: Vec<SkippedLine>,
}

impl ReadResult {
    pub fn skip(&mut self, line: usize, offset: u64, reason: &str) {
        self.skipped_count += 1;
        if self.skipped.len() >= MAX_SKIPPED_RETAINED {
            return;
        }
        let reason = if reason.len() <= MAX_SKIP_REASON_BYTES {
            reason.to_string()
        } else {
            let mut cut = MAX_SKIP_REASON_BYTES - '…'.len_utf8();
            while !reason.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…", &reason[..cut])
        };
        self.skipped.push(SkippedLine { line, offset, reason });
    }

    pub fn classify(&mut self, kind: &str) {
        *self.classified.entry(kind.to_string()).or_default() += 1;
    }

    pub fn merge(&mut self, other: &ReadResult) {
        self.lines += other.lines;
        self.unknown += other.unknown;
        for (k, v) in &other.unknown_types {
            *self.unknown_types.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &other.classified {
            *self.classified.entry(k.clone()).or_default() += v;
        }
        self.skipped_count += other.skipped_count;
        for s in &other.skipped {
            if self.skipped.len() >= MAX_SKIPPED_RETAINED {
                break;
            }
            self.skipped.push(s.clone());
        }
    }

    /// A part as its source typed it, counted as unknown when its type is
    /// not one krowk knows.
    pub fn normalize_part(&mut self, raw_type: &str, raw: Option<&serde_json::Value>) -> gruntas_store::Part {
        let (part, known) = normalize_part(raw_type, raw);
        if !known {
            self.unknown += 1;
            *self.unknown_types.entry(raw_type.to_string()).or_default() += 1;
        }
        part
    }
}

/// Where a JSONL read stopped: the byte offset, and the file's size then, so
/// a file that shrank is re-read from the start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonlCursor {
    pub offset: u64,
    pub size: u64,
}

/// Where a SQLite read stopped: the newest time_updated imported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteCursor {
    pub time_updated: i64,
}

pub fn encode_cursor(c: &impl Serialize) -> String {
    serde_json::to_string(c).expect("a cursor serializes")
}

/// An empty or unreadable cursor is the start.
pub fn decode_jsonl_cursor(s: &str) -> Result<JsonlCursor, ImportError> {
    if s.is_empty() {
        return Ok(JsonlCursor::default());
    }
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        offset: i64,
        #[serde(default)]
        size: i64,
    }
    let raw: Raw = serde_json::from_str(s).map_err(|e| ImportError::Other(format!("decode jsonl cursor: {e}")))?;
    Ok(JsonlCursor { offset: raw.offset.max(0) as u64, size: raw.size.max(0) as u64 })
}

pub fn decode_sqlite_cursor(s: &str) -> Result<SqliteCursor, ImportError> {
    if s.is_empty() {
        return Ok(SqliteCursor::default());
    }
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        time_updated: i64,
    }
    let raw: Raw = serde_json::from_str(s).map_err(|e| ImportError::Other(format!("decode sqlite cursor: {e}")))?;
    Ok(SqliteCursor { time_updated: raw.time_updated.max(0) })
}
