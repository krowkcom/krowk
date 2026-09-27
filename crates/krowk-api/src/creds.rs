//! The credentials file, `credentials.json` in krowk's home: the one file
//! that holds a secret. The registry's keys (one per workspace, and which is
//! the default) live here at the top level; the harness keeps its provider
//! logins and stored API keys beside them (`instances`, `keys`). Each writer
//! reads the whole file into its own shape and keeps every key it does not
//! know, so a login never drops a token and a refresh never drops a key.
//!
//! One lock, one write path (`modify`): every change is a read-modify-write
//! under `credentials.lock`, and the file is replaced by rename, `0600`,
//! never written in place — and never over a file that could not be read,
//! which would replace everything stored in it. A file that is not JSON is
//! named with a line and column only, never serde's words, which quote the
//! value they could not take: a key, as often as not.

use crate::error::{fail, Error};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The process environment, as the CLI reads it. Passed rather than read so
/// a test can hand in its own.
pub type Env<'a> = &'a dyn Fn(&str) -> String;

/// How long a writer waits for another krowk's write (a token refresh) to
/// finish.
pub const LOCK_WAIT: Duration = Duration::from_secs(45);

/// Where a file stops being JSON krowk reads, in the only words an error
/// about it may use.
pub fn not_valid(path: &Path, e: &serde_json::Error) -> String {
    format!("{} is not valid (line {}, column {})", path.display(), e.line(), e.column())
}

/// The file as `T`, or `T::default()` when there is none.
pub fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T, String> {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|e| not_valid(path, &e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("{} cannot be read: {e}", path.display())),
    }
}

/// Replaces the file by rename with `v`, `0600`, synced before the rename.
pub fn write<T: Serialize>(path: &Path, v: &T) -> Result<(), String> {
    let io = |e: std::io::Error| format!("{} could not be written: {e}", path.display());
    let dir = path.parent().unwrap_or(Path::new("."));
    let data = serde_json::to_string_pretty(v).expect("credentials serialize") + "\n";
    let (mut f, tmp) = crate::tempfile::create_file(dir, ".credentials-", ".json", 0o600).map_err(io)?;
    let result = f.write_all(data.as_bytes()).and_then(|()| f.sync_all()).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(io)
}

/// The file's lock, held until dropped: one writer at a time across every
/// krowk on the host.
pub struct Lock(#[allow(dead_code)] std::fs::File);

/// The lock, if nobody holds it. Never blocks, so an async caller can poll
/// it and still hear Ctrl-C.
pub fn try_lock(path: &Path) -> Result<Option<Lock>, String> {
    let lock = path.with_extension("lock");
    let mut o = std::fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o.open(&lock).map_err(|e| format!("{} could not be opened: {e}", lock.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(Lock(f))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{} could not be locked: {e}", lock.display())),
    }
}

/// Why a lock was not had in time.
pub fn busy(path: &Path) -> String {
    format!("another krowk held {} for {} seconds — run the command again", path.display(), LOCK_WAIT.as_secs())
}

/// The lock, waited for from blocking code.
pub fn lock(path: &Path) -> Result<Lock, String> {
    let until = Instant::now() + LOCK_WAIT;
    loop {
        if let Some(l) = try_lock(path)? {
            return Ok(l);
        }
        if Instant::now() > until {
            return Err(busy(path));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The one way the file changes, for a caller already holding the lock:
/// read into `T`, edit, and written back when the edit says it changed
/// something.
pub fn modify_locked<T: DeserializeOwned + Serialize + Default, R>(path: &Path, edit: impl FnOnce(&mut T) -> Result<(R, bool), String>) -> Result<R, String> {
    let mut v = read(path)?;
    let (r, changed) = edit(&mut v)?;
    if changed {
        write(path, &v)?;
    }
    Ok(r)
}

/// `modify_locked` under the lock.
pub fn modify<T: DeserializeOwned + Serialize + Default, R>(path: &Path, edit: impl FnOnce(&mut T) -> Result<(R, bool), String>) -> Result<R, String> {
    let _lock = lock(path)?;
    modify_locked(path, edit)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct StoredKey {
    #[serde(default)]
    token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    key_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    workspace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    workspace_name: String,
}

/// The registry's part of the file, and the rest of it as it was.
#[derive(Debug, Clone, Default)]
struct Credentials {
    default: String,
    workspaces: BTreeMap<String, StoredKey>,
    /// The harness's sections, and whatever a later krowk writes: kept.
    other: Map<String, Value>,
}

/// The name a key is stored under when the registry named no workspace.
const DEFAULT_ENTRY: &str = "default";

/// Which key a stored login is, without the token.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Identity {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace_name: String,
}

/// One stored key as `krowk workspaces` lists it.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct WorkspaceKey {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace_name: String,
    pub default: bool,
}

pub const TOKEN_SOURCE_ENV: &str = "KROWK_TOKEN";
pub const TOKEN_SOURCE_FILE: &str = "credentials file";
pub const TOKEN_SOURCE_NONE: &str = "none";

/// `credentials.json` in krowk's home.
pub fn credentials_path() -> Result<PathBuf, Error> {
    Ok(crate::home::get()?.join(crate::home::CREDENTIALS))
}

/// The path, in words, for a message.
pub fn credentials_text() -> String {
    credentials_path().map(|p| p.display().to_string()).unwrap_or_else(|_| "(no home directory)".into())
}

fn refusing(cause: String) -> Error {
    fail("cli_error", format!("{cause} — refusing to write, because writing over a file that cannot be read would replace every key and login stored in it"))
}

fn read_lenient() -> Credentials {
    credentials_path().ok().and_then(|p| parse(&p).ok()).unwrap_or_default()
}

/// The registry's keys, from a file that may still be the single-key one
/// every login wrote before workspaces existed.
fn parse(path: &Path) -> Result<Credentials, String> {
    let mut other: Map<String, Value> = read(path)?;
    let mut field = |k: &str| match other.remove(k) {
        Some(Value::String(s)) => s,
        _ => String::new(),
    };
    let default = field("default");
    let c = match other.remove("workspaces") {
        // Never serde's words here either: they would quote a key.
        Some(w) => Credentials { default, workspaces: serde_json::from_value(w).map_err(|_| format!("{} is not valid (its workspaces are not krowk's)", path.display()))?, other },
        None if other.contains_key("token") => {
            let mut field = |k: &str| match other.remove(k) {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            let legacy = StoredKey { token: field("token"), key_id: field("key_id"), workspace: field("workspace"), workspace_name: field("workspace_name") };
            let mut c = Credentials { default, other, ..Credentials::default() };
            if !legacy.token.is_empty() {
                c.default = if legacy.workspace.is_empty() { DEFAULT_ENTRY.to_string() } else { legacy.workspace.clone() };
                c.workspaces.insert(c.default.clone(), legacy);
            }
            c
        }
        None => Credentials { default, other, ..Credentials::default() },
    };
    Ok(c)
}

impl Credentials {
    /// The whole file again, the registry's part put back beside the rest.
    fn into_map(self) -> Map<String, Value> {
        let mut m = self.other;
        if !self.default.is_empty() {
            m.insert("default".into(), Value::String(self.default));
        }
        if !self.workspaces.is_empty() {
            m.insert("workspaces".into(), serde_json::to_value(self.workspaces).expect("keys serialize"));
        }
        m
    }
}

/// The one change the registry's keys go through: under the file's lock,
/// read (refusing a file that cannot be), edited, written back when changed.
fn change<R>(edit: impl FnOnce(&mut Credentials) -> Result<(R, bool), Error>) -> Result<R, Error> {
    let path = credentials_path()?;
    let _lock = lock(&path).map_err(|e| fail("cli_error", e))?;
    let mut c = parse(&path).map_err(refusing)?;
    let (r, changed) = edit(&mut c)?;
    if changed {
        write(&path, &c.into_map()).map_err(|e| fail("cli_error", e))?;
    }
    Ok(r)
}

/// The legacy shape, normalised, as a JSON object: what the move from an
/// older krowk's file merges.
pub fn registry_section(path: &Path) -> Result<Map<String, Value>, String> {
    Ok(parse(path)?.into_map())
}

impl Credentials {
    fn entry(&self, workspace: &str) -> Option<&StoredKey> {
        let name = if workspace.is_empty() { &self.default } else { workspace };
        if name.is_empty() {
            return None;
        }
        self.workspaces.get(name)
    }

    fn names(&self) -> Vec<String> {
        self.workspaces.keys().cloned().collect()
    }
}

/// The token a call would carry, lenient: KROWK_TOKEN, else the named or
/// default workspace's stored key, else "".
pub fn read_token(env: Env, workspace: &str) -> String {
    let t = env("KROWK_TOKEN");
    if !t.is_empty() {
        return t;
    }
    read_lenient().entry(workspace).map(|k| k.token.clone()).unwrap_or_default()
}

/// The token a call must carry. A workspace that was named and holds no key is
/// a refusal, not an anonymous upload; so is a default naming nothing stored.
pub fn resolve_token(env: Env, workspace: &str) -> Result<String, Error> {
    let t = env("KROWK_TOKEN");
    if !t.is_empty() {
        return Ok(t);
    }
    let c = read_lenient();
    if !workspace.is_empty() {
        if let Some(k) = c.workspaces.get(workspace).filter(|k| !k.token.is_empty()) {
            return Ok(k.token.clone());
        }
        let names = c.names();
        if !names.is_empty() {
            return Err(fail(
                "no_key_for_workspace",
                format!(
                    "no key is stored for workspace {workspace} — stored: {}; run `krowk login` to add one",
                    names.join(", ")
                ),
            ));
        }
        return Err(fail(
            "no_key_for_workspace",
            format!(
                "no key is stored for workspace {workspace}, and nothing else is stored either — run `krowk login` first"
            ),
        ));
    }
    if c.default.is_empty() {
        return Ok(String::new());
    }
    if let Some(k) = c.workspaces.get(&c.default).filter(|k| !k.token.is_empty()) {
        return Ok(k.token.clone());
    }
    let names = c.names();
    let stored =
        if names.is_empty() { "nothing is stored under any name".to_string() } else { format!("stored: {}", names.join(", ")) };
    Err(fail(
        "dangling_default",
        format!(
            "the default names {}, and no key is stored under that name — {stored}; run `krowk workspaces use <name>` \
             to point the default at a stored key, or `krowk login`",
            c.default
        ),
    ))
}

/// Where the token a call carries comes from.
pub fn token_source(env: Env, workspace: &str) -> &'static str {
    if !env("KROWK_TOKEN").is_empty() {
        return TOKEN_SOURCE_ENV;
    }
    match read_lenient().entry(workspace) {
        Some(k) if !k.token.is_empty() => TOKEN_SOURCE_FILE,
        _ => TOKEN_SOURCE_NONE,
    }
}

/// The identity recorded beside the stored key, when a login recorded one.
/// None under KROWK_TOKEN, whose key the file knows nothing about.
pub fn read_identity(env: Env, workspace: &str) -> Option<Identity> {
    if !env("KROWK_TOKEN").is_empty() {
        return None;
    }
    let c = read_lenient();
    let k = c.entry(workspace)?;
    if k.token.is_empty() || k.key_id.is_empty() {
        return None;
    }
    Some(Identity { key_id: k.key_id.clone(), workspace: k.workspace.clone(), workspace_name: String::new() })
}

/// Stores a key under its workspace and makes it the default.
pub fn save_credentials(token: &str, id: &Identity) -> Result<String, Error> {
    change(|c| {
        let name = if id.workspace.is_empty() { DEFAULT_ENTRY.to_string() } else { id.workspace.clone() };
        c.workspaces.insert(
            name.clone(),
            StoredKey {
                token: token.into(),
                key_id: id.key_id.clone(),
                workspace: id.workspace.clone(),
                workspace_name: id.workspace_name.clone(),
            },
        );
        c.default = name;
        Ok(((), true))
    })?;
    Ok(credentials_text())
}

/// Re-files a stored key under the workspace the registry says it belongs to,
/// when it was stored under another name. Reports whether anything changed.
pub fn adopt_identity(token: &str, id: &Identity) -> Result<bool, Error> {
    if token.is_empty() || id.workspace.is_empty() {
        return Ok(false);
    }
    change(|c| {
        let Some(from) = c.names().into_iter().find(|n| c.workspaces[n].token == token) else {
            return Ok((false, false));
        };
        let want = StoredKey {
            token: token.into(),
            key_id: id.key_id.clone(),
            workspace: id.workspace.clone(),
            workspace_name: id.workspace_name.clone(),
        };
        if from == id.workspace && c.workspaces[&from] == want {
            return Ok((false, false));
        }
        c.workspaces.remove(&from);
        c.workspaces.insert(id.workspace.clone(), want);
        if c.default == from {
            c.default = id.workspace.clone();
        }
        Ok((true, true))
    })
}

/// Forgets the key stored for `workspace` — the default's, when empty — and
/// the default with it when it was that one. The name it was stored under
/// and its identity, or none when nothing was stored there. The key itself
/// still works until it is revoked in the dashboard: this machine just no
/// longer holds it.
pub fn forget_credentials(workspace: &str) -> Result<Option<(String, Identity)>, Error> {
    change(|c| {
        let name = if workspace.is_empty() { c.default.clone() } else { workspace.to_string() };
        let Some(k) = c.workspaces.remove(&name) else {
            return Ok((None, false));
        };
        if c.default == name {
            c.default.clear();
        }
        Ok((Some((name, Identity { key_id: k.key_id, workspace: k.workspace, workspace_name: k.workspace_name })), true))
    })
}

/// Every stored key, by name.
pub fn stored_workspaces() -> Vec<WorkspaceKey> {
    let c = read_lenient();
    c.workspaces
        .iter()
        .map(|(name, k)| WorkspaceKey {
            name: name.clone(),
            key_id: k.key_id.clone(),
            workspace: k.workspace.clone(),
            workspace_name: k.workspace_name.clone(),
            default: *name == c.default,
        })
        .collect()
}

/// Points the default at a stored key. The error is a plain sentence, for
/// the caller to wrap.
pub fn set_default_workspace(name: &str) -> Result<String, String> {
    change(|c| {
        if !c.workspaces.contains_key(name) {
            let names = c.names();
            if names.is_empty() {
                return Err(fail("", "no workspace keys are stored — run `krowk login` first"));
            }
            return Err(fail("", format!("no stored key named {name} — stored: {}", names.join(", "))));
        }
        c.default = name.to_string();
        Ok(((), true))
    })
    .map_err(|e| e.fix())?;
    Ok(credentials_text())
}
