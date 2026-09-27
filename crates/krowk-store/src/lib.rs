//! krowk.db: the local session store — every agent thread on this machine,
//! whichever harness wrote it, read into one schema.
//!
//! The schema is the compatibility boundary with the Go build: a krowk.db
//! either one wrote opens in the other, so the SQL is the Go build's, verbatim,
//! and the gate refuses what v1 never writes rather than repairing it — every
//! row is re-derivable from transcripts, so `krowk sessions rebuild` is the
//! migration.

mod clock;
mod ledger;
mod query;
mod writer;

pub use clock::{new_id, now_ms};
pub use ledger::*;
pub use query::*;
pub use writer::*;
pub use rusqlite::Connection;

use rusqlite::OpenFlags;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The v1 schema, applied once to a fresh file.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");
pub const SCHEMA_VERSION: i64 = 1;

/// The process environment, as the CLI reads it.
pub type Env<'a> = &'a dyn Fn(&str) -> String;

/// A store failure. Most are a sentence for the person; two are shapes the
/// CLI answers differently.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreError {
    /// Nowhere for krowk.db to live: no home, or a KROWK_HOME that is not
    /// absolute.
    NoHome(String),
    /// The file is not the v1 schema; `krowk sessions rebuild` is the fix.
    SchemaMismatch(String),
    /// No such session.
    NotFound(String),
    /// A reference that names several sessions.
    Ambiguous { message: String, ids: Vec<String> },
    Other(String),
}

impl StoreError {
    pub fn message(&self) -> &str {
        match self {
            StoreError::NoHome(m) | StoreError::SchemaMismatch(m) | StoreError::NotFound(m) | StoreError::Other(m) => m,
            StoreError::Ambiguous { message, .. } => message,
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for StoreError {}

pub(crate) fn other(what: &str, e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(format!("store: {what}: {e}"))
}

/// `sessions/krowk.db` in krowk's home (`krowk_api::home`); an error when
/// there is no home to put it in — a store must never fall back to
/// /krowk.db or the working directory.
pub fn db_path(env: Env) -> Result<PathBuf, StoreError> {
    match krowk_api::home::dir(env) {
        Ok(home) => Ok(home.join(krowk_api::home::SESSIONS).join("krowk.db")),
        Err(e) => Err(StoreError::NoHome(format!("store: {}", e.fix()))),
    }
}

fn rebuild_hint(path: &Path, why: &str) -> StoreError {
    StoreError::SchemaMismatch(format!(
        "store: schema mismatch: {why}; run `krowk sessions rebuild` (delete {} and re-import)",
        path.display()
    ))
}

/// Opens krowk.db, creating it with the v1 schema when it is new. The
/// directory is 0700 and the files 0600: transcripts are what an agent was
/// told, secrets included. A file that is not v1 is refused, never repaired.
pub fn open(env: Env) -> Result<Connection, StoreError> {
    let path = db_path(env)?;
    let dir = path.parent().expect("a db path has a directory");
    create_private_dir(dir).map_err(|e| other(&format!("mkdir {}", dir.display()), e))?;
    tighten(dir, 0o700).map_err(|e| other(&format!("chmod {}", dir.display()), e))?;

    // An existing file is inspected read-only and immutable before anything
    // opens it for writing, so a file Open is about to refuse stays untouched.
    let before = std::fs::metadata(&path).ok();
    if before.is_some() {
        check_schema_file(&path)?;
    }
    let file = open_private(&path).map_err(|e| other(&format!("create {}", path.display()), e))?;
    if let (Some(before), Ok(now)) = (&before, file.metadata())
        && !same_file(before, &now)
    {
        return Err(StoreError::Other(format!("store: {} changed during open, refusing", path.display())));
    }
    drop(file);
    tighten(&path, 0o600).map_err(|e| other(&format!("chmod {}", path.display()), e))?;

    let conn = connect(&path).map_err(|e| other(&format!("open {}", path.display()), e))?;
    ensure_schema(&conn, &path)?;
    // WAL so readers never block the writer, NORMAL synchronous — safe under
    // WAL — flipped once the gate has accepted the file, never before a refusal.
    conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| other(&format!("persist journal_mode {}", path.display()), e))?;
    conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| other(&format!("persist synchronous {}", path.display()), e))?;
    let (version, tables) = read_version_and_tables(&conn).map_err(|e| other(&format!("read schema version {}", path.display()), e))?;
    if version == 0 {
        return Err(rebuild_hint(&path, "krowk.db regressed to schema version 0 after the gate accepted it"));
    }
    check_schema_content(&path, version, &tables)?;
    for side in ["-wal", "-shm", "-journal"] {
        let side = PathBuf::from(format!("{}{side}", path.display()));
        if side.exists() {
            tighten(&side, 0o600).map_err(|e| other(&format!("chmod {}", side.display()), e))?;
        }
    }
    Ok(conn)
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "foreign_keys", 1)?;
    Ok(conn)
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

/// Clears group and other bits when any are set.
fn tighten(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
    }
    let _ = (path, mode);
    Ok(())
}

fn expected_tables() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in SCHEMA_SQL.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("CREATE TABLE ") {
            let name = rest.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or_default().to_string();
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

fn read_version_and_tables(conn: &Connection) -> rusqlite::Result<(i64, Vec<String>)> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let tables = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((version, tables))
}

fn check_schema_content(path: &Path, version: i64, tables: &[String]) -> Result<(), StoreError> {
    if version == 0 {
        if tables.iter().any(|t| !t.starts_with("sqlite_")) {
            return Err(rebuild_hint(path, "krowk.db holds tables at schema version 0, which Open never writes"));
        }
        return Ok(());
    }
    if version != SCHEMA_VERSION {
        return Err(rebuild_hint(path, &format!("krowk.db schema version {version} (want {SCHEMA_VERSION})")));
    }
    if tables.iter().any(|t| t == "migrations") {
        return Err(rebuild_hint(path, "krowk.db has a migrations table, which v1 never creates"));
    }
    for want in expected_tables() {
        if !tables.contains(&want) {
            return Err(rebuild_hint(path, &format!("krowk.db is missing table {want:?} (schema version {SCHEMA_VERSION})")));
        }
    }
    Ok(())
}

/// The existing file, read without a lock, without WAL recovery and without
/// flipping its journal mode — so one about to be refused stays byte-identical.
fn check_schema_file(path: &Path) -> Result<(), StoreError> {
    // Escaped, because SQLite reads a URI: an unescaped `?` in a directory
    // name would start the parameters, and `#` would cut the path short.
    let escaped = path.display().to_string().replace('%', "%25").replace('?', "%3f").replace('#', "%23");
    let uri = format!("file:{escaped}?mode=ro&immutable=1");
    let inspected = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        .and_then(|c| read_version_and_tables(&c));
    match inspected {
        Ok((version, tables)) => check_schema_content(path, version, &tables),
        Err(e) if e.to_string().contains("file is not a database") || e.to_string().contains("malformed") => {
            Err(rebuild_hint(path, &format!("krowk.db is unreadable ({e})")))
        }
        Err(e) => Err(other(&format!("inspect {}", path.display()), e)),
    }
}

/// The comment lines dropped, so the schema executes as one batch.
fn schema_statements() -> String {
    SCHEMA_SQL.lines().filter(|l| !l.trim_start().starts_with("--")).collect::<Vec<_>>().join("\n")
}

/// Creates the schema on a fresh file. Two first opens can race; the loser
/// adopts the winner's schema rather than reporting "table already exists".
fn ensure_schema(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    let (version, tables) = read_version_and_tables(conn).map_err(|e| other(&format!("read schema version {}", path.display()), e))?;
    if version != 0 {
        return check_schema_content(path, version, &tables);
    }
    check_schema_content(path, version, &tables)?;
    let applied = conn.execute_batch(&format!("BEGIN IMMEDIATE;\n{}\nPRAGMA user_version = {SCHEMA_VERSION};\nCOMMIT;", schema_statements()));
    if let Err(apply) = applied {
        let _ = conn.execute_batch("ROLLBACK");
        let (v2, t2) = read_version_and_tables(conn).map_err(|_| other(&format!("init schema {}", path.display()), &apply))?;
        check_schema_content(path, v2, &t2)?;
        if v2 == 0 {
            return Err(other(&format!("init schema {}", path.display()), apply));
        }
    }
    Ok(())
}

/// The store's health, in the shape every doctor check reads as.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StatusCheck {
    pub name: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hint: String,
}

pub fn check(env: Env) -> StatusCheck {
    let status = |status: &str, message: String, hint: String| StatusCheck { name: "store".into(), status: status.into(), message, hint };
    let path = match db_path(env) {
        Ok(p) => p,
        Err(e) => return status("fail", "krowk.db has nowhere to live".into(), e.message().trim_start_matches("store: ").into()),
    };
    match open(env) {
        Ok(_) => status("pass", format!("healthy ({}, schema v{SCHEMA_VERSION}, wal)", path.display()), String::new()),
        Err(StoreError::SchemaMismatch(m)) => status(
            "fail",
            m,
            format!("run `krowk sessions rebuild` (delete {} and re-import)", path.display()),
        ),
        Err(e) => {
            let msg = e.message().to_string();
            let msg = if msg.contains(&path.display().to_string()) { msg } else { format!("{}: {msg}", path.display()) };
            status("fail", msg, format!("inspect {}", path.display()))
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    /// A fresh HOME for one test, removed when it goes out of scope.
    pub struct Home(pub PathBuf);

    impl Home {
        pub fn new(name: &str) -> Home {
            let dir = std::env::temp_dir().join(format!("krowk-store-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Home(dir)
        }

        pub fn env(&self) -> impl Fn(&str) -> String + '_ {
            move |k| if k == "HOME" { self.0.display().to_string() } else { String::new() }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Home;
    use super::*;

    #[test]
    fn a_home_whose_path_reads_as_a_uri_still_reopens_its_store() {
        let home = Home::new("uri?mode=rw#frag%41");
        let env = home.env();
        drop(open(&env).unwrap());
        // The second open inspects the existing file through a file: URI.
        let conn = open(&env).expect("reopen through the escaped URI");
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn a_fresh_store_is_v1_private_and_reopens() {
        let home = Home::new("fresh");
        let env = home.env();
        drop(open(&env).unwrap());
        let path = db_path(&env).unwrap();
        let (version, tables) = read_version_and_tables(&connect(&path).unwrap()).unwrap();
        assert_eq!(version, 1);
        assert!(expected_tables().iter().all(|t| tables.contains(t)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        drop(open(&env).unwrap());
        assert_eq!(check(&env).status, "pass");
    }

    #[test]
    fn a_store_that_is_not_v1_is_refused_with_the_rebuild_hint() {
        let home = Home::new("mismatch");
        let env = home.env();
        let path = db_path(&env).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE stray (x); PRAGMA user_version = 7;").unwrap();
        let err = open(&env).unwrap_err();
        assert!(matches!(err, StoreError::SchemaMismatch(_)) && err.message().contains("sessions rebuild"), "{err}");
        assert_eq!(check(&env).status, "fail");
        std::fs::write(&path, "not a database at all, just text that is long enough").unwrap();
        assert!(matches!(open(&env).unwrap_err(), StoreError::SchemaMismatch(_)));
    }

    #[test]
    fn no_home_is_no_store() {
        assert!(db_path(&|_: &str| String::new()).is_err());
        assert!(db_path(&|k: &str| if k == "HOME" { "relative".into() } else { String::new() }).is_err());
        assert!(matches!(open(&|_: &str| String::new()).unwrap_err(), StoreError::NoHome(_)));
    }
}
