//! git as krowk runs it, the one place a git process is made. A repository
//! is a directory the person may not trust — one a model was handed, one
//! just cloned — and its config and `.git/hooks` name commands git executes
//! on its own: `core.fsmonitor` on `status` and `ls-files`, `post-checkout`
//! on `checkout` and `worktree add`, `pre-commit` on `commit`. None of them
//! runs from krowk's own calls: hooks are looked for in an empty directory
//! of krowk's (`<local data>/no-hooks`, which works on Windows, where
//! `/dev/null` does not), fsmonitor is off, and stdin is closed so nothing
//! waits on a prompt. An agent's own `git` through the bash tool is the
//! sandbox's business, not this.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// What hooks are looked for in when krowk has no directory of its own to
/// offer: a file, so `<it>/post-checkout` is never there.
const NOWHERE: &str = "/dev/null";

/// git in `dir`, nothing in the repository run with it. For a call that
/// writes (a commit, a checkout, `worktree add`).
pub fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-c").arg(format!("core.hooksPath={}", no_hooks().display()));
    c.args(["-c", "core.fsmonitor=false"]).current_dir(dir).stdin(Stdio::null());
    c
}

/// `command` for a call that only reads: no optional locks either, so a
/// lookup never rewrites the index under a git the person is running.
pub fn query(dir: &Path) -> Command {
    let mut c = command(dir);
    c.arg("--no-optional-locks");
    c
}

/// The empty directory hooks are looked for in, made and checked once per
/// process.
fn no_hooks() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| empty_dir(crate::home::local_data(&crate::home::process_env)).unwrap_or_else(|| PathBuf::from(NOWHERE)))
}

/// `<data>/no-hooks`, made when missing; none when it cannot be made, is
/// not krowk's own (a symlink, another user's) or holds anything — a file
/// put there would run as a hook.
fn empty_dir(data: Option<PathBuf>) -> Option<PathBuf> {
    let d = data?.join(crate::home::NO_HOOKS);
    let _ = std::fs::create_dir_all(d.parent()?);
    crate::home::make(&d).ok()?;
    std::fs::read_dir(&d).ok()?.next().is_none().then_some(d)
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

    /// A repository's hooks touch a marker; krowk's checkout and commit in
    /// it run neither, and do what they were asked.
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
        let run = |args: &[&str]| command(&d).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        let out = run(&["commit", "-q", "--allow-empty", "-m", "x"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = run(&["checkout", "-q", "-b", "other"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!marker.exists(), "a hook ran");
        let head = query(&d).args(["rev-parse", "--abbrev-ref", "HEAD"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "other");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The hooks directory is krowk's own and empty, or none is offered.
    #[test]
    #[cfg(unix)]
    fn the_hooks_directory_is_empty_or_not_used() {
        let d = scratch("empty");
        let made = empty_dir(Some(d.join("data"))).unwrap();
        assert_eq!(made, d.join("data/no-hooks"));
        assert_eq!(empty_dir(Some(d.join("data"))), Some(made.clone()), "the same one, again");
        std::fs::write(made.join("post-checkout"), "#!/bin/sh\n").unwrap();
        assert_eq!(empty_dir(Some(d.join("data"))), None, "one with a file in it");
        std::fs::create_dir_all(d.join("other")).unwrap();
        std::os::unix::fs::symlink(d.join("data/no-hooks"), d.join("other/no-hooks")).unwrap();
        assert_eq!(empty_dir(Some(d.join("other"))), None, "a symlink");
        assert_eq!(empty_dir(None), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Every git process the workspace makes is made by `command`: a new
    /// `Command::new("git")` anywhere else, tests apart, fails here.
    #[test]
    fn no_git_is_run_but_through_this_module() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let needle = concat!("Command::new(", "\"git\")");
        let mut found = Vec::new();
        let mut stack = vec![crates.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                let name = e.file_name();
                if p.is_dir() {
                    if name != "tests" && name != "target" {
                        stack.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    let n = outside_tests(&src).matches(needle).count();
                    let allowed = usize::from(p.ends_with("krowk-api/src/git.rs"));
                    if n > allowed {
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
