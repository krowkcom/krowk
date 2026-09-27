//! Which workspace a command uses, layered: the global config, the repo's
//! .krowk/config.json, KROWK_WORKSPACE, then --workspace. Each layer that
//! names one is somebody's deliberate ask, so a malformed file is an error
//! rather than a layer quietly skipped.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SOURCE_GLOBAL: &str = "global config";
pub const SOURCE_REPO: &str = "repo config";
pub const SOURCE_ENV: &str = "KROWK_WORKSPACE";
pub const SOURCE_FLAG: &str = "--workspace";

/// The one key there is. A list rather than a constant so the refusal for an
/// unknown key names what is valid.
const KEYS: &[&str] = &["workspace"];

#[derive(Debug, Default)]
pub struct Config {
    pub workspace: String,
    /// Which layer set each key.
    pub sources: BTreeMap<String, String>,
}

fn blank(v: &str) -> bool {
    v.trim().is_empty()
}

/// The effective configuration for `dir` (the working directory when empty).
pub fn load(dir: &str, env: &dyn Fn(&str) -> String, flag_workspace: &str) -> Result<Config, String> {
    let mut c = Config::default();
    let apply = |c: &mut Config, v: String, source: &str| {
        if !blank(&v) {
            c.sources.insert("workspace".into(), source.into());
            c.workspace = v;
        }
    };
    if let Ok(p) = global_path()
        && let Some(v) = read_file(&p)?
    {
        apply(&mut c, v, SOURCE_GLOBAL);
    }
    if let Some(v) = repo_path(dir).map(|p| read_file(&p)).transpose()?.flatten() {
        apply(&mut c, v, SOURCE_REPO);
    }
    apply(&mut c, env("KROWK_WORKSPACE"), SOURCE_ENV);
    apply(&mut c, flag_workspace.to_string(), SOURCE_FLAG);
    Ok(c)
}

/// The workspace a file names, if any.
fn read_file(path: &Path) -> Result<Option<String>, String> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let raw: Map<String, Value> =
        serde_json::from_slice(&data).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    match raw.get("workspace") {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{}: \"workspace\" must be a string", path.display())),
    }
}

/// `config.json` in krowk's home (`krowk_api::home`), or why there is none.
pub fn global_path() -> Result<PathBuf, krowk_api::Error> {
    Ok(krowk_api::home::get()?.join(krowk_api::home::CONFIG))
}

/// The same, in words, for a message: empty without a home.
pub fn global_text() -> String {
    global_path().map(|p| p.display().to_string()).unwrap_or_default()
}

/// <git-root>/.krowk/config.json, when `dir` is inside a checkout. The root is
/// the nearest ancestor holding a .git, directory or file.
pub fn repo_path(dir: &str) -> Option<PathBuf> {
    let start = if dir.is_empty() { std::env::current_dir().ok()? } else { std::path::absolute(dir).ok()? };
    start.ancestors().find(|d| d.join(".git").symlink_metadata().is_ok()).map(|root| root.join(".krowk").join("config.json"))
}

pub fn known_keys() -> Vec<&'static str> {
    KEYS.to_vec()
}

/// A plain refusal for a key nobody has heard of.
pub fn known(name: &str) -> Result<(), String> {
    if KEYS.contains(&name) {
        return Ok(());
    }
    Err(format!("unknown config key {name:?}, valid keys: {}", KEYS.join(", ")))
}

pub fn set(path: &Path, key: &str, value: &str) -> Result<(), String> {
    known(key)?;
    rewrite(path, |raw| {
        raw.insert(key.into(), Value::String(value.into()));
    })
}

pub fn unset(path: &Path, key: &str) -> Result<(), String> {
    known(key)?;
    if !path.exists() {
        return Ok(());
    }
    rewrite(path, |raw| {
        raw.remove(key);
    })
}

/// Edits the file as a JSON object, keeping every key the edit does not
/// touch: how `krowk providers` writes the harness's `instances`.
#[cfg(feature = "harness")]
pub fn edit(path: &Path, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<(), String> {
    rewrite(path, edit)
}

/// Read, edit, write back by rename, keeping every key this build does not
/// know about as it was.
fn rewrite(path: &Path, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<(), String> {
    let mut raw = match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    edit(&mut raw);
    let data = serde_json::to_string_pretty(&raw).expect("config serializes") + "\n";
    write_atomic(path, data.as_bytes()).map_err(|e| e.to_string())
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".config-{}.json", std::process::id()));
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
