//! The credentials file: one key per workspace, and which one is the default.
//! Written 0600 by rename, never in place, and never over a file that could
//! not be read — that would replace every key stored in it with one.

use crate::error::{fail, Error};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The process environment, as the CLI reads it. Passed rather than read so
/// a test can hand in its own.
pub type Env<'a> = &'a dyn Fn(&str) -> String;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    default: String,
    #[serde(default)]
    workspaces: BTreeMap<String, StoredKey>,
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

/// $XDG_CONFIG_HOME/krowk/credentials.json, else ~/.config/krowk/.
pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

/// krowk's config directory: $XDG_CONFIG_HOME/krowk, else ~/.config/krowk —
/// or `.krowk` beside the caller when there is no home at all.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("krowk");
    }
    match home_dir() {
        Some(home) => home.join(".config").join("krowk"),
        None => PathBuf::from(".krowk"),
    }
}

/// Go's os.UserHomeDir on unix: $HOME, and an error when it is empty.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn unreadable_store(path: &Path, cause: &str) -> Error {
    fail(
        "cli_error",
        format!(
            "cannot read the existing credentials at {}: {cause} — refusing to write, because writing over a store \
             that cannot be read would replace every key stored in it with this one",
            path_string(path)
        ),
    )
}

fn read_lenient() -> Credentials {
    read_strict().unwrap_or_default()
}

fn read_strict() -> Result<Credentials, Error> {
    let path = credentials_path();
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Credentials::default()),
        Err(e) => return Err(unreadable_store(&path, &crate::spec::go_os_error("open", &path_string(&path), &e))),
    };
    let raw: serde_json::Map<String, Value> =
        serde_json::from_slice(&data).map_err(|e| unreadable_store(&path, &e.to_string()))?;
    if !raw.contains_key("workspaces") {
        if raw.contains_key("token") {
            // The single-key file every login wrote before workspaces existed.
            let legacy: StoredKey = serde_json::from_slice(&data).map_err(|e| unreadable_store(&path, &e.to_string()))?;
            if legacy.token.is_empty() {
                return Ok(Credentials::default());
            }
            let name = if legacy.workspace.is_empty() { DEFAULT_ENTRY.to_string() } else { legacy.workspace.clone() };
            return Ok(Credentials { default: name.clone(), workspaces: BTreeMap::from([(name, legacy)]) });
        }
        return Ok(Credentials::default());
    }
    serde_json::from_slice(&data).map_err(|e| unreadable_store(&path, &e.to_string()))
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
    let mut c = read_strict()?;
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
    write(&c)
}

/// Re-files a stored key under the workspace the registry says it belongs to,
/// when it was stored under another name. Reports whether anything changed.
pub fn adopt_identity(token: &str, id: &Identity) -> Result<bool, Error> {
    if token.is_empty() || id.workspace.is_empty() {
        return Ok(false);
    }
    let mut c = read_strict()?;
    let Some(from) = c.names().into_iter().find(|n| c.workspaces[n].token == token) else {
        return Ok(false);
    };
    let want = StoredKey {
        token: token.into(),
        key_id: id.key_id.clone(),
        workspace: id.workspace.clone(),
        workspace_name: id.workspace_name.clone(),
    };
    if from == id.workspace && c.workspaces[&from] == want {
        return Ok(false);
    }
    c.workspaces.remove(&from);
    c.workspaces.insert(id.workspace.clone(), want);
    if c.default == from {
        c.default = id.workspace.clone();
    }
    write(&c)?;
    Ok(true)
}

/// Forgets the key stored for `workspace` — the default's, when empty — and
/// the default with it when it was that one. The name it was stored under
/// and its identity, or none when nothing was stored there. The key itself
/// still works until it is revoked in the dashboard: this machine just no
/// longer holds it.
pub fn forget_credentials(workspace: &str) -> Result<Option<(String, Identity)>, Error> {
    let mut c = read_strict()?;
    let name = if workspace.is_empty() { c.default.clone() } else { workspace.to_string() };
    let Some(k) = c.workspaces.remove(&name) else {
        return Ok(None);
    };
    if c.default == name {
        c.default.clear();
    }
    write(&c)?;
    Ok(Some((name, Identity { key_id: k.key_id, workspace: k.workspace, workspace_name: k.workspace_name })))
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
    let mut c = read_strict().map_err(|e| e.fix())?;
    if !c.workspaces.contains_key(name) {
        let names = c.names();
        if names.is_empty() {
            return Err("no workspace keys are stored — run `krowk login` first".into());
        }
        return Err(format!("no stored key named {name} — stored: {}", names.join(", ")));
    }
    c.default = name.to_string();
    write(&c).map_err(|e| e.fix())
}

fn write(c: &Credentials) -> Result<String, Error> {
    let path = credentials_path();
    let dir = path.parent().unwrap_or(Path::new("."));
    let io = |e: std::io::Error| fail("cli_error", e.to_string());
    create_private_dir(dir).map_err(io)?;
    let data = serde_json::to_string_pretty(c).expect("credentials serialize") + "\n";
    let tmp = crate::tempfile::create(dir, "credentials-", ".json", 0o600).map_err(io)?;
    let result = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).open(&tmp)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(io)?;
    Ok(path_string(&path))
}

/// MkdirAll with 0700 for what it creates, as the Go build did.
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}
