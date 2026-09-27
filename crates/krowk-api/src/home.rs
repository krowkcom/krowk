//! krowk's one home: `~/.krowk`, or `$KROWK_HOME`. Every file krowk keeps —
//! its config, its keys and logins, its accounts, sessions and caches — is
//! under it, and every path to one is derived here, in the lean build and
//! the full one alike, so the file tools' fence and the files it fences can
//! never be computed apart.
//!
//! ```text
//! config.json        settings, no secrets
//! credentials.json   0600: registry keys, provider logins, stored API keys
//! accounts/<name>/   a named Claude Code or Codex account's own home
//! sessions/          krowk.db and each session's log
//! cache/             the models.dev listing, the update check
//! readiness/         where a vendor is asked whether it is signed in
//! ```
//!
//! The home is `0700` and made on first need. One that is a symlink, or
//! belongs to another user, is refused rather than used: whoever controls
//! where it leads controls krowk's keys. XDG variables are not read.

use crate::creds::Env;
use crate::error::{fail, Error};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const CONFIG: &str = "config.json";
pub const CREDENTIALS: &str = "credentials.json";
pub const ACCOUNTS: &str = "accounts";
pub const SESSIONS: &str = "sessions";
pub const CACHE: &str = "cache";
pub const READINESS: &str = "readiness";
pub const LEDGER: &str = "ledger";

/// The process environment, for the callers that have no `Env` of their own.
pub fn process_env(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

fn no_home() -> Error {
    fail("no_home", "there is no home directory to keep krowk's files in — set HOME, or KROWK_HOME to an absolute path")
}

/// Where the home is, by the environment alone: `$KROWK_HOME` when set,
/// which must be absolute, else `.krowk` in the user's home directory
/// (`HOME`, or `USERPROFILE` on Windows). Never a relative path — that would
/// make a repository's own files krowk's keys and settings.
pub fn resolve(env: Env) -> Result<PathBuf, Error> {
    let own = env("KROWK_HOME");
    if !own.is_empty() {
        let p = PathBuf::from(&own);
        if !p.is_absolute() || p.parent().is_none() {
            return Err(fail("bad_home", format!("KROWK_HOME is {own:?}, which is not an absolute path below the root — set it to one, or unset it for ~/.krowk")));
        }
        return Ok(p);
    }
    let mut user = env("HOME");
    if user.is_empty() && cfg!(windows) {
        user = env("USERPROFILE");
    }
    let p = PathBuf::from(user);
    match p.is_absolute() {
        true => Ok(p.join(".krowk")),
        false => Err(no_home()),
    }
}

/// Homes this process has already checked (and made, or moved into).
static READY: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The home, checked once per process: one `lstat` when it is there. When
/// it is not, the files an older krowk kept under the XDG directories are
/// moved in first (`crate::migrate`), and it is made `0700`.
pub fn dir(env: Env) -> Result<PathBuf, Error> {
    let home = resolve(env)?;
    let mut ready = READY.lock().unwrap_or_else(|e| e.into_inner());
    if !ready.contains(&home) {
        prepare(&home, env)?;
        ready.push(home.clone());
    }
    Ok(home)
}

/// `dir` with the process environment.
pub fn get() -> Result<PathBuf, Error> {
    dir(&process_env)
}

fn prepare(home: &Path, env: Env) -> Result<(), Error> {
    match std::fs::symlink_metadata(home) {
        Ok(m) => own(home, &m).map_err(|m| fail("bad_home", m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Only the default home inherits an older krowk's files: a
            // KROWK_HOME is a sandbox, and never takes the person's own.
            if env("KROWK_HOME").is_empty() {
                crate::migrate::run(home, env).map_err(|m| fail("migration_failed", m))?;
            }
            make(home).map_err(|m| fail("bad_home", m))
        }
        Err(e) => Err(fail("bad_home", format!("{} cannot be read: {e}", home.display()))),
    }
}

/// Makes `dir` `0700`, and when something is already there, holds it to
/// `own`. A home that cannot be made (a read-only file system) is left
/// missing: reads find nothing, and a write says why it failed.
pub fn make(dir: &Path) -> Result<(), String> {
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    match b.create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let m = std::fs::symlink_metadata(dir).map_err(|e| format!("{} cannot be read: {e}", dir.display()))?;
            own(dir, &m)
        }
        Err(_) => Ok(()),
    }
}

/// A directory of krowk's own: a real directory, not a symlink, belonging
/// to this user, and `0700` (made so when it is looser).
pub fn own(dir: &Path, m: &std::fs::Metadata) -> Result<(), String> {
    if m.file_type().is_symlink() {
        return Err(format!("{} is a symlink — krowk keeps its keys there and follows no link to them; move it aside", dir.display()));
    }
    if !m.is_dir() {
        return Err(format!("{} is not a directory — move it aside", dir.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: getuid has no preconditions and cannot fail.
        if m.uid() != unsafe { libc::getuid() } {
            return Err(format!("{} belongs to another user — move it aside", dir.display()));
        }
        if m.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{} cannot be made private: {e}", dir.display()))?;
        }
    }
    Ok(())
}

/// Whether `p` is the home or inside it, judged by where both really lead
/// (`..`, symlinks), and regardless of case, as macOS and Windows open them.
pub fn holds(home: &Path, p: &Path) -> bool {
    let lower = |q: &Path| PathBuf::from(q.to_string_lossy().to_lowercase());
    let real = |q: &Path| q.canonicalize().unwrap_or_else(|_| q.to_path_buf());
    let p = lower(&real(p));
    p.starts_with(lower(home)) || p.starts_with(lower(&real(home)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> String {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()).unwrap_or_default()
    }

    #[test]
    #[cfg(unix)]
    fn the_home_is_krowk_home_or_dot_krowk_and_never_relative() {
        assert_eq!(resolve(&env(&[("HOME", "/h")])).unwrap(), PathBuf::from("/h/.krowk"));
        assert_eq!(resolve(&env(&[("HOME", "/h"), ("KROWK_HOME", "/k")])).unwrap(), PathBuf::from("/k"));
        assert_eq!(resolve(&env(&[("HOME", "/h"), ("KROWK_HOME", "rel/k")])).unwrap_err().code(), "bad_home");
        assert_eq!(resolve(&env(&[("KROWK_HOME", "/")])).unwrap_err().code(), "bad_home");
        assert_eq!(resolve(&env(&[("HOME", "relative")])).unwrap_err().code(), "no_home");
        assert_eq!(resolve(&env(&[])).unwrap_err().code(), "no_home");
        let xdg = env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "/x"), ("XDG_DATA_HOME", "/y")]);
        assert_eq!(resolve(&xdg).unwrap(), PathBuf::from("/h/.krowk"), "XDG is not read");
    }
}
