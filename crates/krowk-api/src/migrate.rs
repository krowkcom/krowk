//! The one move from an older krowk's layout — `~/.config/krowk`,
//! `~/.local/share/krowk`, `~/.cache/krowk`, or where the XDG variables put
//! them — into the home, the first time a krowk without a home runs. After
//! it the old places are never read again: the home exists, and this runs
//! only when it does not.
//!
//! Crash-safe by construction: the home is built in a sibling staging
//! directory (`.krowk.migrating`) and renamed into place last, so a home
//! that exists is a finished one. Every step is a rename of one entry, or a
//! file written in full and synced before its source is removed, so an
//! interrupted move leaves each entry in the old place or the new, never
//! lost; the next run finds the staging directory and finishes the same
//! steps, each a no-op where its source is gone. Two krowks starting at once
//! serialise on a lock on the staging directory itself.

use crate::creds::{self, Env};
use crate::home;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// `$XDG_<var>/krowk` when absolute, else `<user>/<fallback>/krowk`: where
/// an older krowk kept its files.
fn old(env: Env, user: &Path, var: &str, fallback: &str) -> PathBuf {
    match PathBuf::from(env(var)) {
        x if x.is_absolute() => x.join("krowk"),
        _ => user.join(fallback).join("krowk"),
    }
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Moves what an older krowk left into `home`, which does not exist yet.
pub fn run(home: &Path, env: Env) -> Result<(), String> {
    let Some(user) = home.parent() else { return Ok(()) };
    let cfg = old(env, user, "XDG_CONFIG_HOME", ".config");
    let data = old(env, user, "XDG_DATA_HOME", ".local/share");
    let cache = old(env, user, "XDG_CACHE_HOME", ".cache");
    let staging = home.with_extension("migrating");
    if ![&cfg, &data, &cache, &staging].iter().any(|d| exists(d)) {
        return Ok(());
    }
    home::make(&staging)?;
    let io = |what: &Path, e: std::io::Error| format!("moving krowk's files into {}: {}: {e}", home.display(), what.display());
    #[cfg(unix)]
    let _lock = {
        let dir = std::fs::File::open(&staging).map_err(|e| io(&staging, e))?;
        dir.lock().map_err(|e| io(&staging, e))?;
        dir
    };
    if exists(home) {
        // Another krowk finished while this one waited.
        let _ = std::fs::remove_dir(&staging);
        return Ok(());
    }
    let s = |rel: &str| staging.join(rel);

    // Both credential files, merged into the one: written in full before
    // either source goes.
    let (reg, prov) = (cfg.join("credentials.json"), cfg.join("providers").join("credentials.json"));
    if exists(&reg) || exists(&prov) {
        let into = s(home::CREDENTIALS);
        let aside = |e: String| format!("{e} — fix it or move it aside, and krowk moves the rest of its files into {}", home.display());
        let mut merged: Map<String, Value> = creds::read(&into).map_err(aside)?;
        merged.extend(creds::read::<Map<String, Value>>(&prov).map_err(aside)?);
        merged.extend(creds::registry_section(&reg).map_err(aside)?);
        creds::write(&into, &merged)?;
        for f in [&reg, &prov, &cfg.join("providers").join("credentials.lock")] {
            let _ = std::fs::remove_file(f);
        }
        let _ = std::fs::remove_dir(cfg.join("providers"));
    }

    // A named account's home, as `accounts/<name>`.
    for vendor in ["claude", "codex"] {
        if let Ok(rd) = std::fs::read_dir(data.join(vendor)) {
            for e in rd.flatten() {
                shift(&e.path(), &s(home::ACCOUNTS).join(e.file_name())).map_err(|e| io(&data, e))?;
            }
        }
        let _ = std::fs::remove_dir(data.join(vendor));
    }

    // config.json, with each account's path pointing at where it went. One
    // krowk cannot read is moved as it is.
    let conf = cfg.join(home::CONFIG);
    if exists(&conf) {
        match creds::read::<Map<String, Value>>(&conf) {
            Ok(mut v) => {
                repoint(&mut v, &data, home, &staging);
                creds::write(&s(home::CONFIG), &v)?;
                let _ = std::fs::remove_file(&conf);
            }
            Err(_) => shift(&conf, &s(home::CONFIG)).map_err(|e| io(&conf, e))?,
        }
    }

    let sessions = s(home::SESSIONS);
    let moves: [(PathBuf, PathBuf); 10] = [
        (cfg.join("trusted.json"), s("trusted.json")),
        (cfg.join("agents"), s("agents")),
        (cfg.join("update-check.json"), s(home::CACHE).join("update-check.json")),
        (data.join("sessions"), sessions.clone()),
        (data.join("krowk.db"), sessions.join("krowk.db")),
        (data.join("krowk.db-wal"), sessions.join("krowk.db-wal")),
        (data.join("krowk.db-shm"), sessions.join("krowk.db-shm")),
        (data.join("tui-history.jsonl"), sessions.join("tui-history.jsonl")),
        (data.join(home::READINESS), s(home::READINESS)),
        (data.join(home::LEDGER), s(home::LEDGER)),
    ];
    for (from, to) in &moves {
        shift(from, to).map_err(|e| io(from, e))?;
    }
    for e in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
        shift(&e.path(), &s(home::CACHE).join(e.file_name())).map_err(|e| io(&cache, e))?;
    }
    for d in [&cfg, &data, &cache] {
        let _ = std::fs::remove_dir(d);
    }
    std::fs::rename(&staging, home).map_err(|e| io(home, e))?;
    eprintln!("krowk: moved krowk's files to {}", home.display());
    Ok(())
}

/// `configDir` and `codexHome` that named an account under the old data
/// directory name it where it went — when it went.
fn repoint(config: &mut Map<String, Value>, data: &Path, home: &Path, staging: &Path) {
    let Some(Value::Object(instances)) = config.get_mut("instances") else { return };
    for inst in instances.values_mut().filter_map(Value::as_object_mut) {
        for key in ["configDir", "codexHome"] {
            let Some(Value::String(p)) = inst.get_mut(key) else { continue };
            let name = ["claude", "codex"].iter().find_map(|v| Path::new(p.as_str()).strip_prefix(data.join(v)).ok().map(Path::to_path_buf));
            if let Some(name) = name.filter(|n| n.components().count() == 1)
                && !exists(Path::new(p.as_str()))
                && exists(&staging.join(home::ACCOUNTS).join(&name))
            {
                *p = home.join(home::ACCOUNTS).join(name).display().to_string();
            }
        }
    }
}

/// Moves `from` to `to`, parents made `0700`. A directory already at `to`
/// takes `from`'s entries one by one; anything else there is what an
/// interrupted copy left, and `from`, still whole, replaces it. Across file
/// systems the entry is copied beside `to`, renamed into place, and only
/// then removed.
fn shift(from: &Path, to: &Path) -> std::io::Result<()> {
    let Ok(m) = std::fs::symlink_metadata(from) else { return Ok(()) };
    if let Some(parent) = to.parent() {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(parent)?;
    }
    match std::fs::symlink_metadata(to) {
        Ok(t) if m.is_dir() && t.is_dir() => {
            for e in std::fs::read_dir(from)?.flatten() {
                shift(&e.path(), &to.join(e.file_name()))?;
            }
            return std::fs::remove_dir(from);
        }
        Ok(t) if t.is_dir() => std::fs::remove_dir_all(to)?,
        Ok(_) => std::fs::remove_file(to)?,
        Err(_) => {}
    }
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            let mut part = to.as_os_str().to_owned();
            part.push(".part");
            let part = PathBuf::from(part);
            let _ = std::fs::remove_dir_all(&part).or_else(|_| std::fs::remove_file(&part));
            copy(from, &part)?;
            std::fs::rename(&part, to)?;
            match m.is_dir() {
                true => std::fs::remove_dir_all(from),
                false => std::fs::remove_file(from),
            }
        }
        r => r,
    }
}

/// A copy that keeps permissions and symlinks as they are.
fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    let m = std::fs::symlink_metadata(from)?;
    if m.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(std::fs::read_link(from)?, to);
    }
    if !m.is_dir() {
        return std::fs::copy(from, to).map(drop);
    }
    std::fs::create_dir(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        copy(&e.path(), &to.join(e.file_name()))?;
    }
    std::fs::set_permissions(to, m.permissions())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn two_krowks_starting_at_once_move_everything_once() {
        let d = std::env::temp_dir().join(format!("krowk-migrate-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let cfg = d.join(".config/krowk");
        std::fs::create_dir_all(cfg.join("providers")).unwrap();
        std::fs::write(cfg.join("config.json"), r#"{"workspace":"ws"}"#).unwrap();
        std::fs::write(cfg.join("credentials.json"), r#"{"token":"legacy-key","workspace":"ws"}"#).unwrap();
        std::fs::write(cfg.join("providers/credentials.json"), r#"{"version":1,"keys":{"openai":{"env":"K"}}}"#).unwrap();
        let home = d.join(".krowk");
        let h = d.display().to_string();
        let runs: Vec<_> = (0..2)
            .map(|_| {
                let (home, h) = (home.clone(), h.clone());
                std::thread::spawn(move || {
                    let env = move |k: &str| if k == "HOME" { h.clone() } else { String::new() };
                    run(&home, &env)
                })
            })
            .collect();
        for r in runs {
            r.join().unwrap().unwrap();
        }
        let c: Map<String, Value> = creds::read(&home.join(home::CREDENTIALS)).unwrap();
        // The single-key file every login wrote before workspaces is normalised.
        assert_eq!(c["workspaces"]["ws"]["token"], "legacy-key");
        assert_eq!(c["default"], "ws");
        assert!(c.get("token").is_none());
        assert_eq!(c["keys"]["openai"]["env"], "K");
        assert!(home.join(home::CONFIG).is_file() && !cfg.exists() && !home.with_extension("migrating").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
