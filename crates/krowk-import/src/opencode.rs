//! opencode's transcripts, read out of its one SQLite database.
//!
//! One ref per session row, so each session carries its own watermark and
//! its key stays `opencode:<session_id>`. The database is opened read-only
//! and nothing else: opencode holds it open in WAL mode while it runs, and a
//! reader that wrote — a sidecar, a lock, a PRAGMA — would contend with the
//! live agent. No PRAGMA is ever issued.
//!
//! Ids are opencode's (ses_/msg_/prt_), never derived; transcript order is
//! time_created with rowid breaking ties. A read is always the whole session:
//! turns are cumulative spans whose costs are summed over the span, so a read
//! resumed from the middle could neither number nor cost them, and the
//! store's foreign_id dedup makes the re-read free. The cursor is the largest
//! message or part time_updated successfully imported.
//!
//! A tool part carries its call and its result in one row, so a finished
//! (completed or error) tool becomes a tool_call and a tool_result twin
//! sharing the call id; a tool still running is the call alone. Every other
//! part keeps its raw payload through `normalize_part`.
//!
//! Huge rows are never selected whole: lengths first, then the full blob for
//! a row under the cap or a short `substr` prefix for one over it, whose
//! identity fields are string-scanned at the top level of the JSON.

use crate::{
    check_os, decode_sqlite_cursor, encode_cursor, home_dir, home_path, known_part_type, new_tool_call_part,
    new_tool_result_part, split_turns, Env, ImportError, ReadResult, Ref, Source, SqliteCursor, TurnCandidate,
    PART_FILE, PART_PATCH, PART_STEP, PART_TEXT, PART_THINKING,
};
use krowk_store::{Binding, Message, Part, Role, Session, Thread, Turn, Worktree};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

/// The harness, and the model vendor for a message naming no providerID.
pub const HARNESS: &str = "opencode";

/// opencode's database, relative to home; resolved through `home_path` so a
/// symlink out of home is refused rather than followed.
const DB_REL: &str = ".local/share/opencode/opencode.db";

/// A message blob over this is read as a prefix only: one live row is 239MB
/// of summary.diffs, and even json_extract parses the whole blob.
const MESSAGE_RAW_LIMIT: i64 = 200_000;
/// Role, model, provider, tokens and cost ride at the top of the blob.
const MESSAGE_PREFIX_LEN: i64 = 2000;
/// Live parts peak at 2.6MB; this is for the pathological one.
const PART_RAW_LIMIT: i64 = 5_000_000;
/// Covers a part's discriminator, tool twin ids and state status.
const PART_PREFIX_LEN: i64 = 4000;

const VCS_GIT: &str = "git";
const VCS_NONE: &str = "none";

pub struct Opencode;

impl Source for Opencode {
    fn name(&self) -> &'static str {
        crate::PROVIDER_OPENCODE
    }

    /// Every session row, sorted by id. A machine with no database — or one
    /// that cannot be statted, or holds something that is not this database —
    /// has no transcripts, which is an answer and not an error.
    fn discover(&self, env: Env) -> Result<Vec<Ref>, ImportError> {
        check_os().map_err(|e| wrap("opencode", e))?;
        let db = match home_path(env, DB_REL) {
            Ok(p) => p,
            // home_path does not say NotFound by kind; a home that does not
            // exist is the one way it fails for a missing file.
            Err(ImportError::Other(_)) if !home_dir(env).is_empty() && !Path::new(&home_dir(env)).exists() => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(wrap("opencode: resolve database", e)),
        };
        match std::fs::metadata(&db) {
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied) => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(ImportError::Other(format!("opencode: stat database: {e}"))),
        }
        let ids = match list_sessions(&db) {
            Ok(ids) => ids,
            // Gone since the stat, or replaced by something that is not this
            // database: the empty-machine answer.
            Err(e) if is_open_missing(&e.to_string()) => return Ok(Vec::new()),
            Err(e) => return Err(ImportError::Other(format!("opencode: list sessions: {e}"))),
        };
        Ok(ids.into_iter().map(|id| Ref { provider: self.name().into(), id, path: DB_REL.into() }).collect())
    }

    /// The whole session, whatever the cursor says; the cursor is only
    /// checked. On an error the caller keeps the cursor it had, which the
    /// session has not invalidated.
    fn read(&self, env: Env, r: &Ref, cursor: &str) -> Result<(Thread, String, ReadResult), ImportError> {
        decode_sqlite_cursor(cursor).map_err(|e| wrap("opencode", e))?;
        let rel = if r.path.is_empty() { DB_REL } else { r.path.as_str() };
        // The path is a hint naming the one database this source reads,
        // never an arbitrary file to open.
        if rel != DB_REL {
            return Err(ImportError::Other(format!("opencode: unexpected ref path {rel:?}")));
        }
        let db_path = home_path(env, rel).map_err(|e| wrap("opencode: resolve database", e))?;
        let db = open_read_only(&db_path).map_err(|e| ImportError::Other(format!("opencode: open {rel}: {e}")))?;
        let mut b = Builder::new(r);
        b.load(&db).map_err(|e| ImportError::Other(format!("opencode: read {}: {e}", r.id)))?;
        let cursor = encode_cursor(&SqliteCursor { time_updated: b.max_updated });
        let (th, acc) = b.finish();
        Ok((th, cursor, acc))
    }

    /// session.parent_id, in one read-only query.
    fn parents(&self, env: Env, _refs: &[Ref]) -> std::collections::HashMap<String, String> {
        let Ok(path) = home_path(env, DB_REL) else { return Default::default() };
        let Ok(db) = open_read_only(&path) else { return Default::default() };
        db.prepare("SELECT id, parent_id FROM session WHERE parent_id IS NOT NULL AND parent_id != ''")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
            .unwrap_or_default()
    }

    fn unchanged(&self, env: Env, r: &Ref, cursor: &str) -> bool {
        let Ok(c) = decode_sqlite_cursor(cursor) else { return false };
        c.time_updated > 0 && matches!(changed_since(env, r, c.time_updated), Ok(false))
    }
}

/// Whether any message or part row of the session was updated after `since`,
/// a watermark a previous read handed back: two EXISTS probes instead of the
/// whole-session read. The session row is not consulted — its time_updated
/// moves with things the watermark does not cover.
pub fn changed_since(env: Env, r: &Ref, since: i64) -> Result<bool, ImportError> {
    let path = home_path(env, DB_REL)?;
    let db = open_read_only(&path).map_err(sql_err)?;
    db.query_row(
        "SELECT EXISTS (SELECT 1 FROM message WHERE session_id = ?1 AND time_updated > ?2)
           OR EXISTS (SELECT 1 FROM part WHERE session_id = ?1 AND time_updated > ?2)",
        rusqlite::params![r.id, since],
        |row| row.get(0),
    )
    .map_err(sql_err)
}

/// One read-only handle. The path is handed to SQLite as a plain filename,
/// not a URI, so `?#&` in a directory cannot become URI parameters; it is
/// always absolute (home_path), so the bundled build's SQLITE_USE_URI never
/// sees a `file:` prefix either.
fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
}

fn list_sessions(path: &Path) -> rusqlite::Result<Vec<String>> {
    let db = open_read_only(path)?;
    let mut stmt = db.prepare("SELECT id FROM session ORDER BY id ASC")?;
    stmt.query_map([], |row| row.get(0))?.collect()
}

/// The errors of a database that vanished after the stat or is not this one.
fn is_open_missing(s: &str) -> bool {
    s.contains("unable to open") || s.contains("no such table") || s.contains("not a database")
}

fn sql_err(e: rusqlite::Error) -> ImportError {
    ImportError::Other(e.to_string())
}

/// `prefix: message`, keeping the kind so a refusal stays a refusal.
fn wrap(prefix: &str, e: ImportError) -> ImportError {
    let m = format!("{prefix}: {}", e.message());
    match e {
        ImportError::NoHome(_) => ImportError::NoHome(m),
        ImportError::OutsideHome(_) => ImportError::OutsideHome(m),
        ImportError::EscapingSymlink(_) => ImportError::EscapingSymlink(m),
        ImportError::NotRegularFile(_) => ImportError::NotRegularFile(m),
        ImportError::TooLarge(_) => ImportError::TooLarge(m),
        ImportError::UnsupportedOs(_) => ImportError::UnsupportedOs(m),
        ImportError::Other(_) => ImportError::Other(m),
    }
}

/// A TEXT or BLOB column as bytes; NULL is an error, as Go's string scan.
fn bytes(row: &rusqlite::Row, i: usize) -> rusqlite::Result<Vec<u8>> {
    let v = row.get_ref(i)?;
    v.as_bytes()
        .map(<[u8]>::to_vec)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, v.data_type(), Box::new(e)))
}

/// A nullable TEXT or BLOB column as text, NULL as empty.
fn text_or_empty(row: &rusqlite::Row, i: usize) -> rusqlite::Result<String> {
    let v = row.get_ref(i)?;
    Ok(v.as_bytes_or_null().ok().flatten().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default())
}

/// The token classes a turn is costed from, as opencode names them.
#[derive(Debug, Clone, Copy, Default)]
struct Tokens {
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_write: i64,
}

impl Tokens {
    /// Leniently, field by field: a class that is not an integer stays zero,
    /// a visibly missing number rather than a failed message.
    fn from_value(v: &Value) -> Tokens {
        let int = |v: Option<&Value>| v.and_then(Value::as_i64).unwrap_or(0);
        let cache = v.get("cache");
        Tokens {
            input: int(v.get("input")),
            output: int(v.get("output")),
            reasoning: int(v.get("reasoning")),
            cache_read: int(cache.and_then(|c| c.get("read"))),
            cache_write: int(cache.and_then(|c| c.get("write"))),
        }
    }
}

/// One message row decoded down to what the builder acts on.
struct MessageFields {
    role: String,
    model_id: String,
    provider_id: String,
    usage: String,
    tokens: Tokens,
    cost: Option<f64>,
    raw_json: Option<String>,
}

/// A tool part's execution state; input and output stay JSON, since a tool
/// that returned structure must not be flattened into text.
#[derive(Debug, Clone, Default)]
struct ToolState {
    status: String,
    input: Option<Value>,
    output: Option<Value>,
}

/// part.data down to the discriminator and what the tool twin needs.
struct PartData {
    kind: String,
    tool: String,
    call_id: String,
    state: Option<ToolState>,
}

/// A JSON object (or null) as Go's struct decode sees it: anything else, or
/// a named field of the wrong type, fails the whole decode.
fn object(v: &Value) -> Result<(), String> {
    if v.is_object() || v.is_null() { Ok(()) } else { Err(format!("cannot decode {} into an object", kind_of(v))) }
}

fn str_field(v: &Value, k: &str) -> Result<String, String> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(o) => Err(format!("field {k}: cannot decode {} into a string", kind_of(o))),
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl PartData {
    fn from_value(v: &Value) -> Result<PartData, String> {
        object(v)?;
        // Decoded only to fail as Go's decode does on a mistyped field.
        str_field(v, "text")?;
        str_field(v, "snapshot")?;
        let state = match v.get("state") {
            None | Some(Value::Null) => None,
            Some(s) => {
                object(s)?;
                Some(ToolState {
                    status: str_field(s, "status")?,
                    input: s.get("input").cloned(),
                    output: s.get("output").cloned(),
                })
            }
        };
        Ok(PartData { kind: str_field(v, "type")?, tool: str_field(v, "tool")?, call_id: str_field(v, "callID")?, state })
    }
}

struct MessageMeta {
    id: String,
    updated: i64,
    size: i64,
}

struct PartMeta {
    id: String,
    size: i64,
    updated: i64,
}

struct Builder<'a> {
    r: &'a Ref,
    acc: ReadResult,
    worktree_path: String,
    worktree_vcs: String,
    directory: String,
    title: String,
    model: String,
    provider: String,
    parent_id: String,
    messages: Vec<Message>,
    tokens: Vec<Tokens>,
    costs: Vec<Option<f64>>,
    candidates: Vec<TurnCandidate>,
    /// The watermark: the largest time_updated successfully imported.
    max_updated: i64,
}

impl<'a> Builder<'a> {
    fn new(r: &'a Ref) -> Builder<'a> {
        Builder {
            r,
            acc: ReadResult::default(),
            worktree_path: String::new(),
            worktree_vcs: String::new(),
            directory: String::new(),
            title: String::new(),
            model: String::new(),
            provider: String::new(),
            parent_id: String::new(),
            messages: Vec::new(),
            tokens: Vec::new(),
            costs: Vec::new(),
            candidates: Vec::new(),
            max_updated: 0,
        }
    }

    /// The session row, its project row, and every message with its parts,
    /// in transcript order.
    fn load(&mut self, db: &Connection) -> Result<(), String> {
        let id = self.r.id.as_str();
        if id.is_empty() {
            return Err("opencode: ref has no session id".into());
        }
        let row = db
            .query_row("SELECT project_id, parent_id, directory, title, model FROM session WHERE id = ?1", [id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((project_id, parent_id, directory, title, model)) = row else {
            return Err(format!("session {id} not found"));
        };
        self.directory = directory.unwrap_or_default();
        self.title = title.unwrap_or_default();
        self.parent_id = parent_id.unwrap_or_default();
        (self.model, self.provider) = session_model_of(&model.unwrap_or_default());

        // A project row deleted out from under its session falls back to the
        // session's own directory.
        let (worktree, vcs) = db
            .query_row("SELECT worktree, vcs FROM project WHERE id = ?1", [&project_id], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        (self.worktree_path, self.worktree_vcs) =
            resolve_worktree(&worktree.unwrap_or_default(), &vcs.unwrap_or_default(), &self.directory);

        // No data column here, so a 239MB row is one integer until its turn.
        let metas: Vec<MessageMeta> = db
            .prepare(
                "SELECT id, time_created, time_updated, length(CAST(data AS BLOB))
                   FROM message WHERE session_id = ?1 ORDER BY time_created ASC, rowid ASC",
            )
            .and_then(|mut stmt| {
                stmt.query_map([id], |row| Ok(MessageMeta { id: row.get(0)?, updated: row.get(2)?, size: row.get(3)? }))?
                    .collect()
            })
            .map_err(|e| e.to_string())?;
        for (i, m) in metas.iter().enumerate() {
            let n = i + 1;
            match self.message(db, n, m) {
                // A message this build cannot use is a skip, not a failed
                // read, and does not move the watermark, so the next read
                // retries it. opencode has no byte offsets: offset 0.
                Err(e) => self.acc.skip(n, 0, &e),
                // A part edited after its message still moves it.
                Ok(part_max) => self.max_updated = self.max_updated.max(m.updated).max(part_max),
            }
        }
        Ok(())
    }

    /// One message row: the whole blob under the cap, else a prefix whose
    /// top-level fields are scanned. A prefix naming no role is an error.
    /// Returns the largest part time_updated under it.
    fn message(&mut self, db: &Connection, n: usize, m: &MessageMeta) -> Result<i64, String> {
        if m.size <= MESSAGE_RAW_LIMIT {
            let data =
                db.query_row("SELECT data FROM message WHERE id = ?1", [&m.id], |row| bytes(row, 0)).map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&data);
            let bad = |e: String| format!("opencode: message {}: {e}", m.id);
            let v: Value = crate::decode_line(text.as_bytes()).map_err(|e| bad(e.to_string()))?;
            object(&v).map_err(bad)?;
            let tokens = v.get("tokens");
            let cost = match v.get("cost") {
                None | Some(Value::Null) => None,
                Some(c) => Some(c.as_f64().ok_or_else(|| bad(format!("field cost: cannot decode {} into a number", kind_of(c))))?),
            };
            let d = MessageFields {
                role: str_field(&v, "role").map_err(bad)?,
                model_id: str_field(&v, "modelID").map_err(bad)?,
                provider_id: str_field(&v, "providerID").map_err(bad)?,
                usage: tokens.map(Value::to_string).unwrap_or_default(),
                tokens: tokens.map(Tokens::from_value).unwrap_or_default(),
                cost,
                raw_json: std::str::from_utf8(&data).ok().map(str::to_string),
            };
            return self.add_message(db, n, &m.id, d);
        }
        let head = db
            .query_row("SELECT substr(data,1,?1) FROM message WHERE id = ?2", rusqlite::params![MESSAGE_PREFIX_LEN, m.id], |row| {
                text_or_empty(row, 0)
            })
            .map_err(|e| e.to_string())?;
        let role = extract_top_json_string(&head, "role").unwrap_or_default();
        if role.is_empty() {
            return Err(format!("opencode: message {} has no role in its prefix", m.id));
        }
        // Scoped to the tokens object, so a nested "input" in the tail cannot
        // inflate a turn; usage is the verbatim object, never a remarshal
        // that would fabricate zeros for classes the prefix never named.
        let mut tk = Tokens::default();
        let mut usage = String::new();
        if let Some(obj) = extract_top_object(&head, "tokens") {
            let mut found = false;
            let mut set = |slot: &mut i64, hay: &str, key: &str| {
                if let Some(v) = extract_json_int(hay, key) {
                    *slot = v;
                    found = true;
                }
            };
            set(&mut tk.input, obj, "input");
            set(&mut tk.output, obj, "output");
            set(&mut tk.reasoning, obj, "reasoning");
            if let Some(cache) = extract_object_field(obj, "cache") {
                set(&mut tk.cache_read, cache, "read");
                set(&mut tk.cache_write, cache, "write");
            }
            if found && serde_json::from_str::<serde::de::IgnoredAny>(obj).is_ok() {
                usage = obj.to_string();
            }
        }
        let d = MessageFields {
            role,
            model_id: extract_top_json_string(&head, "modelID").unwrap_or_default(),
            provider_id: extract_top_json_string(&head, "providerID").unwrap_or_default(),
            usage,
            tokens: tk,
            cost: extract_top_json_float(&head, "cost"),
            raw_json: None,
        };
        self.add_message(db, n, &m.id, d)
    }

    /// The assembly both row paths share.
    fn add_message(&mut self, db: &Connection, n: usize, id: &str, d: MessageFields) -> Result<i64, String> {
        let role = match d.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            other => {
                // Counted, so a new role shows up instead of vanishing.
                self.acc.classify(&format!("role:{other}"));
                return Err(format!("opencode: message {id} has role {other:?}"));
            }
        };
        if !d.provider_id.is_empty() {
            self.provider = d.provider_id.clone();
        }
        if role == Role::Assistant && !d.model_id.is_empty() {
            self.model = d.model_id.clone();
        }
        let (parts, part_max) = self.parts(db, n, id)?;
        // opencode user rows are always prompts: no injected furniture wears
        // the user role here, so nothing is marked meta.
        self.candidates.push(TurnCandidate {
            role: Some(role),
            part_types: parts.iter().map(|p| p.kind.clone()).collect(),
            ..TurnCandidate::default()
        });
        // Lines is what was consumed: the message and its parts.
        self.acc.lines += 1 + parts.len();
        self.messages.push(Message {
            role,
            provider: first_non_empty(&d.provider_id, HARNESS).to_string(),
            model: d.model_id,
            foreign_id: id.to_string(),
            usage: d.usage,
            raw_json: d.raw_json,
            turn_seq: None,
            parts,
        });
        self.tokens.push(d.tokens);
        self.costs.push(d.cost);
        Ok(part_max)
    }

    /// One message's parts in timeline order, and the largest part
    /// time_updated among those used. Two-phase like messages.
    fn parts(&mut self, db: &Connection, n: usize, msg_id: &str) -> Result<(Vec<Part>, i64), String> {
        let metas: Vec<PartMeta> = db
            .prepare(
                "SELECT id, length(CAST(data AS BLOB)), time_updated
                   FROM part WHERE message_id = ?1 ORDER BY time_created ASC, rowid ASC",
            )
            .and_then(|mut stmt| {
                stmt.query_map([msg_id], |row| Ok(PartMeta { id: row.get(0)?, size: row.get(1)?, updated: row.get(2)? }))?
                    .collect()
            })
            .map_err(|e| e.to_string())?;
        let (mut parts, mut part_max) = (Vec::new(), 0i64);
        for m in &metas {
            match self.part_by_id(db, m) {
                // One bad row is a skip, not a failed message; it still
                // counts as consumed and does not move the watermark.
                Err(e) => {
                    self.acc.skip(n, 0, &e);
                    self.acc.lines += 1;
                }
                Ok(ps) => {
                    part_max = part_max.max(m.updated);
                    parts.extend(ps);
                }
            }
        }
        Ok((parts, part_max))
    }

    fn part_by_id(&mut self, db: &Connection, m: &PartMeta) -> Result<Vec<Part>, String> {
        if m.size <= PART_RAW_LIMIT {
            let data =
                db.query_row("SELECT data FROM part WHERE id = ?1", [&m.id], |row| bytes(row, 0)).map_err(|e| e.to_string())?;
            return Ok(self.part(&m.id, &data));
        }
        let head = db
            .query_row("SELECT substr(data,1,?1) FROM part WHERE id = ?2", rusqlite::params![PART_PREFIX_LEN, m.id], |row| {
                text_or_empty(row, 0)
            })
            .map_err(|e| e.to_string())?;
        let kind = extract_top_json_string(&head, "type").unwrap_or_default();
        if kind.is_empty() {
            return Err(format!("opencode: part {} has no type in its prefix", m.id));
        }
        // Status nests in the state object, so the state is found first and
        // scanned within; input and output ride raw, absent when the cap cut
        // them off rather than half a value.
        let state = extract_top_object(&head, "state").and_then(|obj| {
            let status = extract_top_json_string(obj, "status").unwrap_or_default();
            let raw = |key| extract_top_raw_value(obj, key).and_then(|r| serde_json::from_str::<Value>(r).ok());
            let (input, output) = (raw("input"), raw("output"));
            (!status.is_empty() || input.is_some() || output.is_some()).then_some(ToolState { status, input, output })
        });
        let d = PartData {
            kind,
            tool: extract_top_json_string(&head, "tool").unwrap_or_default(),
            call_id: extract_top_json_string(&head, "callID").unwrap_or_default(),
            state,
        };
        // Only the scanned fields are known, so the payload is a marker; a
        // capped tool is classified so the lossy row stays visible.
        if d.kind == "tool" {
            self.acc.classify("capped-tool");
            return Ok(self.tool_parts(&m.id, &d));
        }
        let marker = json!({ "capped": "part data exceeded 5MB, see source database", "type": d.kind });
        Ok(vec![with_foreign_id(self.acc.normalize_part(canonical_kind(&d.kind), Some(&marker)), &m.id)])
    }

    /// One part row onto one or two canonical parts, the raw payload kept.
    fn part(&mut self, id: &str, data: &[u8]) -> Vec<Part> {
        let text = String::from_utf8_lossy(data);
        let value = crate::decode_line(text.as_bytes()).ok();
        // A payload that is not UTF-8 JSON is kept as a string rather than
        // stored as bytes no reader could decode.
        let usable = value.clone().filter(|_| std::str::from_utf8(data).is_ok());
        let payload = |kind: &str| match &usable {
            _ if data.is_empty() => None,
            Some(v) => Some(v.clone()),
            None if known_part_type(kind) => Some(json!({ "raw": text })),
            None => Some(Value::String(text.to_string())),
        };
        let Some(d) = value.as_ref().and_then(|v| PartData::from_value(v).ok()) else {
            return vec![with_foreign_id(self.acc.normalize_part("part", payload("part").as_ref()), id)];
        };
        if d.kind == "tool" {
            return self.tool_parts(id, &d);
        }
        let kind = canonical_kind(&d.kind);
        vec![with_foreign_id(self.acc.normalize_part(kind, payload(kind).as_ref()), id)]
    }

    /// A tool row as its call, and its result once completed or errored —
    /// a tool still running has returned nothing. Both share the call id
    /// (the part id when the row names none). Another terminal status twins
    /// nothing but is classified, so the row is visible.
    fn tool_parts(&mut self, id: &str, d: &PartData) -> Vec<Part> {
        let call_id = first_non_empty(&d.call_id, id);
        let st = d.state.clone().unwrap_or_default();
        let mut parts = vec![with_foreign_id(new_tool_call_part(call_id, &d.tool, st.input.as_ref()), id)];
        match st.status.as_str() {
            "completed" | "error" => {
                parts.push(with_foreign_id(new_tool_result_part(call_id, st.output.as_ref(), st.status == "error"), id));
            }
            "" | "running" | "pending" => {}
            other => self.acc.classify(&format!("tool:{other}")),
        }
        parts
    }

    fn finish(self) -> (Thread, ReadResult) {
        let binding = |id: &str| Binding {
            provider: crate::PROVIDER_OPENCODE.into(),
            harness: HARNESS.into(),
            foreign_session_id: id.into(),
            resume_cmd: resume_cmd(id),
        };
        let th = Thread {
            worktree: Worktree { name: base_name(&self.worktree_path), path: self.worktree_path.clone(), vcs: self.worktree_vcs.clone() },
            session: Session {
                directory: self.directory.clone(),
                title: self.title.clone(),
                model: self.model.clone(),
                provider: first_non_empty(&self.provider, HARNESS).into(),
                harness: HARNESS.into(),
                ..Session::default()
            },
            binding: binding(&self.r.id),
            parent: (!self.parent_id.is_empty() && self.parent_id != self.r.id).then(|| binding(&self.parent_id)),
            turns: self.turns(),
            events: Vec::new(),
            messages: {
                let mut messages = self.messages;
                crate::link_turns(&mut messages, &split_turns(&self.candidates));
                messages
            },
        };
        (th, self.acc)
    }

    /// Turns costed over their whole span: token columns summed, dollars in
    /// micros, rounded. A span where no message carried a cost keeps no
    /// dollar cost rather than a guessed zero. Every turn is done: nothing in
    /// the transcript tells a cancelled turn from a finished one.
    fn turns(&self) -> Vec<Turn> {
        split_turns(&self.candidates)
            .into_iter()
            .map(|span| {
                let mut t = Turn { status: "done".into(), ..Turn::default() };
                let (mut dollars, mut priced) = (0f64, false);
                for i in span.start..span.end.min(self.tokens.len()) {
                    let tk = self.tokens[i];
                    t.cost_input += tk.input;
                    t.cost_output += tk.output;
                    t.cost_reasoning += tk.reasoning;
                    t.cost_cache_read += tk.cache_read;
                    t.cost_cache_write += tk.cache_write;
                    // Reasoning is tracked apart and not in the total, as in
                    // the Claude reader.
                    t.cost_total += tk.input + tk.output + tk.cache_read + tk.cache_write;
                    if let Some(c) = self.costs[i] {
                        priced = true;
                        dollars += c;
                    }
                }
                t.cost_usd_micros = priced.then(|| (dollars * 1e6).round() as i64);
                t
            })
            .collect()
    }
}

/// opencode's part types onto krowk's; anything else passes through to be
/// counted as unknown.
fn canonical_kind(t: &str) -> &str {
    match t {
        "text" => PART_TEXT,
        // A reader asking "did the model reason here" must get yes.
        "reasoning" => PART_THINKING,
        "file" => PART_FILE,
        "patch" => PART_PATCH,
        "step-start" | "step-finish" => PART_STEP,
        other => other,
    }
}

/// The session-level model backstop, from session.model's JSON; nothing when
/// it is absent or unreadable.
fn session_model_of(raw: &str) -> (String, String) {
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return Default::default() };
    match (object(&v), str_field(&v, "id"), str_field(&v, "providerID")) {
        (Ok(()), Ok(id), Ok(provider)) => (id, provider),
        _ => Default::default(),
    }
}

/// The project row's worktree with its vcs — only git passes through, a novel
/// vcs would read as a promise. A missing or relative worktree (relative
/// resolves against whoever's cwd) falls back to the session directory.
fn resolve_worktree(worktree: &str, vcs: &str, directory: &str) -> (String, String) {
    if worktree.is_empty() || !Path::new(worktree).is_absolute() {
        let dir = if directory.is_empty() { String::new() } else { clean(directory) };
        return (dir, VCS_NONE.into());
    }
    (clean(worktree), if vcs == VCS_GIT { VCS_GIT } else { VCS_NONE }.into())
}

/// Lexical cleaning, as Go's filepath.Clean. A private copy of home.rs's,
/// which drops a leading `..` of a relative path where Go keeps it.
fn clean(p: &str) -> String {
    let mut out = PathBuf::new();
    for c in Path::new(p).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() { ".".into() } else { out.display().to_string() }
}

/// The worktree's display name; an empty path has none.
fn base_name(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// What a person types to get back in. The id rides unquoted, so anything
/// outside [A-Za-z0-9_-] omits it rather than risk injection.
fn resume_cmd(id: &str) -> String {
    if id.is_empty() {
        return String::new();
    }
    if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        format!("opencode run --session {id}")
    } else {
        "opencode run".into()
    }
}

fn first_non_empty<'s>(a: &'s str, b: &'s str) -> &'s str {
    if a.is_empty() { b } else { a }
}

/// Twins share their row's id: the store dedups messages, not parts.
fn with_foreign_id(mut p: Part, id: &str) -> Part {
    p.foreign_id = id.to_string();
    p
}

// The prefix scanners. Every one works on byte offsets of ASCII delimiters,
// so a slice never splits a character. The top-level ones are anchored to
// brace depth 1: the first "role" in the bytes is not necessarily the
// message's, and a role flipped by a nested match would file an assistant
// row as a prompt.

fn skip_ws(h: &[u8], mut p: usize) -> usize {
    while p < h.len() && matches!(h[p], b' ' | b'\t' | b'\n' | b'\r') {
        p += 1;
    }
    p
}

/// Unclosed `{` before `pos`, ignoring braces inside strings. Rescans from
/// the start per call — quadratic, and fine on a ≤4KB prefix.
fn brace_depth_at(h: &[u8], pos: usize) -> usize {
    let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
    for &c in &h[..pos.min(h.len())] {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

/// The offset just past the colon of `"key":` at depth 1.
fn top_key_pos(hay: &str, key: &str) -> Option<usize> {
    let (needle, h) = (format!("\"{key}\""), hay.as_bytes());
    let mut off = 0;
    while off < hay.len() {
        let start = off + hay[off..].find(&needle)?;
        off = start + needle.len();
        if brace_depth_at(h, start) != 1 {
            continue;
        }
        let p = skip_ws(h, off);
        if p < h.len() && h[p] == b':' {
            return Some(p + 1);
        }
    }
    None
}

/// The index of the bracket closing the one at `p`, strings skipped; with
/// `arrays`, `[` `]` count as brackets too.
fn balanced_end(h: &[u8], p: usize, arrays: bool) -> Option<usize> {
    let (mut depth, mut in_str, mut escaped) = (0i64, false, false);
    for (q, &c) in h.iter().enumerate().skip(p) {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'[' if arrays => depth += 1,
            b'}' => depth -= 1,
            b']' if arrays => depth -= 1,
            _ => continue,
        }
        if depth == 0 {
            return Some(q);
        }
    }
    None
}

/// The index of the quote closing the string opening at `p`.
fn string_end(h: &[u8], p: usize) -> Option<usize> {
    let mut escaped = false;
    for (q, &c) in h.iter().enumerate().skip(p + 1) {
        if escaped {
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if c == b'"' {
            return Some(q);
        }
    }
    None
}

/// A top-level string field, its escapes decoded as JSON; a string cut off
/// by the cap is absent, not half-decoded.
fn extract_top_json_string(hay: &str, key: &str) -> Option<String> {
    let h = hay.as_bytes();
    let p = skip_ws(h, top_key_pos(hay, key)?);
    if p >= h.len() || h[p] != b'"' {
        return None;
    }
    serde_json::from_str(&hay[p..=string_end(h, p)?]).ok()
}

fn number_end(h: &[u8], mut p: usize, letters: bool) -> usize {
    while p < h.len() && (matches!(h[p], b'-' | b'+' | b'.' | b'0'..=b'9' | b'e' | b'E') || (letters && h[p].is_ascii_alphabetic())) {
        p += 1;
    }
    p
}

fn extract_top_json_number<'h>(hay: &'h str, key: &str) -> Option<&'h str> {
    let p = skip_ws(hay.as_bytes(), top_key_pos(hay, key)?);
    let end = number_end(hay.as_bytes(), p, false);
    (end > p).then(|| &hay[p..end])
}

fn extract_top_json_float(hay: &str, key: &str) -> Option<f64> {
    extract_top_json_number(hay, key)?.parse().ok()
}

/// The balanced object that is the value of `"key"` at any depth, so a
/// scoped scan stays inside the object it names.
fn extract_object_field<'h>(hay: &'h str, key: &str) -> Option<&'h str> {
    let (needle, h) = (format!("\"{key}\""), hay.as_bytes());
    let mut off = 0;
    while off < hay.len() {
        let start = off + hay[off..].find(&needle)?;
        off = start + needle.len();
        let p = skip_ws(h, off);
        if p >= h.len() || h[p] != b':' {
            continue;
        }
        let p = skip_ws(h, p + 1);
        if p >= h.len() || h[p] != b'{' {
            continue;
        }
        return balanced_end(h, p, false).map(|q| &hay[p..=q]);
    }
    None
}

/// The balanced object that is the value of a top-level key.
fn extract_top_object<'h>(hay: &'h str, key: &str) -> Option<&'h str> {
    let h = hay.as_bytes();
    let p = skip_ws(h, top_key_pos(hay, key)?);
    if p >= h.len() || h[p] != b'{' {
        return None;
    }
    balanced_end(h, p, false).map(|q| &hay[p..=q])
}

/// The raw JSON value of a top-level key — string, object, array or literal.
/// A value cut off by the cap, or not valid JSON, is absent.
fn extract_top_raw_value<'h>(hay: &'h str, key: &str) -> Option<&'h str> {
    let h = hay.as_bytes();
    let p = skip_ws(h, top_key_pos(hay, key)?);
    if p >= h.len() {
        return None;
    }
    let raw = match h[p] {
        b'"' => &hay[p..=string_end(h, p)?],
        b'{' | b'[' => &hay[p..=balanced_end(h, p, true)?],
        _ => {
            let end = number_end(h, p, true);
            if end == p {
                return None;
            }
            &hay[p..end]
        }
    };
    serde_json::from_str::<serde::de::IgnoredAny>(raw).is_ok().then_some(raw)
}

/// `"key": <number>` at any depth, as its literal.
fn extract_json_number<'h>(hay: &'h str, key: &str) -> Option<&'h str> {
    let (needle, h) = (format!("\"{key}\""), hay.as_bytes());
    let mut off = 0;
    while off < hay.len() {
        let start = off + hay[off..].find(&needle)?;
        off = start + needle.len();
        let p = skip_ws(h, off);
        if p >= h.len() || h[p] != b':' {
            continue;
        }
        let p = skip_ws(h, p + 1);
        let end = number_end(h, p, false);
        if end > p {
            return Some(&hay[p..end]);
        }
    }
    None
}

fn extract_json_int(hay: &str, key: &str) -> Option<i64> {
    extract_json_number(hay, key)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{known_part_type, PART_TOOL_CALL, PART_TOOL_RESULT, PART_UNKNOWN};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FIXTURE_SQL: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/opencode/opencode/opencode.sql"));
    const GOLDEN: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/opencode/golden.json"));
    const PARENT: &str = "ses_parent";
    const CHILD: &str = "ses_child";
    const PARENT_CURSOR: i64 = 1757000000405;

    /// A home holding an opencode.db built from the checked-in SQL, and a
    /// worktree directory for the project to point at. The root carries
    /// `?#&` so every test also proves the path never becomes a URI.
    struct Fixture {
        root: PathBuf,
        home: String,
        worktree: String,
        db_file: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            static N: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir()
                .join(format!("krowk-opencode-{}-{}-a?b#c&d", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let (home, worktree) = (root.join("home"), root.join("repo"));
            std::fs::create_dir_all(&worktree).unwrap();
            let dir = home.join(".local/share/opencode");
            std::fs::create_dir_all(&dir).unwrap();
            let f = Fixture {
                home: home.display().to_string(),
                worktree: worktree.display().to_string(),
                db_file: dir.join("opencode.db"),
                root,
            };
            let sql = FIXTURE_SQL.replace("{{WORKTREE}}", &f.worktree);
            assert!(!sql.contains("{{"), "fixture sql has an unfilled placeholder");
            Connection::open(&f.db_file).unwrap().execute_batch(&sql).unwrap();
            f
        }

        fn env(&self) -> impl Fn(&str) -> String + '_ {
            move |k: &str| if k == "HOME" { self.home.clone() } else { String::new() }
        }

        fn discover(&self) -> Vec<Ref> {
            Opencode.discover(&self.env()).unwrap()
        }

        fn try_read(&self, id: &str, cursor: &str) -> Result<(Thread, String, ReadResult), ImportError> {
            Opencode.read(&self.env(), &Ref { provider: "opencode".into(), id: id.into(), path: DB_REL.into() }, cursor)
        }

        fn read(&self, id: &str) -> (Thread, String, ReadResult) {
            self.try_read(id, "").unwrap()
        }

        fn exec(&self, sql: &str, params: impl rusqlite::Params) {
            Connection::open(&self.db_file).unwrap().execute(sql, params).unwrap();
        }

        /// A session of one message holding the given part rows.
        fn session(&self, id: &str, role: &str, parts: &[(&str, &str)]) {
            self.exec(
                "INSERT INTO session (id, project_id, parent_id, directory, title, model, time_created, time_updated) VALUES (?1, 'prj_1', NULL, ?2, 't', NULL, 1, 1)",
                rusqlite::params![id, self.worktree],
            );
            let msg = format!("msg_{id}");
            self.exec(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1757000002000, 1757000002001, ?3)",
                rusqlite::params![msg, id, format!("{{\"role\":\"{role}\"}}")],
            );
            for (pid, data) in parts {
                self.exec(
                    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 1757000002000, 1757000002001, ?4)",
                    rusqlite::params![pid, msg, id, data],
                );
            }
        }

        fn unresolve(&self, s: &str) -> String {
            s.replace(&self.worktree, "{{WORKTREE}}").replace(&self.home, "{{HOME}}")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn parts(th: &Thread) -> impl Iterator<Item = &Part> {
        th.messages.iter().flat_map(|m| &m.parts)
    }

    fn cursor_of(c: &str) -> i64 {
        decode_sqlite_cursor(c).unwrap().time_updated
    }

    fn ids_of(th: &Thread) -> Vec<String> {
        th.messages
            .iter()
            .flat_map(|m| std::iter::once(m.foreign_id.clone()).chain(m.parts.iter().map(|p| format!("{}/{}:{}", m.foreign_id, p.foreign_id, p.kind))))
            .collect()
    }

    /// The Go golden's shape: its canonicalThread, as encoding/json wrote it.
    fn canonical(f: &Fixture, r: &Ref, th: &Thread, cursor: &str, res: &ReadResult) -> Value {
        let binding = |b: &Binding| {
            json!({ "provider": b.provider, "harness": b.harness, "foreign_session_id": b.foreign_session_id, "resume_cmd": b.resume_cmd })
        };
        let counts = |m: &std::collections::BTreeMap<String, usize>| if m.is_empty() { Value::Null } else { json!(m) };
        let mut skipped: Vec<usize> = res.skipped.iter().map(|s| s.line).collect();
        skipped.sort();
        json!({
            "ref": { "provider": r.provider, "id": r.id, "path": r.path, "key": r.key() },
            "worktree": { "Path": f.unresolve(&th.worktree.path), "VCS": th.worktree.vcs, "Name": th.worktree.name },
            "session": {
                "directory": f.unresolve(&th.session.directory), "title": th.session.title, "model": th.session.model,
                "provider": th.session.provider, "harness": th.session.harness,
            },
            "binding": binding(&th.binding),
            "parent": th.parent.as_ref().map(binding),
            "turns": th.turns.iter().map(|t| json!({
                "status": t.status, "cost_input": t.cost_input, "cost_output": t.cost_output, "cost_total": t.cost_total,
                "cost_cache_read": t.cost_cache_read, "cost_cache_write": t.cost_cache_write,
                "cost_reasoning": t.cost_reasoning, "cost_usd_micros": t.cost_usd_micros,
            })).collect::<Vec<_>>(),
            "messages": th.messages.iter().map(|m| json!({
                "role": m.role.as_str(), "provider": m.provider, "model": m.model, "foreign_id": m.foreign_id,
                "usage": m.usage, "raw_json": m.raw_json.as_deref().map(|r| f.unresolve(r)).unwrap_or_default(),
                "parts": m.parts.iter().map(|p| json!({
                    "type": p.kind, "tool_call_id": p.tool_call_id, "signature": p.signature,
                    "foreign_id": p.foreign_id, "data": f.unresolve(&p.data),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "cursor": cursor_of(cursor),
            "result": {
                "lines": res.lines, "unknown": res.unknown, "unknown_types": counts(&res.unknown_types),
                "classified": counts(&res.classified), "skipped_count": res.skipped_count, "skipped_lines": skipped,
            },
        })
    }

    #[test]
    fn golden() {
        let f = Fixture::new();
        let got: Vec<Value> = f
            .discover()
            .iter()
            .map(|r| {
                let (th, cur, res) = Opencode.read(&f.env(), r, "").unwrap();
                canonical(&f, r, &th, &cur, &res)
            })
            .collect();
        let want: Value = serde_json::from_str(GOLDEN).unwrap();
        assert_eq!(Value::Array(got), want);
    }

    #[test]
    fn discover_lists_one_sorted_ref_per_session() {
        let f = Fixture::new();
        let refs = f.discover();
        assert_eq!(refs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), [CHILD, PARENT]);
        assert!(refs.iter().all(|r| r.provider == "opencode" && r.path == DB_REL));
        assert_eq!(refs[1].key(), "opencode:ses_parent");
    }

    #[test]
    fn discover_on_an_empty_or_garbage_machine_is_empty() {
        let f = Fixture::new();
        std::fs::remove_file(&f.db_file).unwrap();
        assert_eq!(f.discover(), vec![]);
        std::fs::write(&f.db_file, "replaced-by-garbage").unwrap();
        assert_eq!(f.discover(), vec![]);
        let missing = f.root.join("nobody").display().to_string();
        assert_eq!(Opencode.discover(&|_: &str| missing.clone()).unwrap(), vec![]);
    }

    /// Importing leaves the database byte-identical and adds no sidecar,
    /// against the WAL mode opencode runs in, with a writer held open.
    #[test]
    fn reads_are_read_only_under_wal() {
        let f = Fixture::new();
        let wal = Connection::open(&f.db_file).unwrap();
        wal.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).unwrap();
        wal.execute_batch(
            "INSERT INTO session (id, project_id, parent_id, directory, title, model, time_created, time_updated) VALUES ('ses_scratch', 'prj_1', NULL, 'x', 'scratch', NULL, 1, 1);
             DELETE FROM session WHERE id = 'ses_scratch';",
        )
        .unwrap();
        let sidecar = |s: &str| std::fs::read(format!("{}{s}", f.db_file.display())).ok();
        let (db, wal_bytes) = (std::fs::read(&f.db_file).unwrap(), sidecar("-wal"));
        let present: Vec<bool> = ["-wal", "-shm", "-journal"].iter().map(|s| sidecar(s).is_some()).collect();
        for r in f.discover() {
            Opencode.read(&f.env(), &r, "").unwrap();
            assert!(!Opencode.unchanged(&f.env(), &r, "{\"time_updated\":1}"));
        }
        assert_eq!(std::fs::read(&f.db_file).unwrap(), db, "database changed by import");
        assert_eq!(sidecar("-wal"), wal_bytes, "-wal changed by import");
        assert_eq!(["-wal", "-shm", "-journal"].iter().map(|s| sidecar(s).is_some()).collect::<Vec<_>>(), present);
    }

    #[test]
    fn tool_results_pair_with_their_calls_and_parts_count_twins() {
        let f = Fixture::new();
        let (th, _, _) = f.read(PARENT);
        let mut calls = std::collections::HashSet::new();
        let mut results = 0;
        for p in parts(&th) {
            assert!(known_part_type(&p.kind) && !p.foreign_id.is_empty(), "{p:?}");
            if p.kind == PART_TOOL_CALL {
                assert!(!p.tool_call_id.is_empty());
                calls.insert(p.tool_call_id.clone());
            } else if p.kind == PART_TOOL_RESULT {
                results += 1;
                assert!(calls.contains(&p.tool_call_id), "tool_result {} has no earlier call", p.tool_call_id);
            }
        }
        // Completed + error twin; running is a call alone.
        assert_eq!(results, 2);
        let running: Vec<&str> = parts(&th).filter(|p| p.tool_call_id == "call_3").map(|p| p.kind.as_str()).collect();
        assert_eq!(running, [PART_TOOL_CALL]);
        // 12 source rows plus one per twin.
        assert_eq!(parts(&th).count(), 12 + results);
        assert_eq!(parts(&f.read(CHILD).0).count(), 2);
    }

    #[test]
    fn turns_are_costed_over_their_span() {
        let f = Fixture::new();
        let (th, _, _) = f.read(PARENT);
        let got: Vec<_> = th
            .turns
            .iter()
            .map(|t| (t.status.as_str(), t.cost_input, t.cost_output, t.cost_reasoning, t.cost_cache_read, t.cost_cache_write, t.cost_total, t.cost_usd_micros))
            .collect();
        assert_eq!(
            got,
            [
                ("done", 9121, 1660, 23, 42496, 120, 9121 + 1660 + 42496 + 120, Some(12345)),
                ("done", 100, 50, 0, 0, 0, 150, Some(100)),
            ]
        );
    }

    #[test]
    fn parent_and_worktree_come_from_the_rows() {
        let f = Fixture::new();
        let (parent, _, _) = f.read(PARENT);
        assert_eq!(parent.parent, None);
        assert_eq!(parent.worktree, Worktree { path: f.worktree.clone(), vcs: VCS_GIT.into(), name: "repo".into() });
        assert_eq!((parent.session.title.as_str(), parent.session.model.as_str()), ("Parent session", "gpt-5.5"));
        assert_eq!((parent.session.provider.as_str(), parent.session.harness.as_str()), ("openai", HARNESS));
        assert_eq!(parent.binding.resume_cmd, "opencode run --session ses_parent");
        assert!(parent.messages.iter().all(|m| m.role != Role::Assistant || m.provider == "openai"));
        let (child, _, _) = f.read(CHILD);
        let p = child.parent.unwrap();
        assert_eq!((p.provider.as_str(), p.foreign_session_id.as_str()), ("opencode", PARENT));
        assert_eq!(child.worktree.path, f.worktree);
    }

    #[test]
    fn a_reimport_moves_only_the_watermark_and_unchanged_follows_it() {
        let f = Fixture::new();
        let r = &f.discover()[1];
        let (first, cur, _) = f.read(PARENT);
        assert_eq!(cursor_of(&cur), PARENT_CURSOR);
        assert!(Opencode.unchanged(&f.env(), r, &cur));
        assert!(!Opencode.unchanged(&f.env(), r, ""));
        let (again, cur2, _) = f.try_read(PARENT, &cur).unwrap();
        assert_eq!((cursor_of(&cur2), ids_of(&again)), (PARENT_CURSOR, ids_of(&first)));
        f.exec("UPDATE message SET time_updated = ?1 WHERE id = 'msg_a2'", [PARENT_CURSOR + 5000]);
        assert!(!Opencode.unchanged(&f.env(), r, &cur));
        assert_eq!(changed_since(&f.env(), r, PARENT_CURSOR), Ok(true));
        let (third, cur3, _) = f.try_read(PARENT, &cur2).unwrap();
        assert_eq!((cursor_of(&cur3), ids_of(&third)), (PARENT_CURSOR + 5000, ids_of(&first)));
    }

    fn find<'t>(th: &'t Thread, id: &str) -> &'t Message {
        th.messages.iter().find(|m| m.foreign_id == id).expect("message missing from import")
    }

    /// The OOM regression: a huge row keeps its identity with no raw.
    #[test]
    fn an_oversized_message_keeps_identity_and_drops_raw() {
        let f = Fixture::new();
        let big = json!({
            "role": "assistant", "modelID": "gpt-5.5", "providerID": "openai",
            "summary": { "diffs": "x".repeat(300 * 1024) },
            "tokens": { "input": 7, "output": 3, "reasoning": 0, "cache": { "read": 0, "write": 0 } }, "cost": 0.00042,
        });
        f.exec(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('msg_big', 'ses_parent', 1757000000500, 1757000000501, ?1)",
            [big.to_string()],
        );
        f.exec("INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('prt_big_text', 'msg_big', 'ses_parent', 1757000000500, 1757000000501, '{\"type\":\"text\",\"text\":\"big answer\"}')", []);
        let huge = json!({ "role": "user", "summary": { "diffs": "d".repeat(1024 * 1024) } });
        f.exec(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('msg_huge_user', 'ses_child', 1757000001300, 1757000001301, ?1)",
            [huge.to_string()],
        );
        f.exec("INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('prt_huge_text', 'msg_huge_user', 'ses_child', 1757000001300, 1757000001301, '{\"type\":\"text\",\"text\":\"summary\"}')", []);

        let (th, _, res) = f.read(PARENT);
        let m = find(&th, "msg_big");
        assert_eq!((m.role, m.model.as_str(), m.provider.as_str(), m.raw_json.as_ref()), (Role::Assistant, "gpt-5.5", "openai", None));
        assert_eq!(m.usage, "", "the tokens object is past the prefix");
        assert_eq!(res.skipped_count, 0);

        let (th, _, res) = f.read(CHILD);
        let m = find(&th, "msg_huge_user");
        assert_eq!((m.role, m.provider.as_str(), m.usage.as_str(), m.raw_json.as_ref(), m.parts.len()), (Role::User, HARNESS, "", None, 1));
        assert_eq!(res.skipped_count, 0);
    }

    #[test]
    fn a_prefix_with_tokens_up_front_keeps_them() {
        let f = Fixture::new();
        let data = format!(r#"{{"role":"assistant","tokens":{{"input":7,"cache":{{"read":2}}}},"cost":0.5,"pad":"{}"}}"#, "p".repeat(300_000));
        f.session("ses_tok", "user", &[]);
        f.exec("INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('msg_tok', 'ses_tok', 1757000002002, 1757000002003, ?1)", [data]);
        let (th, _, _) = f.read("ses_tok");
        assert_eq!(find(&th, "msg_tok").usage, r#"{"input":7,"cache":{"read":2}}"#);
        let t = &th.turns[0];
        assert_eq!((t.cost_input, t.cost_cache_read, t.cost_total, t.cost_usd_micros), (7, 2, 9, Some(500_000)));
    }

    #[test]
    fn prefix_scans_are_anchored_to_the_top_level() {
        let hay = r#"{"nested":{"role":"user","input":99},"role":"assistant","modelID":"m"}"#;
        assert_eq!(extract_top_json_string(hay, "role").as_deref(), Some("assistant"));
        let hay2 = r#"{"role":"assistant","tokens":{"input":5},"cost":0.5}"#;
        let tok = extract_top_object(hay2, "tokens").unwrap();
        assert_eq!(extract_json_int(tok, "input"), Some(5));
        assert_eq!(extract_top_json_number(hay2, "input"), None);
        assert_eq!(extract_top_json_float(hay2, "cost"), Some(0.5));

        let esc = r#"{"type":"text","text":"line1\nline2 \"quoted\" café back\\slash \/slash","pad":1}"#;
        assert_eq!(extract_top_json_string(esc, "text").as_deref(), Some("line1\nline2 \"quoted\" café back\\slash /slash"));
        assert_eq!(extract_top_json_string(r#"{"type":"text","state":{"text":"decoy"},"text":"a\tb"}"#, "text").as_deref(), Some("a\tb"));
        assert_eq!(extract_top_json_string(r#"{"type":"text","text":"abc"#, "text"), None);
        assert_eq!(extract_top_raw_value(r#"{"a":[1,{"b":"]"}],"c":tru"#, "a"), Some(r#"[1,{"b":"]"}]"#));
        assert_eq!(extract_top_raw_value(r#"{"c":tru"#, "c"), None);
    }

    #[test]
    fn read_refuses_a_foreign_path_a_bad_cursor_and_a_missing_session() {
        let f = Fixture::new();
        let r = Ref { provider: "opencode".into(), id: PARENT.into(), path: "/etc/passwd".into() };
        assert!(Opencode.read(&f.env(), &r, "").is_err());
        assert!(f.try_read(PARENT, "not a cursor").is_err());
        assert!(f.try_read("ses_gone", "").is_err());
    }

    #[test]
    fn an_oversized_part_keeps_its_routing() {
        let f = Fixture::new();
        let big = format!(r#"{{"type":"text","text":"{}"}}"#, "y".repeat(PART_RAW_LIMIT as usize + 100));
        f.session("ses_cap", "user", &[("prt_cap_big", &big)]);
        let (th, _, res) = f.read("ses_cap");
        let p = &th.messages[0].parts;
        assert_eq!((p.len(), p[0].kind.as_str(), p[0].foreign_id.as_str()), (1, PART_TEXT, "prt_cap_big"));
        assert_eq!(res.skipped_count, 0);
    }

    #[test]
    fn oversized_tool_parts_twin_off_the_scanned_state() {
        let f = Fixture::new();
        let pad = "z".repeat(PART_RAW_LIMIT as usize + 100);
        let input = json!({ "cmd": "echo \"hi\"\ncafé" }).to_string();
        let err = format!(r#"{{"type":"tool","tool":"bash","callID":"c1","state":{{"status":"error","input":{{"cmd":"x"}},"output":"boom"}},"pad":"{pad}"}}"#);
        let ok = format!(r#"{{"type":"tool","tool":"bash","callID":"c2","state":{{"status":"completed","input":{input},"output":"line1\nline2 \"q\""}},"pad":"{pad}"}}"#);
        f.session("ses_capt", "assistant", &[("prt_err", &err), ("prt_ok", &ok)]);
        let (th, _, res) = f.read("ses_capt");
        let data: Vec<(&str, Value)> = parts(&th).map(|p| (p.kind.as_str(), serde_json::from_str(&p.data).unwrap())).collect();
        assert_eq!(
            data,
            [
                (PART_TOOL_CALL, json!({ "name": "bash", "input": { "cmd": "x" } })),
                (PART_TOOL_RESULT, json!({ "output": "boom", "is_error": true })),
                (PART_TOOL_CALL, json!({ "name": "bash", "input": { "cmd": "echo \"hi\"\ncafé" } })),
                (PART_TOOL_RESULT, json!({ "output": "line1\nline2 \"q\"", "is_error": false })),
            ]
        );
        assert_eq!(res.classified.get("capped-tool"), Some(&2));
    }

    #[test]
    fn unknown_parts_and_statuses_are_counted() {
        let f = Fixture::new();
        f.session(
            "ses_unk",
            "assistant",
            &[
                ("prt_unk", r#"{"type":"future-widget","frobnicate":true}"#),
                ("prt_ts", r#"{"type":"tool","tool":"bash","callID":"call_x","state":{"status":"timeout","input":{},"output":"late"}}"#),
            ],
        );
        let (th, _, res) = f.read("ses_unk");
        assert_eq!((res.unknown, res.unknown_types.get("future-widget")), (1, Some(&1)));
        let kinds: Vec<&str> = parts(&th).map(|p| p.kind.as_str()).collect();
        assert_eq!(kinds, [PART_UNKNOWN, PART_TOOL_CALL]);
        assert_eq!(res.classified.get("tool:timeout"), Some(&1));
    }

    #[test]
    fn a_bad_part_is_skipped_and_its_message_kept() {
        let f = Fixture::new();
        let bad = format!(r#"{{"blob":"{}"}}"#, "q".repeat(PART_RAW_LIMIT as usize + 100));
        f.session("ses_bp", "user", &[("prt_good", r#"{"type":"text","text":"kept"}"#), ("prt_bad", &bad)]);
        let (th, _, res) = f.read("ses_bp");
        assert_eq!(th.messages.len(), 1);
        assert_eq!(th.messages[0].parts.iter().map(|p| p.foreign_id.as_str()).collect::<Vec<_>>(), ["prt_good"]);
        assert_eq!(res.skipped_count, 1);
    }

    #[test]
    fn the_session_row_backstops_the_model_and_no_cost_is_no_cost() {
        let f = Fixture::new();
        f.session("ses_fb", "user", &[("prt_fb", r#"{"type":"text","text":"hi"}"#)]);
        f.exec("UPDATE session SET model = '{\"id\":\"fallback-model\",\"providerID\":\"fallback-provider\"}' WHERE id = 'ses_fb'", []);
        let (th, _, _) = f.read("ses_fb");
        assert_eq!((th.session.model.as_str(), th.session.provider.as_str()), ("fallback-model", "fallback-provider"));
        assert!(th.turns.iter().all(|t| t.cost_usd_micros.is_none()));
    }

    #[test]
    fn a_missing_project_falls_back_to_the_directory() {
        let f = Fixture::new();
        f.session("ses_np", "user", &[]);
        f.exec("UPDATE session SET project_id = 'prj_gone' WHERE id = 'ses_np'", []);
        let (th, _, _) = f.read("ses_np");
        assert_eq!((th.worktree.path.as_str(), th.worktree.vcs.as_str()), (f.worktree.as_str(), VCS_NONE));
    }

    #[test]
    fn the_watermark_follows_parts_and_holds_below_skips() {
        let f = Fixture::new();
        f.session("ses_wm", "user", &[("prt_wm1", r#"{"type":"text","text":"hi"}"#)]);
        f.exec("UPDATE part SET time_updated = 1757000011401 WHERE id = 'prt_wm1'", []);
        f.exec("INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('msg_wm2', 'ses_wm', 1757000002500, 1757000020000, '{\"role\":\"bogus\"}')", []);
        let (th, cur, res) = f.read("ses_wm");
        assert_eq!(cursor_of(&cur), 1757000011401);
        assert_eq!((th.messages.len(), res.skipped_count, res.classified.get("role:bogus")), (1, 1, Some(&1)));
    }

    #[test]
    fn resume_cmd_omits_an_unsafe_id() {
        assert_eq!(resume_cmd("ses-abc_123"), "opencode run --session ses-abc_123");
        for bad in ["ses x", "ses\"x", "ses;rm -rf", "ses$(id)", "ses|less", "../ses"] {
            assert_eq!(resume_cmd(bad), "opencode run", "{bad}");
        }
        assert_eq!(resume_cmd(""), "");
    }

    #[test]
    fn a_relative_worktree_falls_back_to_the_directory() {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(resolve_worktree("relative/path", "git", "/abs/dir"), s("/abs/dir", VCS_NONE));
        assert_eq!(resolve_worktree("relative/path", "git", ""), s("", VCS_NONE));
        assert_eq!(resolve_worktree("/abs/wt/", "git", "/abs/dir"), s("/abs/wt", VCS_GIT));
        assert_eq!(resolve_worktree("/abs/wt", "hg", ""), s("/abs/wt", VCS_NONE));
        assert_eq!((clean("a/../../b"), base_name("/"), base_name("")), ("../b".into(), "/".into(), String::new()));
    }

    #[test]
    fn each_message_names_its_turn_so_a_model_switch_prices_per_turn() {
        let f = Fixture::new();
        f.exec(
            "INSERT INTO session (id, project_id, parent_id, directory, title, model, time_created, time_updated) VALUES ('ses_hop', 'prj_1', NULL, ?1, 't', NULL, 1, 1)",
            rusqlite::params![f.worktree],
        );
        let rows = [
            ("msg_1", r#"{"role":"user"}"#, Some("first")),
            ("msg_2", r#"{"role":"assistant","modelID":"m-a","providerID":"p"}"#, None),
            ("msg_3", r#"{"role":"user"}"#, Some("second")),
            ("msg_4", r#"{"role":"assistant","modelID":"m-b","providerID":"p"}"#, None),
        ];
        for (i, (id, data, text)) in rows.iter().enumerate() {
            let t = 1_757_000_000_000_i64 + i as i64;
            f.exec(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, 'ses_hop', ?2, ?2, ?3)",
                rusqlite::params![id, t, data],
            );
            if let Some(text) = text {
                f.exec(
                    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 'ses_hop', ?3, ?3, ?4)",
                    rusqlite::params![format!("prt_{id}"), id, t, json!({ "type": "text", "text": text }).to_string()],
                );
            }
        }
        let (th, _, _) = f.read("ses_hop");
        assert_eq!(th.turns.len(), 2);
        let links: Vec<(Option<i64>, &str)> = th.messages.iter().map(|m| (m.turn_seq, m.model.as_str())).collect();
        assert_eq!(links, vec![(Some(0), ""), (Some(0), "m-a"), (Some(1), ""), (Some(1), "m-b")]);
    }
}
