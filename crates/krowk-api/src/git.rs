//! git as krowk runs it, the one place a git process is made. A repository
//! is a directory the person may not trust, and git runs some commands on
//! its own. Three are turned off for every call krowk makes: hooks (looked
//! for in an empty directory of krowk's instead, `<home>/no-hooks`, which
//! works on Windows as `/dev/null` does not), `core.fsmonitor`, and commit
//! and tag signing (krowk's own commits need none, and with stdin closed a
//! pinentry could only hang). Filters a checkout maps files to — git-lfs's
//! smudge, say — still run, deliberately: a checkout without them is wrong,
//! and their commands come from config, not from the repository's files.
//! An agent's own `git` through the bash tool is the sandbox's business,
//! not this.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// git in `dir`, with hooks, fsmonitor and signing off. For a call that
/// writes (a commit, a checkout, `worktree add`). An error when krowk has
/// no empty directory to offer git as hooks: then nothing is run.
pub fn command(dir: &Path) -> std::io::Result<Command> {
    let mut hooks = OsString::from("core.hooksPath=");
    hooks.push(no_hooks()?);
    let mut c = Command::new("git");
    c.arg("-c").arg(hooks);
    c.args(["-c", "core.fsmonitor=false", "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false"]);
    c.current_dir(dir).stdin(Stdio::null());
    Ok(c)
}

/// `command` for a call that only reads: no optional locks either, so a
/// lookup never rewrites the index under a git the person is running.
pub fn query(dir: &Path) -> std::io::Result<Command> {
    let mut c = command(dir)?;
    c.arg("--no-optional-locks");
    Ok(c)
}

/// A fallback made by this process, for when the home's will not do.
static FRESH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The empty directory hooks are looked for in, judged again on every call:
/// a file put there since would run as a hook.
fn no_hooks() -> std::io::Result<PathBuf> {
    let home = crate::home::get().ok();
    let mut fresh = FRESH.lock().unwrap_or_else(|e| e.into_inner());
    pick(home.as_deref(), &std::env::temp_dir(), &mut fresh)
}

/// `<home>/no-hooks` when it is krowk's own and empty. Else — no home, a
/// home on a read-only disk, one with something in its `no-hooks` — a
/// directory this process made itself under `tmp`, `0700` and under a new
/// name, so nothing can be waiting in it; another when that one has
/// filled. Never a path krowk did not make: `/dev/null` is a directory
/// anyone may create on a Windows drive.
fn pick(home: Option<&Path>, tmp: &Path, fresh: &mut Option<PathBuf>) -> std::io::Result<PathBuf> {
    if let Some(d) = home.map(|h| h.join(crate::home::NO_HOOKS)).filter(|d| empty(d)) {
        return Ok(d);
    }
    if let Some(d) = fresh.as_ref().filter(|d| empty(d)) {
        return Ok(d.clone());
    }
    let d = made(tmp)?;
    *fresh = Some(d.clone());
    Ok(d)
}

/// Whether `d` is a directory of krowk's own (made when missing) with
/// nothing in it.
fn empty(d: &Path) -> bool {
    crate::home::make(d).is_ok() && std::fs::read_dir(d).is_ok_and(|mut e| e.next().is_none())
}

/// A new `0700` directory in `tmp`, made by this call and no one else's:
/// creating one that exists fails, and a new name is tried.
fn made(tmp: &Path) -> std::io::Result<PathBuf> {
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    for attempt in 0..100u32 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let d = tmp.join(format!("krowk-no-hooks-{}-{}", std::process::id(), nanos.wrapping_add(attempt.wrapping_mul(7919))));
        match b.create(&d) {
            Ok(()) => return Ok(d),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not make an empty directory for git's hooks"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("krowk-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A repository's hooks touch a marker: a plain git commit runs them,
    /// krowk's commit and checkout run neither, and do what they were
    /// asked.
    #[test]
    #[cfg(unix)]
    fn a_repositorys_hooks_never_run_from_krowks_git() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("hooks");
        if !Command::new("git").args(["init", "-q"]).current_dir(&d).status().is_ok_and(|s| s.success()) {
            eprintln!("git is not installed: skipping");
            return;
        }
        let marker = d.join("hook-ran");
        for hook in ["post-checkout", "pre-commit"] {
            let p = d.join(".git/hooks").join(hook);
            std::fs::write(&p, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let id = ["-c", "user.name=t", "-c", "user.email=t@t"];
        let plain = Command::new("git").args(id).args(["commit", "-q", "--allow-empty", "-m", "plain"]).current_dir(&d).status().unwrap();
        assert!(plain.success() && marker.exists(), "the hooks would run");
        std::fs::remove_file(&marker).unwrap();

        let run = |args: &[&str]| command(&d).unwrap().args(id).args(args).output().unwrap();
        let out = run(&["commit", "-q", "--allow-empty", "-m", "x"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = run(&["checkout", "-q", "-b", "other"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!marker.exists(), "a hook ran");
        let head = query(&d).unwrap().args(["rev-parse", "--abbrev-ref", "HEAD"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "other");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The home's directory while it is empty and krowk's own; else one
    /// this process made, and a new one once that has something in it.
    #[test]
    #[cfg(unix)]
    fn hooks_are_looked_for_only_where_nothing_can_be_waiting() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("pick");
        let (home, tmp) = (d.join("home"), d.join("tmp"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        let mut fresh = None;
        let own = pick(Some(&home), &tmp, &mut fresh).unwrap();
        assert_eq!(own, home.join("no-hooks"));
        assert_eq!(std::fs::metadata(&own).unwrap().permissions().mode() & 0o777, 0o700);

        std::fs::write(own.join("post-checkout"), "#!/bin/sh\n").unwrap();
        let first = pick(Some(&home), &tmp, &mut fresh).unwrap();
        assert!(first.starts_with(&tmp), "{}", first.display());
        assert_eq!(std::fs::metadata(&first).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(pick(None, &tmp, &mut fresh).unwrap(), first, "the same one, while empty");
        std::fs::write(first.join("pre-commit"), "#!/bin/sh\n").unwrap();
        let second = pick(None, &tmp, &mut fresh).unwrap();
        assert!(second != first && second.starts_with(&tmp));

        // A symlink in the home's place leads somewhere else: not used.
        std::fs::remove_dir_all(&own).unwrap();
        std::os::unix::fs::symlink(&tmp, &own).unwrap();
        assert_eq!(pick(Some(&home), &tmp, &mut fresh).unwrap(), second);
        // Nowhere to make one: an error, not a path krowk did not make.
        std::fs::write(second.join("x"), "").unwrap();
        assert!(pick(None, &d.join("gone"), &mut fresh).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Every git process the workspace makes is made by `command`: a new
    /// `Command::new("git")` anywhere else, tests apart, fails here.
    #[test]
    fn no_git_is_run_but_through_this_module() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap();
        let needles = [concat!("Command::new(", "\"git\")"), concat!("Command::new(", "\"git.exe\")")];
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                let name = e.file_name();
                if p.is_dir() {
                    // A crate's integration tests are tests; dot-directories
                    // (.git, an agent's worktrees) are not this checkout's.
                    let tests = name == "tests" && std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|t| t.contains("[package]"));
                    if !tests && name != "target" && name != "node_modules" && !name.to_string_lossy().starts_with('.') {
                        stack.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let src = outside_tests(&std::fs::read_to_string(&p).unwrap());
                    let n: usize = needles.iter().map(|n| src.matches(n).count()).sum();
                    if n > usize::from(p.ends_with("krowk-api/src/git.rs")) {
                        found.push(p.display().to_string());
                    }
                }
            }
        }
        assert!(found.is_empty(), "git run other than through krowk_api::git: {found:?}");
    }

    /// A source file with its `#[cfg(test)] mod … { … }` blocks taken out.
    fn outside_tests(src: &str) -> String {
        let mut out = String::new();
        let mut lines = src.lines().peekable();
        while let Some(l) = lines.next() {
            if l == "#[cfg(test)]" && lines.peek().is_some_and(|n| n.split_whitespace().any(|w| w == "mod") && n.ends_with('{')) {
                lines.by_ref().find(|n| *n == "}");
                continue;
            }
            out.push_str(l);
            out.push('\n');
        }
        out
    }
}
