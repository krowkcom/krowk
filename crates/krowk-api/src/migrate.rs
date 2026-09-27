//! The one move from an older krowk's layout — `~/.config/krowk`,
//! `~/.local/share/krowk`, or where the XDG variables put them — into the
//! home, the first time a krowk without a home runs. It is all or nothing:
//! the home is built whole in a sibling staging directory
//! (`.krowk.migrating`) and renamed into place in one step, so a home that
//! exists is a finished one, and one that exists is never merged into.
//!
//! Every entry goes in by one rename, and each rename is written to a
//! journal in the staging directory (and synced) before it is made. When
//! any entry cannot be moved — another file system, two entries with one
//! name, a permission — every rename made so far is undone from the
//! journal, the staging directory goes, and the move is refused with what
//! to do by hand. A move cut short by a crash is undone the same way when
//! the next krowk finds the staging directory, and then made again. The two
//! credential files and config.json are read, not moved: their merged and
//! repointed copies are written into the staging directory, and the old
//! ones removed only once the home is in place. The cache is not moved; it
//! is rebuilt, and the old one removed after. Two krowks starting at once
//! take turns on a lock file beside the home (`.krowk.migrate.lock`), which
//! is never removed: a stale one is harmless under `flock`.

use crate::creds::{self, Env};
use crate::home;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The journal of renames, in the staging directory and then the home:
/// each a source and a destination, each ended by a NUL, which no path
/// holds.
const JOURNAL: &str = ".moves";

fn bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    return std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()).to_vec();
    #[cfg(not(unix))]
    return p.to_string_lossy().into_owned().into_bytes();
}

fn path(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    return PathBuf::from(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b));
    #[cfg(not(unix))]
    return PathBuf::from(String::from_utf8_lossy(b).into_owned());
}

/// `$XDG_<var>/krowk` when absolute, else `<user>/<fallback>/krowk`: where
/// an older krowk kept its files, and the variable that put it there.
fn old(env: Env, user: &Path, var: &'static str, fallback: &str) -> (PathBuf, Option<&'static str>) {
    match PathBuf::from(env(var)) {
        x if x.is_absolute() => (x.join("krowk"), Some(var)),
        _ => (user.join(fallback).join("krowk"), None),
    }
}

/// The old config, data and cache directories.
fn olds(home: &Path, env: Env) -> Option<[(PathBuf, Option<&'static str>); 3]> {
    let user = home.parent()?;
    Some([old(env, user, "XDG_CONFIG_HOME", ".config"), old(env, user, "XDG_DATA_HOME", ".local/share"), old(env, user, "XDG_CACHE_HOME", ".cache")])
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// `<home>.migrating`, where the move builds the home.
pub fn staging(home: &Path) -> PathBuf {
    home.with_extension("migrating")
}

/// `<home>.migrate.lock`, which two krowks moving at once take turns on.
pub fn lock_path(home: &Path) -> PathBuf {
    home.with_extension("migrate.lock")
}

/// For a home that exists: once per home, one line naming the old
/// directories when any is still there, which krowk no longer reads.
pub fn note_old(home: &Path, env: Env) {
    let marker = home.join(home::CHECKED);
    if exists(&marker) {
        return;
    }
    let left: Vec<String> = olds(home, env).into_iter().flatten().map(|(d, _)| d).filter(|d| exists(d)).map(|d| d.display().to_string()).collect();
    if !left.is_empty() {
        eprintln!("krowk: old krowk files at {} are not used — krowk keeps everything in {} now; move or delete them", left.join(", "), home.display());
    }
    let _ = std::fs::write(marker, "");
}

// Test-only fault injection: a crash (the process gone, nothing undone)
// or a failure (the move refused, and undone) after a numbered step.
#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Option<(usize, bool)>> = const { std::cell::Cell::new(None) };
    static STEP: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether a test asked for a fault here: `Some(true)` a crash.
fn fault() -> Option<bool> {
    #[cfg(test)]
    {
        let n = STEP.with(|s| s.replace(s.get() + 1)) + 1;
        if let Some((at, crash)) = FAULT.with(|f| f.get())
            && at == n
        {
            return Some(crash);
        }
    }
    None
}

/// Moves what an older krowk left into `home`, which does not exist.
pub fn run(home: &Path, env: Env) -> Result<(), String> {
    let Some([(cfg, _), (data, data_var), (cache, _)]) = olds(home, env) else { return Ok(()) };
    let staging = staging(home);
    if ![&cfg, &data, &staging].iter().any(|d| exists(d)) {
        return Ok(());
    }
    let lock_file = lock_path(home);
    let io = |what: &Path, e: std::io::Error| format!("moving krowk's files into {}: {}: {e}", home.display(), what.display());
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_file).map_err(|e| io(&lock_file, e))?;
    lock.lock().map_err(|e| io(&lock_file, e))?;
    if exists(home) {
        // Another krowk finished while this one waited.
        return Ok(());
    }
    if exists(&staging) {
        // A move cut short: undone, then made again from the start.
        undo(&staging)?;
    }

    // What moves, by one rename each, and where it lands in the home.
    let conf = cfg.join(home::CONFIG);
    let config: Option<Map<String, Value>> = creds::read(&conf).ok().filter(|_| exists(&conf));
    let mut plan: Vec<(PathBuf, PathBuf)> = Vec::new();
    for e in std::fs::read_dir(&cfg).into_iter().flatten().flatten() {
        let n = e.file_name();
        let known = ["credentials.json", "providers", "update-check.json"].iter().any(|k| n == *k) || (n == home::CONFIG && config.is_some());
        if !known {
            plan.push((e.path(), PathBuf::from(n)));
        }
    }
    // The session logs first: krowk.db and its WAL go in beside them.
    let sessions = PathBuf::from(home::SESSIONS);
    if exists(&data.join(home::SESSIONS)) {
        plan.push((data.join(home::SESSIONS), sessions.clone()));
    }
    for e in std::fs::read_dir(&data).into_iter().flatten().flatten() {
        let n = e.file_name();
        let s = n.to_string_lossy();
        match &*s {
            "sessions" | "import.lock" => {}
            "claude" | "codex" => {
                for a in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    plan.push((a.path(), Path::new(home::ACCOUNTS).join(a.file_name())));
                }
            }
            "krowk.db" | "krowk.db-wal" | "krowk.db-shm" | "tui-history.jsonl" => plan.push((e.path(), sessions.join(&n))),
            _ => plan.push((e.path(), PathBuf::from(&n))),
        }
    }

    home::make(&staging)?;
    let s = |rel: &Path| staging.join(rel);
    let result = (|| {
        // The two credential files, merged, and config.json with each
        // account's path pointing at where it goes: written whole; the old
        // ones stay until the home is in place.
        let (reg, prov) = (cfg.join("credentials.json"), cfg.join("providers").join("credentials.json"));
        if exists(&reg) || exists(&prov) {
            let aside = |e: String| format!("{e} — fix it or move it aside, and krowk moves its files into {}", home.display());
            let mut merged: Map<String, Value> = creds::read(&prov).map_err(aside)?;
            merged.extend(creds::registry_section(&reg).map_err(aside)?);
            creds::write(&s(Path::new(home::CREDENTIALS)), &merged)?;
        }
        if let Some(mut v) = config.clone() {
            repoint(&mut v, &data, home, &plan);
            let to = s(Path::new(home::CONFIG));
            creds::write(&to, &v)?;
            if let Ok(m) = std::fs::metadata(&conf) {
                let _ = std::fs::set_permissions(&to, m.permissions());
            }
        }
        let mut journal = std::fs::OpenOptions::new().create(true).append(true).open(s(Path::new(JOURNAL))).map_err(|e| io(&staging, e))?;
        for (from, rel) in &plan {
            let to = s(rel);
            let why = match exists(&to) {
                true => Some(format!("{} would land where another of krowk's files already goes, {} — rename or remove one", from.display(), home.join(rel).display())),
                false => None,
            };
            if let Some(why) = why {
                return Err(why);
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
            }
            // Written down before it is made, so a crash can be undone.
            let record = [bytes(from), vec![0], bytes(&to), vec![0]].concat();
            journal.write_all(&record).and_then(|()| journal.sync_data()).map_err(|e| io(&staging, e))?;
            match fault() {
                Some(true) => return Ok(false),
                Some(false) => return Err(format!("{} could not be moved (a test's fault)", from.display())),
                None => {}
            }
            if let Err(e) = std::fs::rename(from, &to) {
                return Err(match e.kind() {
                    std::io::ErrorKind::CrossesDevices => cross_device(from, &home.join(rel), home, &data, data_var),
                    _ => format!("{} could not be moved to {}: {e}", from.display(), home.join(rel).display()),
                });
            }
            if fault() == Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    match result {
        Ok(false) => return Err("a crash, as a test asked".into()),
        Ok(true) => {}
        Err(why) => {
            let undone = undo(&staging).err().map(|e| format!(" — and undoing it failed too: {e}")).unwrap_or_else(|| " Nothing was moved.".into());
            return Err(format!("krowk could not move its files into {}: {why}.{undone} Run krowk again once that is fixed, or set KROWK_HOME to an absolute path to start a new home and leave these files alone.", home.display()));
        }
    }
    if fault() == Some(true) {
        return Err("a crash, as a test asked".into());
    }
    std::fs::rename(&staging, home).map_err(|e| io(home, e))?;

    // In place: what was read rather than moved goes, and the cache.
    let _ = std::fs::remove_file(home.join(JOURNAL));
    for f in ["credentials.json", "providers/credentials.json", "providers/credentials.lock", "update-check.json"] {
        let _ = std::fs::remove_file(cfg.join(f));
    }
    if config.is_some() {
        let _ = std::fs::remove_file(&conf);
    }
    let _ = std::fs::remove_file(data.join("import.lock"));
    for d in [cfg.join("providers"), data.join("claude"), data.join("codex"), cfg.clone(), data.clone()] {
        let _ = std::fs::remove_dir(d);
    }
    let _ = std::fs::remove_dir_all(&cache);
    let _ = std::fs::write(home.join(home::CHECKED), "");
    eprintln!("krowk: moved krowk's files to {}", home.display());
    Ok(())
}

/// What to do by hand when `from` is on another file system than the home.
fn cross_device(from: &Path, to: &Path, home: &Path, data: &Path, var: Option<&str>) -> String {
    let account = from.parent().and_then(Path::file_name).is_some_and(|n| n == "claude" || n == "codex");
    let repoint = match account {
        true => format!(", then set that account's configDir (or codexHome) in {} to {}", home.join(home::CONFIG).display(), to.display()),
        false => String::new(),
    };
    match var {
        Some(var) if from.starts_with(data) => format!(
            "{} is on another file system than {} ({var} puts it there), and a move across file systems cannot be undone if it is cut short — move it next to it and unset {var}: `mv {} {}`",
            data.display(),
            home.display(),
            data.display(),
            home.parent().unwrap_or(home).join(".local/share/krowk").display()
        ),
        _ => format!(
            "{} is on another file system than {}, and a move across file systems cannot be undone if it is cut short — move it out of the way (`mv {} {}.elsewhere`), run krowk, then `mv {}.elsewhere {}`{repoint}",
            from.display(),
            home.display(),
            from.display(),
            from.display(),
            from.display(),
            to.display()
        ),
    }
}

/// Every rename the journal records, undone in reverse, and the staging
/// directory removed — which by then holds only what was written into it.
fn undo(staging: &Path) -> Result<(), String> {
    let journal = std::fs::read(staging.join(JOURNAL)).unwrap_or_default();
    // A last record cut short by a crash was never renamed: it is skipped.
    // What follows the last NUL is part of a record, or nothing.
    let mut parts: Vec<&[u8]> = journal.split(|b| *b == 0).collect();
    parts.pop();
    let moves: Vec<(PathBuf, PathBuf)> = parts.chunks_exact(2).map(|c| (path(c[0]), path(c[1]))).collect();
    for (from, to) in moves.iter().rev() {
        if exists(to) && !exists(from) {
            if let Some(parent) = from.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::rename(to, from).map_err(|e| format!("{} could not be put back at {}: {e} — it is still in {}", to.display(), from.display(), staging.display()))?;
        }
    }
    for f in [JOURNAL, home::CREDENTIALS, home::CONFIG] {
        let _ = std::fs::remove_file(staging.join(f));
    }
    for d in [home::ACCOUNTS, home::SESSIONS, ""] {
        let _ = std::fs::remove_dir(staging.join(d));
    }
    match exists(staging) {
        true => Err(format!("{} still holds files krowk did not put there — look at it before running krowk again", staging.display())),
        false => Ok(()),
    }
}

/// `configDir` and `codexHome` that named an account under the old data
/// directory name it where it goes.
fn repoint(config: &mut Map<String, Value>, data: &Path, home: &Path, plan: &[(PathBuf, PathBuf)]) {
    let Some(Value::Object(instances)) = config.get_mut("instances") else { return };
    for inst in instances.values_mut().filter_map(Value::as_object_mut) {
        for key in ["configDir", "codexHome"] {
            let Some(Value::String(p)) = inst.get_mut(key) else { continue };
            let from = ["claude", "codex"].iter().map(|v| data.join(v)).find(|d| Path::new(p.as_str()).parent() == Some(d.as_path()));
            if let Some((_, rel)) = from.and_then(|_| plan.iter().find(|(f, _)| f == Path::new(p.as_str()))) {
                *p = home.join(rel).display().to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// An old layout with something of every kind: both credential files,
    /// config.json naming an account, the account, a session, krowk.db and
    /// its WAL, remembered grants, a cache.
    fn old_layout(name: &str) -> (PathBuf, impl Fn(&str) -> String + Clone + use<>) {
        let d = std::env::temp_dir().join(format!("krowk-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (cfg, data, cache) = (d.join(".config/krowk"), d.join(".local/share/krowk"), d.join(".cache/krowk"));
        for dir in [cfg.join("providers"), data.join("claude/claude-work"), data.join("sessions/s1"), cache.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let files = [
            (cfg.join("credentials.json"), r#"{"default":"ws","workspaces":{"ws":{"token":"the-key"}}}"#.to_string()),
            (cfg.join("providers/credentials.json"), r#"{"version":1,"keys":{"openai":{"env":"K"}}}"#.to_string()),
            (cfg.join("config.json"), json!({"instances": {"claude:work": {"kind": "claude-code", "configDir": data.join("claude/claude-work")}}}).to_string()),
            (cfg.join("permissions.json"), "{}".to_string()),
            (data.join("claude/claude-work/login"), "x".to_string()),
            (data.join("sessions/s1/events.jsonl"), "{}".to_string()),
            (data.join("krowk.db"), "db".to_string()),
            (data.join("krowk.db-wal"), "wal".to_string()),
            (data.join("import.lock"), String::new()),
            (cache.join("models.json"), "{}".to_string()),
        ];
        for (f, body) in files {
            std::fs::write(f, body).unwrap();
        }
        let h = d.display().to_string();
        (d, move |k: &str| if k == "HOME" { h.clone() } else { String::new() })
    }

    /// Every path under `d` with what its files hold — but the migration's
    /// lock file, which is never removed.
    fn tree(d: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![d.to_path_buf()];
        while let Some(at) = stack.pop() {
            for e in std::fs::read_dir(&at).into_iter().flatten().flatten() {
                let p = e.path();
                let rel = p.strip_prefix(d).unwrap().display().to_string();
                if rel.ends_with(".migrate.lock") {
                    continue;
                }
                match p.is_dir() {
                    true => {
                        out.push(format!("{rel}/"));
                        stack.push(p);
                    }
                    false => out.push(format!("{rel} {}", std::fs::read_to_string(&p).unwrap_or_default())),
                }
            }
        }
        out.sort();
        out
    }

    fn with_fault<R>(at: Option<(usize, bool)>, f: impl FnOnce() -> R) -> R {
        FAULT.with(|c| c.set(at));
        STEP.with(|c| c.set(0));
        let r = f();
        FAULT.with(|c| c.set(None));
        r
    }

    fn assert_moved(d: &Path) {
        let home = d.join(".krowk");
        let c: Map<String, Value> = creds::read(&home.join(home::CREDENTIALS)).unwrap();
        assert_eq!((c["workspaces"]["ws"]["token"].as_str(), c["keys"]["openai"]["env"].as_str()), (Some("the-key"), Some("K")));
        let cfg: Map<String, Value> = creds::read(&home.join(home::CONFIG)).unwrap();
        assert_eq!(cfg["instances"]["claude:work"]["configDir"], home.join("accounts/claude-work").display().to_string());
        for f in ["accounts/claude-work/login", "sessions/s1/events.jsonl", "sessions/krowk.db", "sessions/krowk.db-wal", "permissions.json", home::CHECKED] {
            assert!(home.join(f).exists(), "{f}");
        }
        for gone in [".config/krowk", ".local/share/krowk", ".cache/krowk", ".krowk.migrating", ".krowk/.moves"] {
            assert!(!d.join(gone).exists(), "{gone} is left");
        }
    }

    #[test]
    fn a_crash_after_any_step_is_undone_and_the_next_run_makes_the_whole_move() {
        let (d, env) = old_layout("crash");
        let steps = with_fault(None, || {
            run(&d.join(".krowk"), &env).unwrap();
            STEP.with(|s| s.get())
        });
        assert_moved(&d);
        assert!(steps > 5, "{steps} steps");
        for at in 1..=steps {
            // Undone by hand: everything is back where it was.
            let (d, env) = old_layout(&format!("crash-undo-{at}"));
            let before = tree(&d);
            assert!(with_fault(Some((at, true)), || run(&d.join(".krowk"), &env)).is_err(), "step {at}");
            assert!(!d.join(".krowk").exists(), "a crash at step {at} left a home");
            undo(&staging(&d.join(".krowk"))).unwrap();
            assert_eq!(tree(&d), before, "undoing a crash at step {at}");
            let _ = std::fs::remove_dir_all(&d);
            // Found by the next krowk: undone, then the whole move made.
            let (d, env) = old_layout(&format!("crash-resume-{at}"));
            assert!(with_fault(Some((at, true)), || run(&d.join(".krowk"), &env)).is_err());
            with_fault(None, || run(&d.join(".krowk"), &env)).unwrap();
            assert_moved(&d);
            let _ = std::fs::remove_dir_all(&d);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failure_at_any_step_puts_everything_back_and_refuses() {
        let (d, env) = old_layout("fail");
        let before = tree(&d);
        let home = d.join(".krowk");
        let steps = with_fault(None, || {
            run(&home, &env).unwrap();
            STEP.with(|s| s.get())
        });
        for at in 1..=steps {
            let (d, env) = old_layout("fail");
            let home = d.join(".krowk");
            match with_fault(Some((at, false)), || run(&home, &env)) {
                // A failure a step past the last rename is no failure.
                Ok(()) => assert_moved(&d),
                Err(e) => {
                    assert!(e.contains("Nothing was moved") && e.contains("KROWK_HOME"), "step {at}: {e}");
                    assert_eq!(tree(&d), before, "a failure at step {at} left the old layout changed");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_entries_with_one_name_refuse_the_move_and_change_nothing() {
        let (d, env) = old_layout("clash");
        std::fs::write(d.join(".local/share/krowk/permissions.json"), "stray").unwrap();
        let before = tree(&d);
        let e = run(&d.join(".krowk"), &env).unwrap_err();
        assert!(e.contains("rename or remove one") && e.contains("Nothing was moved"), "{e}");
        assert_eq!(tree(&d), before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_home_that_exists_is_never_merged_into_and_the_old_files_are_named_once() {
        let (d, env) = old_layout("exists");
        let home = d.join(".krowk");
        std::fs::create_dir_all(&home).unwrap();
        let before = tree(&d.join(".config"));
        note_old(&home, &env);
        assert!(home.join(home::CHECKED).exists() && !home.join(home::CREDENTIALS).exists());
        assert_eq!(tree(&d.join(".config")), before, "nothing was taken");
        let _ = std::fs::remove_dir_all(&d);
    }
}
