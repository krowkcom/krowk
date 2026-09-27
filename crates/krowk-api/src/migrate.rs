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

/// Where an older krowk left files for this home, if anywhere: the old
/// directories that exist, and a staging directory a move cut short left.
pub fn pending(home: &Path, env: Env) -> bool {
    let Some(user) = home.parent() else { return false };
    [old(env, user, "XDG_CONFIG_HOME", ".config"), old(env, user, "XDG_DATA_HOME", ".local/share"), old(env, user, "XDG_CACHE_HOME", ".cache"), staging(home)].iter().any(|d| exists(d))
}

/// `<home>.migrating`, where the move builds the home.
pub fn staging(home: &Path) -> PathBuf {
    home.with_extension("migrating")
}

/// `<home>.migrate.lock`, which two krowks moving at once take turns on.
pub fn lock_path(home: &Path) -> PathBuf {
    home.with_extension("migrate.lock")
}

/// Whether the home holds nothing krowk would know it by: none (it is
/// made, or moved into, now) or one with neither of its two files — made
/// by hand, or by a run that had nothing to move.
pub fn unfilled(home: &Path) -> bool {
    !exists(&home.join(home::CREDENTIALS)) && !exists(&home.join(home::CONFIG))
}

/// What a move could not take, to name in its notice.
type Left = Vec<String>;

/// Moves what an older krowk left into `home`, which does not exist yet or
/// is `unfilled`.
pub fn run(home: &Path, env: Env) -> Result<(), String> {
    let Some(user) = home.parent() else { return Ok(()) };
    let cfg = old(env, user, "XDG_CONFIG_HOME", ".config");
    let data = old(env, user, "XDG_DATA_HOME", ".local/share");
    let cache = old(env, user, "XDG_CACHE_HOME", ".cache");
    let staging = staging(home);
    let io = |what: &Path, e: std::io::Error| format!("moving krowk's files into {}: {}: {e}", home.display(), what.display());
    let lock_file = lock_path(home);
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_file).map_err(|e| io(&lock_file, e))?;
    lock.lock().map_err(|e| io(&lock_file, e))?;
    // Another krowk may have finished while this one waited.
    if exists(home) && !unfilled(home) && !exists(&staging) {
        return Ok(());
    }
    home::make(&staging)?;
    let s = |rel: &str| staging.join(rel);
    let mut left = Left::new();

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
        for e in std::fs::read_dir(data.join(vendor)).into_iter().flatten().flatten() {
            shift(&e.path(), &s(home::ACCOUNTS).join(e.file_name()), true, &mut left)?;
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
                // Its own mode, not the credentials file's.
                if let Ok(m) = std::fs::metadata(&conf) {
                    let _ = std::fs::set_permissions(s(home::CONFIG), m.permissions());
                }
                let _ = std::fs::remove_file(&conf);
            }
            Err(_) => shift(&conf, &s(home::CONFIG), true, &mut left)?,
        }
    }

    let sessions = s(home::SESSIONS);
    let moves: [(PathBuf, PathBuf); 8] = [
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
        shift(from, to, true, &mut left)?;
    }
    let _ = std::fs::remove_file(data.join("import.lock"));
    // The rest of each directory is krowk's too — remembered grants,
    // skills, AGENTS.md, whatever a later krowk kept — and goes to the
    // home's root (the cache's to `cache/`) as it is, never over a name
    // already there: that one stays where it was, named in the notice.
    for (from, to) in [(&cfg, staging.clone()), (&data, staging.clone()), (&cache, s(home::CACHE))] {
        for e in std::fs::read_dir(from).into_iter().flatten().flatten() {
            shift(&e.path(), &to.join(e.file_name()), false, &mut left)?;
        }
    }
    for d in [&cfg, &data, &cache] {
        let _ = std::fs::remove_dir(d);
    }
    if exists(home) {
        // A home that was there, empty or nearly, takes the staged entries
        // one by one, never over its own, and its two files last: they are
        // what marks it filled, so a move cut short here resumes.
        let last = [home::CREDENTIALS, home::CONFIG];
        for e in std::fs::read_dir(&staging).map_err(|e| io(&staging, e))?.flatten().filter(|e| !last.iter().any(|k| e.file_name() == *k)) {
            shift(&e.path(), &home.join(e.file_name()), false, &mut left)?;
        }
        for n in last {
            shift(&staging.join(n), &home.join(n), false, &mut left)?;
        }
        if std::fs::remove_dir(&staging).is_err() {
            left.push(format!("{} (what could not go in beside what was there)", staging.display()));
        }
    } else {
        std::fs::rename(&staging, home).map_err(|e| io(home, e))?;
    }
    drop(lock);
    let _ = std::fs::remove_file(&lock_file);
    match left.is_empty() {
        true => eprintln!("krowk: moved krowk's files to {}", home.display()),
        false => eprintln!("krowk: moved krowk's files to {} — left where they were: {}", home.display(), left.join("; ")),
    }
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

/// Moves `from` to `to` by rename, parents made `0700`. A directory already
/// at `to` takes `from`'s entries one by one. Anything else already there
/// is replaced when `replace` (a known move resumed) and otherwise left, as
/// is an entry on another file system: each is named in `left`, with the
/// `mv` that finishes it.
fn shift(from: &Path, to: &Path, replace: bool, left: &mut Left) -> Result<(), String> {
    let Ok(m) = std::fs::symlink_metadata(from) else { return Ok(()) };
    let io = |e: std::io::Error| format!("moving {} to {}: {e}", from.display(), to.display());
    if let Some(parent) = to.parent() {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(parent).map_err(io)?;
    }
    match std::fs::symlink_metadata(to) {
        Ok(t) if m.is_dir() && t.is_dir() => {
            for e in std::fs::read_dir(from).map_err(io)?.flatten() {
                shift(&e.path(), &to.join(e.file_name()), replace, left)?;
            }
            let _ = std::fs::remove_dir(from);
            return Ok(());
        }
        Ok(_) if !replace => {
            left.push(format!("{} (a file of that name is there already)", from.display()));
            return Ok(());
        }
        Ok(t) if t.is_dir() => std::fs::remove_dir_all(to).map_err(io)?,
        Ok(_) => std::fs::remove_file(to).map_err(io)?,
        Err(_) => {}
    }
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            left.push(format!("{} (another file system: mv {} {})", from.display(), from.display(), to.display()));
            Ok(())
        }
        r => r.map_err(io),
    }
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

    fn old_home(name: &str) -> (PathBuf, impl Fn(&str) -> String) {
        let d = std::env::temp_dir().join(format!("krowk-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".config/krowk")).unwrap();
        std::fs::write(d.join(".config/krowk/credentials.json"), r#"{"default":"ws","workspaces":{"ws":{"token":"the-key"}}}"#).unwrap();
        let h = d.display().to_string();
        (d, move |k: &str| if k == "HOME" { h.clone() } else { String::new() })
    }

    #[test]
    fn a_stray_file_never_replaces_one_the_move_made() {
        let (d, env) = old_home("stray");
        // A credentials.json in the old data directory, where krowk never
        // kept one: it must not replace the merged one.
        std::fs::create_dir_all(d.join(".local/share/krowk")).unwrap();
        std::fs::write(d.join(".local/share/krowk/credentials.json"), "{}").unwrap();
        run(&d.join(".krowk"), &env).unwrap();
        let c: Map<String, Value> = creds::read(&d.join(".krowk/credentials.json")).unwrap();
        assert_eq!(c["workspaces"]["ws"]["token"], "the-key");
        assert!(d.join(".local/share/krowk/credentials.json").exists(), "left where it was, and named");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_home_beside_an_old_layout_is_filled_from_it() {
        let (d, env) = old_home("unfilled");
        let home = d.join(".krowk");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::write(home.join("sessions/tui-history.jsonl"), "mine").unwrap();
        assert!(unfilled(&home) && pending(&home, &env));
        run(&home, &env).unwrap();
        let c: Map<String, Value> = creds::read(&home.join(home::CREDENTIALS)).unwrap();
        assert_eq!(c["workspaces"]["ws"]["token"], "the-key");
        assert_eq!(std::fs::read_to_string(home.join("sessions/tui-history.jsonl")).unwrap(), "mine", "what was there is kept");
        assert!(!unfilled(&home) && !pending(&home, &env), "and nothing is left to move");
        let _ = std::fs::remove_dir_all(&d);
    }
}
