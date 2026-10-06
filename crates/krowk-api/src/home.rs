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
//! no-hooks/          empty: where krowk's own git looks for hooks
//! ```
//!
//! The home is `0700` and made on first need. One that is a symlink, or
//! belongs to another user, is refused rather than used: whoever controls
//! where it leads controls krowk's keys. XDG variables are not read for
//! it; the one directory krowk keeps outside it, the worktrees it makes for
//! agents, is under `XDG_DATA_HOME` (`worktrees_root`).

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
/// Kept empty: what `crate::git` points `core.hooksPath` at.
pub const NO_HOOKS: &str = "no-hooks";
/// Marks a home whose old-layout check is done (`migrate::note_old`).
pub const CHECKED: &str = ".old-layout-checked";

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
    resolve_on(env, cfg!(windows))
}

fn resolve_on(env: Env, windows: bool) -> Result<PathBuf, Error> {
    // `..` taken out first: `HOME=/x/gone/../h` would otherwise miss the
    // old layout and make `gone/`, and `KROWK_HOME=$HOME/x/..` make the
    // person's own home krowk's, chmod 0700 and all.
    let user = user_home(env, windows);
    let own = env("KROWK_HOME");
    if !own.is_empty() {
        let p = lexical(Path::new(&own));
        // The user's home itself is judged by where both really lead, and
        // regardless of case, as macOS and Windows open them.
        let is_user = user.as_ref().is_some_and(|u| holds(u, &p) && holds(&p, u));
        if !Path::new(&own).is_absolute() || p.parent().is_none() || is_user {
            return Err(fail("bad_home", format!("KROWK_HOME is {own:?}, which is not an absolute path below the root and apart from your home directory — set it to one, or unset it for ~/.krowk")));
        }
        return Ok(p);
    }
    user.map(|u| u.join(".krowk")).ok_or_else(no_home)
}

/// The user's home directory, `..` taken out: `HOME`, or `USERPROFILE` on
/// Windows; none unless absolute.
fn user_home(env: Env, windows: bool) -> Option<PathBuf> {
    let mut user = env("HOME");
    if user.is_empty() && windows {
        user = env("USERPROFILE");
    }
    Some(PathBuf::from(user)).filter(|p| p.is_absolute()).map(|p| lexical(&p))
}

/// Everything the file tools and `krowk_push` keep away from: the home in
/// use and the default one (`~/.krowk`, when `KROWK_HOME` names another),
/// each with the staging directory and lock a move from the old layout
/// uses beside it.
pub fn fenced(env: Env) -> Vec<PathBuf> {
    let homes = [resolve(env).ok(), user_home(env, cfg!(windows)).map(|u| u.join(".krowk"))];
    homes.into_iter().flatten().flat_map(|h| siblings(&h)).collect()
}

/// Where krowk makes the git worktrees its agents work in (worktrees WT3):
/// `$XDG_DATA_HOME/krowk/worktrees` when `XDG_DATA_HOME` is absolute, else
/// `.local/share/krowk/worktrees` in the user's home; none without either.
/// Outside krowk's home on purpose: the home is fenced from the file tools
/// and hidden in the sandbox, and an agent must edit the files in its
/// worktree. Being under it is not what makes a directory krowk's — the
/// environment names it — so the sandbox checks the worktree itself too
/// (`krowk_harness::sandbox::managed_worktree`).
pub fn worktrees_root(env: Env) -> Option<PathBuf> {
    let xdg = env("XDG_DATA_HOME");
    let data = match Path::new(&xdg).is_absolute() {
        true => lexical(Path::new(&xdg)),
        false => user_home(env, cfg!(windows))?.join(".local/share"),
    };
    Some(data.join("krowk/worktrees"))
}

/// A home, its staging directory and its migration lock.
pub fn siblings(home: &Path) -> [PathBuf; 3] {
    [home.to_path_buf(), crate::migrate::staging(home), crate::migrate::lock_path(home)]
}

/// Homes this process has already checked (and made, or moved into).
static READY: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The home, checked once per process: one `lstat` when it is there, and
/// for the default home one `stat` of its old-layout marker. When it is
/// not, the files an older krowk kept under the XDG directories are moved
/// in first (`crate::migrate`), and it is made `0700`.
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
    // Only the default home inherits an older krowk's files: a KROWK_HOME
    // is a sandbox, and never takes the person's own.
    let inherits = env("KROWK_HOME").is_empty();
    match std::fs::symlink_metadata(home) {
        Ok(m) => {
            own(home, &m).map_err(|m| fail("bad_home", m))?;
            // Once per home, a line naming old files it never took: one
            // `stat` of its marker after that.
            if inherits {
                crate::migrate::note_old(home, env);
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if inherits {
                crate::migrate::run(home, env).map_err(|m| fail("migration_failed", m))?;
            }
            if let Some(parent) = home.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // A home that cannot be made (a read-only file system) is left
            // missing: reads find nothing, and a write says why it failed.
            match make(home) {
                Err(m) if exists(home) => Err(fail("bad_home", m)),
                Err(_) => Ok(()),
                Ok(()) => {
                    // What the move left, or an old layout with nothing to
                    // move, named now, once.
                    if inherits {
                        crate::migrate::note_old(home, env);
                    }
                    Ok(())
                }
            }
        }
        Err(e) => Err(fail("bad_home", format!("{} cannot be read: {e}", home.display()))),
    }
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Makes `dir` `0700` in a parent that exists, or holds what is already
/// there to `own`: looked at first, so one that is there costs one `lstat`.
pub fn make(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) => return own(dir, &m),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{} cannot be read: {e}", dir.display())),
        Err(_) => {}
    }
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    match b.create(dir) {
        Ok(()) => Ok(()),
        // Made by another krowk in between: held to the same rules.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => make(dir),
        Err(e) => Err(format!("{} could not be made: {e}", dir.display())),
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
/// (`..`, symlinks, a part that does not exist yet), and regardless of
/// case, as macOS and Windows open them.
pub fn holds(home: &Path, p: &Path) -> bool {
    let lower = |q: &Path| PathBuf::from(q.to_string_lossy().to_lowercase());
    let homes = [lower(home), lower(&leads(home))];
    [lower(&lexical(p)), lower(&leads(p))].iter().any(|q| homes.iter().any(|h| q.starts_with(h)))
}

/// `.` and `..` taken out by the path's words alone.
pub fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if out.pop() => {}
            c => out.push(c),
        }
    }
    out
}

/// Where `p` leads: its nearest part that exists, every symlink followed,
/// and the rest as written.
fn leads(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = p;
    loop {
        if let Ok(real) = at.canonicalize() {
            return rest.iter().rev().fold(real, |r, n| r.join(n));
        }
        match (at.parent(), at.file_name()) {
            (Some(up), Some(name)) => {
                rest.push(name);
                at = up;
            }
            _ => return lexical(p),
        }
    }
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

    #[test]
    #[cfg(unix)]
    fn dot_dot_is_taken_out_before_the_home_is_judged() {
        assert_eq!(resolve(&env(&[("HOME", "/x/gone/../h")])).unwrap(), PathBuf::from("/x/h/.krowk"));
        assert_eq!(resolve(&env(&[("HOME", "/h"), ("KROWK_HOME", "/k/sub/..")])).unwrap(), PathBuf::from("/k"));
        // The person's own home, however spelled, and the root are refused.
        for own in ["/h/x/..", "/h", "/h/./", "/k/.."] {
            let e = move |k: &str| match k {
                "HOME" => "/h".to_string(),
                "KROWK_HOME" => own.to_string(),
                _ => String::new(),
            };
            assert_eq!(resolve(&e).unwrap_err().code(), "bad_home", "{own}");
        }
        let fenced = fenced(&env(&[("HOME", "/h"), ("KROWK_HOME", "/k")]));
        for p in ["/k", "/k.migrating", "/k.migrate.lock", "/h/.krowk", "/h/.krowk.migrating", "/h/.krowk.migrate.lock"] {
            assert!(fenced.contains(&PathBuf::from(p)), "{p} in {fenced:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn worktrees_live_under_the_data_dir_outside_the_home() {
        assert_eq!(worktrees_root(&env(&[("HOME", "/h")])), Some(PathBuf::from("/h/.local/share/krowk/worktrees")));
        assert_eq!(worktrees_root(&env(&[("HOME", "/h"), ("XDG_DATA_HOME", "/d/x/..")])), Some(PathBuf::from("/d/krowk/worktrees")));
        assert_eq!(worktrees_root(&env(&[("HOME", "/h"), ("XDG_DATA_HOME", "rel")])), Some(PathBuf::from("/h/.local/share/krowk/worktrees")), "a relative XDG_DATA_HOME is ignored");
        assert_eq!(worktrees_root(&env(&[("HOME", "/h"), ("KROWK_HOME", "/k")])), Some(PathBuf::from("/h/.local/share/krowk/worktrees")), "not under KROWK_HOME");
        assert_eq!(worktrees_root(&env(&[])), None);
    }

    // Windows sets USERPROFILE and no HOME; elsewhere only HOME counts.
    #[test]
    #[cfg(unix)]
    fn on_windows_the_home_is_under_userprofile_when_home_is_unset() {
        let up = env(&[("USERPROFILE", "/Users/ada")]);
        assert_eq!(resolve_on(&up, true).unwrap(), PathBuf::from("/Users/ada/.krowk"));
        assert_eq!(resolve_on(&up, false).unwrap_err().code(), "no_home");
        assert_eq!(resolve_on(&env(&[("HOME", "/h"), ("USERPROFILE", "/u")]), true).unwrap(), PathBuf::from("/h/.krowk"));
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("krowk-home-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn a_home_is_made_private_and_one_that_is_a_symlink_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("own");
        let home = d.join("h");
        let s = home.display().to_string();
        let env = move |k: &str| if k == "KROWK_HOME" { s.clone() } else { String::new() };
        assert_eq!(dir(&env).unwrap(), home);
        assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
        // Loosened by hand: closed again the next time a process looks.
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare(&home, &env).unwrap();
        assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
        // A symlink in its place leads somewhere else: refused, not followed.
        let link = d.join("link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let l = link.display().to_string();
        let e = dir(&move |k: &str| if k == "KROWK_HOME" { l.clone() } else { String::new() }).unwrap_err();
        assert!(e.code() == "bad_home" && e.fix().contains("is a symlink"), "{}", e.fix());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_path_is_in_the_home_however_it_is_spelled() {
        let d = scratch("holds");
        let home = d.join(".krowk");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        for inside in [home.join("credentials.json"), home.join("sessions/../credentials.json"), d.join(".KROWK/config.json"), home.clone()] {
            assert!(holds(&home, &inside), "{}", inside.display());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&home, d.join("alias")).unwrap();
            assert!(holds(&home, &d.join("alias/credentials.json")));
        }
        assert!(!holds(&home, &d.join("other/credentials.json")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
