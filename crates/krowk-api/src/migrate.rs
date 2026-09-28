//! The one move from an older krowk's layout into the home, the first time
//! a krowk without a home runs. What a released krowk (up to 0.10) kept was
//! small: `config.json`, the registry key in `credentials.json` and the
//! update check under `~/.config/krowk`, the session index `krowk.db` under
//! `~/.local/share/krowk`, and the models.dev cache under `~/.cache/krowk`
//! (or wherever the XDG variables put them).
//!
//! So the move is small too, and nothing of the person's is ever renamed:
//! the old `config.json` and credentials (the registry key, and a dev
//! build's provider file, which is the same format) are *read* and written
//! as the new home's `config.json` and one merged `credentials.json`, in a
//! staging directory (`.krowk.migrating`) that holds only what krowk writes
//! and is renamed into place in one step. Only then are the old credential
//! files, config.json, update check and cache deleted — taking the keys
//! out of a directory a dotfiles repository may track is the point.
//!
//! A crash before the rename leaves only the staging directory, which the
//! next krowk deletes and builds again. A crash after it leaves old files
//! the home already holds: the next krowk's once-per-home check
//! (`note_old`) deletes each old credential file whose every entry the new
//! one holds. Everything else in the old directories — krowk.db, a dev
//! build's accounts and sessions — stays where it is, and that check names
//! it once, with the command that deals with it. Two krowks starting at
//! once take turns on a lock file beside the home (`.krowk.migrate.lock`),
//! never removed: a stale one is harmless under `flock`.

use crate::creds::{self, Env};
use crate::home;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// `$XDG_<var>/krowk` when absolute, else `<user>/<fallback>/krowk`.
fn old(env: Env, user: &Path, var: &str, fallback: &str) -> PathBuf {
    match PathBuf::from(env(var)) {
        x if x.is_absolute() => x.join("krowk"),
        _ => user.join(fallback).join("krowk"),
    }
}

/// The old config, data and cache directories.
fn olds(home: &Path, env: Env) -> Option<[PathBuf; 3]> {
    let user = home.parent()?;
    Some([old(env, user, "XDG_CONFIG_HOME", ".config"), old(env, user, "XDG_DATA_HOME", ".local/share"), old(env, user, "XDG_CACHE_HOME", ".cache")])
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// The old credential files: the registry key, and a dev build's provider
/// file.
fn old_creds(cfg: &Path) -> [PathBuf; 2] {
    [cfg.join(home::CREDENTIALS), cfg.join("providers").join(home::CREDENTIALS)]
}

/// `<home>.migrating`, where the move builds the home.
pub fn staging(home: &Path) -> PathBuf {
    home.with_extension("migrating")
}

/// `<home>.migrate.lock`, which two krowks moving at once take turns on.
pub fn lock_path(home: &Path) -> PathBuf {
    home.with_extension("migrate.lock")
}

// Test-only: a crash after the rename, before the old files are deleted.
#[cfg(test)]
thread_local! {
    static CRASH_AFTER_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A directory's entries made durable (unix: `fsync` of the directory).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Moves an older krowk's config and keys into `home`, which does not
/// exist. Nothing to move is nothing done.
pub fn run(home: &Path, env: Env) -> Result<(), String> {
    let Some([cfg, _, cache]) = olds(home, env) else { return Ok(()) };
    let (conf, creds_files, staging) = (cfg.join(home::CONFIG), old_creds(&cfg), staging(home));
    if !exists(&conf) && !creds_files.iter().any(|f| exists(f)) && !exists(&staging) {
        return Ok(());
    }
    let io = |what: &Path, e: std::io::Error| format!("moving krowk's config and keys into {}: {}: {e}", home.display(), what.display());
    let lock_file = lock_path(home);
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_file).map_err(|e| io(&lock_file, e))?;
    lock.lock().map_err(|e| io(&lock_file, e))?;
    if exists(home) {
        // Another krowk finished while this one waited.
        return Ok(());
    }
    // A build cut short before its rename: only what krowk wrote is there.
    if exists(&staging) {
        std::fs::remove_dir_all(&staging).map_err(|e| io(&staging, e))?;
    }
    // Read everything first: an old key file krowk cannot read stops the
    // move before anything is written, and keeps the file.
    let aside = |e: String| format!("{e} — fix it or move it aside, and krowk moves its config and keys into {}", home.display());
    let [reg, prov] = &creds_files;
    let mut merged: Map<String, Value> = creds::read(prov).map_err(aside)?;
    merged.extend(creds::registry_section(reg).map_err(aside)?);
    let config = match std::fs::read(&conf) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(&conf, e)),
    };

    home::make(&staging)?;
    let built = (|| {
        if !merged.is_empty() {
            creds::write(&staging.join(home::CREDENTIALS), &merged)?;
        }
        if let Some(b) = &config {
            // As it was, byte for byte, with its own mode.
            let to = staging.join(home::CONFIG);
            let f = std::fs::File::create(&to).and_then(|mut f| std::io::Write::write_all(&mut f, b).and_then(|()| f.sync_all()));
            f.map_err(|e| io(&to, e))?;
            if let Ok(m) = std::fs::metadata(&conf) {
                let _ = std::fs::set_permissions(&to, m.permissions());
            }
        }
        sync_dir(&staging);
        std::fs::rename(&staging, home).map_err(|e| io(home, e))?;
        sync_dir(home.parent().unwrap_or(home));
        Ok::<(), String>(())
    })();
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    eprintln!("krowk: moved krowk's config and keys to {}", home.display());
    #[cfg(test)]
    if CRASH_AFTER_RENAME.with(|c| c.get()) {
        return Ok(());
    }
    // In place: the old copies go — the keys first.
    for f in &creds_files {
        let _ = std::fs::remove_file(f);
    }
    for f in [cfg.join("providers").join("credentials.lock"), conf, cfg.join("update-check.json")] {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_dir(cfg.join("providers"));
    let _ = std::fs::remove_dir(&cfg);
    let _ = std::fs::remove_dir_all(&cache);
    Ok(())
}

/// Whether the home's credentials file holds every entry of `old`: each
/// workspace, login and stored key it names (a value may have changed
/// since — a token refreshed — the entry is what counts), and its default.
fn held(old: &Map<String, Value>, new: &Map<String, Value>) -> bool {
    old.iter().all(|(k, v)| match (v, new.get(k)) {
        (Value::Object(o), Some(Value::Object(n))) => o.keys().all(|e| n.contains_key(e)),
        (Value::Object(o), _) => o.is_empty(),
        (_, n) => k == "version" || n.is_some(),
    })
}

/// For a home just made or found: once per home (a marker file in it), the
/// old copies of what it already holds are deleted — the end of a move a
/// crash cut short — and one line names what is left in the old
/// directories, with what to do about it.
pub fn note_old(home: &Path, env: Env) {
    let marker = home.join(home::CHECKED);
    if exists(&marker) {
        return;
    }
    let Some([cfg, data, _]) = olds(home, env) else { return };
    let new: Map<String, Value> = creds::read(&home.join(home::CREDENTIALS)).unwrap_or_default();
    let [reg, prov] = old_creds(&cfg);
    for (f, old) in [(&reg, creds::registry_section(&reg)), (&prov, creds::read(&prov))] {
        if exists(f) && old.is_ok_and(|o| held(&o, &new)) {
            let _ = std::fs::remove_file(f);
        }
    }
    let conf = cfg.join(home::CONFIG);
    if exists(&conf) && std::fs::read(&conf).ok() == std::fs::read(home.join(home::CONFIG)).ok() {
        let _ = std::fs::remove_file(&conf);
    }
    let _ = std::fs::remove_file(cfg.join("providers").join("credentials.lock"));
    let _ = std::fs::remove_dir(cfg.join("providers"));

    let h = home.display();
    let mut lines: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    for dir in [&cfg, &data] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let (p, name) = (e.path(), e.file_name().to_string_lossy().into_owned());
            match name.as_str() {
                "krowk.db-wal" | "krowk.db-shm" | "import.lock" => {}
                "krowk.db" => lines.push(format!("the session index {} is not used — run `krowk sessions rebuild`, then delete it", p.display())),
                "config.json" | "credentials.json" | "providers" => lines.push(format!("{} is not used — krowk reads {h}; copy what you need across, then delete it", p.display())),
                "claude" | "codex" => {
                    for a in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                        let dir = a.file_name().to_string_lossy().into_owned();
                        let (key, method, vendor) = if name == "claude" { ("configDir", "anthropic", "claude-") } else { ("codexHome", "openai", "codex-") };
                        lines.push(format!(
                            "account {}: `mkdir -p {h}/accounts && mv {} {h}/accounts/{dir}` and set its {key} in {h}/config.json to {h}/accounts/{dir} — or sign in again, `krowk connect {method} --method subscription --name {}`",
                            a.path().display(),
                            a.path().display(),
                            dir.strip_prefix(vendor).unwrap_or(&dir)
                        ));
                    }
                }
                _ => other.push(format!("`mv {} {h}/{name}`", p.display())),
            }
        }
    }
    if !other.is_empty() {
        lines.push(format!("to keep the rest, {}", other.join(", ")));
    }
    if !lines.is_empty() {
        eprintln!("krowk: old krowk files krowk no longer reads:\n  {}", lines.join("\n  "));
    }
    if lines.is_empty() {
        for d in [&cfg, &data] {
            let _ = std::fs::remove_dir(d);
        }
    }
    let _ = std::fs::write(marker, "");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &str = "krowk_sk_OLD-SENTINEL";

    /// What a released krowk left, and a dev build's provider file.
    fn old_layout(name: &str) -> (PathBuf, impl Fn(&str) -> String + Clone + use<>) {
        let d = std::env::temp_dir().join(format!("krowk-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (cfg, data, cache) = (d.join(".config/krowk"), d.join(".local/share/krowk"), d.join(".cache/krowk"));
        for dir in [cfg.join("providers"), data.clone(), cache.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let files = [
            (cfg.join("credentials.json"), json!({"default": "ws", "workspaces": {"ws": {"token": KEY}}}).to_string()),
            (cfg.join("providers/credentials.json"), r#"{"version":1,"keys":{"openai":{"env":"K"}}}"#.to_string()),
            (cfg.join("config.json"), r#"{"workspace":"ws"}"#.to_string()),
            (cfg.join("update-check.json"), "{}".to_string()),
            (data.join("krowk.db"), "db".to_string()),
            (cache.join("models.json"), "{}".to_string()),
        ];
        for (f, body) in files {
            std::fs::write(f, body).unwrap();
        }
        let h = d.display().to_string();
        (d, move |k: &str| if k == "HOME" { h.clone() } else { String::new() })
    }

    fn assert_moved(d: &Path) {
        let home = d.join(".krowk");
        let c: Map<String, Value> = creds::read(&home.join(home::CREDENTIALS)).unwrap();
        assert_eq!((c["workspaces"]["ws"]["token"].as_str(), c["keys"]["openai"]["env"].as_str()), (Some(KEY), Some("K")));
        assert_eq!(std::fs::read_to_string(home.join(home::CONFIG)).unwrap(), r#"{"workspace":"ws"}"#);
        for gone in [".config/krowk/credentials.json", ".config/krowk/providers", ".config/krowk/config.json", ".config/krowk/update-check.json", ".cache/krowk", ".krowk.migrating"] {
            assert!(!d.join(gone).exists(), "{gone} is left");
        }
        assert!(d.join(".local/share/krowk/krowk.db").exists(), "the index is not moved");
    }

    #[test]
    fn the_config_and_every_key_move_and_the_old_secrets_are_deleted() {
        let (d, env) = old_layout("fresh");
        run(&d.join(".krowk"), &env).unwrap();
        assert_moved(&d);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join(".krowk/credentials.json")).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(d.join(".krowk")).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_crash_after_the_rename_is_finished_by_the_next_start() {
        let (d, env) = old_layout("crash");
        let home = d.join(".krowk");
        CRASH_AFTER_RENAME.with(|c| c.set(true));
        run(&home, &env).unwrap();
        CRASH_AFTER_RENAME.with(|c| c.set(false));
        assert!(d.join(".config/krowk/credentials.json").exists(), "the crash left the old key");
        note_old(&home, &env);
        assert!(!d.join(".config/krowk/credentials.json").exists() && !d.join(".config/krowk/providers").exists(), "deleted on the next start");
        assert!(!d.join(".config/krowk/config.json").exists(), "and the old config, identical to the new");
        assert!(home.join(home::CHECKED).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_staging_directory_left_by_a_crash_is_deleted_and_the_move_made_again() {
        let (d, env) = old_layout("staging");
        let staging = d.join(".krowk.migrating");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("credentials.json"), "{\"half").unwrap();
        std::fs::write(staging.join(".credentials-1234.json"), "{").unwrap();
        run(&d.join(".krowk"), &env).unwrap();
        assert_moved(&d);
        assert!(!d.join(".krowk/.credentials-1234.json").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_home_that_exists_takes_nothing_and_an_old_key_goes_only_when_it_holds_it_all() {
        let (d, env) = old_layout("exists");
        let home = d.join(".krowk");
        std::fs::create_dir_all(&home).unwrap();
        // The home holds the registry's workspace but not the provider key.
        std::fs::write(home.join(home::CREDENTIALS), json!({"default": "ws", "workspaces": {"ws": {"token": "rotated"}}}).to_string()).unwrap();
        note_old(&home, &env);
        assert!(!d.join(".config/krowk/credentials.json").exists(), "every entry is in the home: deleted");
        assert!(d.join(".config/krowk/providers/credentials.json").exists(), "a key the home lacks: kept");
        let c: Map<String, Value> = creds::read(&home.join(home::CREDENTIALS)).unwrap();
        assert!(c.get("keys").is_none(), "nothing merged in");
        assert!(d.join(".config/krowk/config.json").exists(), "a config that differs is kept");
        // Once per home: the marker ends it.
        std::fs::write(home.join(home::CREDENTIALS), json!({"keys": {"openai": {}}}).to_string()).unwrap();
        note_old(&home, &env);
        assert!(d.join(".config/krowk/providers/credentials.json").exists(), "checked once");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_malformed_old_key_file_refuses_the_move_and_is_kept() {
        let (d, env) = old_layout("malformed");
        let reg = d.join(".config/krowk/credentials.json");
        std::fs::write(&reg, format!("{{\"token\": \"{KEY}\", oops}}")).unwrap();
        let e = run(&d.join(".krowk"), &env).unwrap_err();
        assert!(e.contains("is not valid (line 1, column") && !e.contains(KEY), "{e}");
        assert!(reg.exists() && !d.join(".krowk").exists() && !d.join(".krowk.migrating").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    #[cfg(unix)]
    fn two_krowks_starting_at_once_move_everything_once() {
        let (d, env) = old_layout("race");
        let home = d.join(".krowk");
        let runs: Vec<_> = (0..2)
            .map(|_| {
                let (home, env) = (home.clone(), env.clone());
                std::thread::spawn(move || run(&home, &env))
            })
            .collect();
        for r in runs {
            r.join().unwrap().unwrap();
        }
        assert_moved(&d);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_single_key_file_of_older_logins_is_normalised() {
        let (d, env) = old_layout("legacy");
        std::fs::write(d.join(".config/krowk/credentials.json"), r#"{"token":"legacy-key","workspace":"ws"}"#).unwrap();
        run(&d.join(".krowk"), &env).unwrap();
        let c: Map<String, Value> = creds::read(&d.join(".krowk/credentials.json")).unwrap();
        assert_eq!((c["workspaces"]["ws"]["token"].as_str(), c["default"].as_str()), (Some("legacy-key"), Some("ws")));
        assert!(c.get("token").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
