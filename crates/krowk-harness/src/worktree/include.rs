//! Included files (WT7): the third prepare step. A new worktree has none
//! of the main checkout's ignored files, and some of them are what an
//! agent needs to run the project at all: `.env`, `.env.local`, a local
//! config. Claude Code's answer is `.worktreeinclude`, and krowk follows
//! it: when the main checkout has one at its top, every file it matches
//! (gitignore syntax) that git ignores is copied into the worktree.
//!
//! - **Matched by git, not by krowk**: `git ls-files --others --ignored
//!   --exclude-from=.worktreeinclude` in the main checkout lists the
//!   untracked files the patterns match, negations and all, and `git
//!   check-ignore` keeps those git ignores in the main checkout and in the
//!   worktree both. So a file tracked on either side is never copied, nor
//!   one the worktree would see as a change, which would keep it.
//! - **Not the seeded directories**: `worktrees.seed`'s names (WT5's) are
//!   left out of the listing by pathspec, so git does not walk a `target`
//!   or `node_modules` of a hundred thousand files looking for a `.env`;
//!   those directories are seeding's, copied whole or deliberately not.
//!   Other ignored directories are walked.
//! - **Copies, never symlinks**: a link would let the agent edit the
//!   person's real secrets, and lead out of the worktree the sandbox
//!   shows. A clone where the probe found copy-on-write works
//!   (`seed::cow`), else a plain copy, modes and mtimes kept.
//! - **Never through a link**: a file that is a symlink, or under a
//!   directory that is one, in the main checkout or in the worktree, is
//!   not copied, so nothing outside the main checkout is read and nothing
//!   outside the worktree written. Only regular files are copied.
//! - **Never over a file**: one the worktree already has stays as it is.
//!
//! Only `.worktreeinclude` at the main checkout's top counts, as in Claude
//! Code. Nothing here stops the agent, nor tells it anything: what was not
//! copied, and why, goes to the repository's `seed.log`.

use super::{Prepare, query, seed};
use std::collections::HashSet;
use std::path::{Component, Path};

/// The file in the main checkout's top that lists what to copy.
pub const INCLUDE_FILE: &str = ".worktreeinclude";

/// The include step (see the module docs).
pub(super) fn include(p: &Prepare<'_>) -> Option<String> {
    let wt = p.worktree;
    let dir = wt.repo_dir();
    let log = |line: &str| seed::log(dir, &wt.hex, line);
    let list = wt.main.join(INCLUDE_FILE);
    match list.symlink_metadata() {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            log(&format!("nothing copied: {INCLUDE_FILE} is not a regular file"));
            return None;
        }
        Err(_) => return None,
    }
    let paths = match listed(&wt.main, &list, &p.config.seed()) {
        Ok(paths) => paths,
        Err(why) => {
            log(&format!("nothing copied from {INCLUDE_FILE}: {why}"));
            return None;
        }
    };
    if paths.is_empty() {
        return None;
    }
    let ignored = match (ignored(&wt.main, &paths), ignored(&wt.path, &paths)) {
        (Ok(main), Ok(here)) => main.intersection(&here).cloned().collect::<HashSet<_>>(),
        (Err(why), _) | (_, Err(why)) => {
            log(&format!("nothing copied from {INCLUDE_FILE}: {why}"));
            return None;
        }
    };
    let cow = seed::cow(dir, &wt.common);
    let mut copied = 0;
    for path in &paths {
        if !ignored.contains(path) {
            log(&format!("{path} not copied: git does not ignore it in both checkouts, so it is tracked or would be a change"));
            continue;
        }
        match copy(&wt.main, &wt.path, path, cow) {
            Ok(()) => copied += 1,
            Err(why) => log(&format!("{path} not copied: {why}")),
        }
    }
    if copied > 0 {
        log(&format!("{copied} file(s) copied from {INCLUDE_FILE}"));
    }
    None
}

/// The untracked files of the main checkout `main` that the patterns in
/// `list` match, outside the top-level directories `seeded`, relative to
/// its top. Blocking.
fn listed(main: &Path, list: &Path, seeded: &[String]) -> Result<Vec<String>, String> {
    let mut from = std::ffi::OsString::from("--exclude-from=");
    from.push(list);
    let mut c = query(main).map_err(|e| e.to_string())?;
    c.args(["ls-files", "-z", "--others", "--ignored"]).arg(from).arg("--");
    // Only exclusions: git takes them as "everything but", and does not
    // walk what they name.
    for name in seeded.iter().filter(|n| seed::top_level_name(n)) {
        c.arg(format!(":(exclude,top,literal){name}"));
    }
    let out = c.output().map_err(|e| format!("git ls-files: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(split(&out.stdout))
}

/// Which of `paths` git ignores in the checkout `dir`: not tracked there,
/// and matched by its ignore rules. Blocking.
fn ignored(dir: &Path, paths: &[String]) -> Result<HashSet<String>, String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut c = query(dir).map_err(|e| e.to_string())?;
    c.args(["check-ignore", "-z", "--stdin"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("git check-ignore: {e}"))?;
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // Written beside the read, so neither side waits on a full pipe.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| format!("git check-ignore: {e}"))?;
    // 1 is "none of them".
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(format!("git check-ignore: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    writer.join().unwrap_or_else(|_| Err(std::io::Error::other("the writer panicked"))).map_err(|e| format!("git check-ignore: {e}"))?;
    Ok(split(&out.stdout).into_iter().collect())
}

/// NUL-separated paths, untrimmed: a name may start or end with a space.
/// One that is not UTF-8 is left out (krowk names paths as text).
fn split(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0).filter(|p| !p.is_empty()).filter_map(|p| String::from_utf8(p.to_vec()).ok()).collect()
}

/// `<main>/<rel>` copied to `<into>/<rel>`, a regular file reached through
/// real directories only on both sides, the directories it needs in `into`
/// made, never over anything already there: a clone when `cow`, falling
/// back to a plain copy, mode and mtime kept. Why not, otherwise.
fn copy(main: &Path, into: &Path, rel: &str, cow: bool) -> Result<(), String> {
    let rel = Path::new(rel);
    let parts: Vec<&std::ffi::OsStr> = rel.components().map(|c| if let Component::Normal(n) = c { Some(n) } else { None }).collect::<Option<_>>().ok_or("it is not a path inside the checkout")?;
    let Some((name, dirs)) = parts.split_last() else { return Err("it is not a path inside the checkout".into()) };
    let (mut from, mut to) = (main.to_path_buf(), into.to_path_buf());
    for d in dirs {
        from.push(d);
        to.push(d);
        let shown = from.strip_prefix(main).unwrap_or(&from).display().to_string();
        match from.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{shown} is a symlink in the main checkout, and links are not followed")),
            Err(e) => return Err(format!("{shown}: {e}")),
        }
        match to.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{shown} is not a directory in the worktree")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&to).map_err(|e| format!("create {shown} in the worktree: {e}"))?,
            Err(e) => return Err(format!("{shown} in the worktree: {e}")),
        }
    }
    from.push(name);
    to.push(name);
    match from.symlink_metadata() {
        Ok(m) if m.is_file() => {}
        Ok(m) if m.is_symlink() => return Err("it is a symlink, and links are not copied".into()),
        Ok(_) => return Err("it is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("it is gone from the main checkout".into()),
        Err(e) => return Err(e.to_string()),
    }
    if to.symlink_metadata().is_ok() {
        return Err("the worktree already has it".into());
    }
    if cow && seed::clone_tree(&from, &to).is_ok() {
        return Ok(());
    }
    plain_copy(&from, &to)
}

/// `from` copied to `to`, which must not exist, with its mode and mtime;
/// `from` is not followed if it is a link. On failure `to` is gone.
fn plain_copy(from: &Path, to: &Path) -> Result<(), String> {
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, libc::O_NOFOLLOW);
    let mut src = open.open(from).map_err(|e| e.to_string())?;
    let meta = src.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("it is not a regular file".into());
    }
    // Made with the source's mode, not the umask's: a 0600 `.env` must never
    // sit readable by others while its contents are written.
    let mut create = std::fs::OpenOptions::new();
    create.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut create, std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777);
    let mut dst = create.open(to).map_err(|e| e.to_string())?;
    let done = std::io::copy(&mut src, &mut dst).and_then(|_| meta.modified()).and_then(|m| dst.set_modified(m)).and_then(|()| dst.set_permissions(meta.permissions()));
    if let Err(e) = done {
        drop(dst);
        let _ = std::fs::remove_file(to);
        return Err(e.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{git_ok, has_git, repo};
    use super::super::{Finished, Worktree, create, finish, prepare};
    use super::*;
    use crate::instances::WorktreesConfig;
    use std::path::PathBuf;

    fn prepared(w: &Worktree) -> Vec<String> {
        prepare(&Prepare::new(w, &WorktreesConfig::default()))
    }

    fn logged(w: &Worktree) -> String {
        std::fs::read_to_string(w.repo_dir().join(seed::LOG_FILE)).unwrap_or_default()
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(not(unix))]
    fn mode(p: &Path) -> bool {
        std::fs::metadata(p).unwrap().permissions().readonly()
    }

    #[cfg(unix)]
    fn set_mode(p: &Path, m: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_mode(_: &Path, _: u32) {}

    /// A repository ignoring `.env*` and `config/local.yml`, with both on
    /// disk; None without git.
    fn with_secrets(name: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return None;
        }
        let (base, main, root) = repo(name);
        std::fs::create_dir_all(main.join("config")).unwrap();
        std::fs::write(main.join(".gitignore"), ".env*\n*.bak\n/config/local.yml\n/target\n").unwrap();
        std::fs::write(main.join("config/app.yml"), "tracked\n").unwrap();
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-q", "-m", "ignores"]);
        std::fs::write(main.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(main.join(".env.local"), "LOCAL=1\n").unwrap();
        std::fs::write(main.join("config/local.yml"), "db: local\n").unwrap();
        set_mode(&main.join(".env"), 0o600);
        set_mode(&main.join("config/local.yml"), 0o640);
        Some((base, main, root))
    }

    /// `.env*` and `config/local.yml` listed and ignored: copied with the
    /// same contents and modes, and a copy changed leaves the original as
    /// it was. The worktree still counts as unchanged.
    #[test]
    fn wt7_listed_ignored_files_are_copied_with_their_modes() {
        let Some((base, main, root)) = with_secrets("include-copy") else { return };
        std::fs::write(main.join(INCLUDE_FILE), "# what an agent needs\n.env*\nconfig/local.yml\n").unwrap();
        // Ignored, but not listed: stays behind.
        std::fs::write(main.join("notes.bak"), "").unwrap();
        // Listed, but in a seeded directory: seeding's, not this step's.
        std::fs::create_dir_all(main.join("target/debug")).unwrap();
        std::fs::write(main.join("target/debug/.env"), "built\n").unwrap();
        let w = create(&main, &root, "s1").unwrap();
        assert_eq!(prepared(&w), Vec::<String>::new());
        for path in [".env", ".env.local", "config/local.yml"] {
            let (theirs, ours) = (main.join(path), w.path.join(path));
            assert_eq!(std::fs::read(&ours).unwrap_or_else(|e| panic!("{path}: {e}\n{}", logged(&w))), std::fs::read(&theirs).unwrap(), "{path}");
            assert_eq!(mode(&ours), mode(&theirs), "{path}");
            assert!(!ours.symlink_metadata().unwrap().file_type().is_symlink(), "{path}: a copy, not a link");
        }
        assert!(!w.path.join("notes.bak").exists());
        // Not `target/debug/.env`: seeding clones `target` or not, and git
        // does not look in it.
        let list = main.join(INCLUDE_FILE);
        assert_eq!(listed(&main, &list, &["target".into()]).unwrap(), [".env", ".env.local", "config/local.yml"]);
        assert!(listed(&main, &list, &[]).unwrap().contains(&"target/debug/.env".to_string()));
        assert!(logged(&w).contains(&format!("{} 3 file(s) copied from {INCLUDE_FILE}", w.hex)), "{}", logged(&w));
        std::fs::write(w.path.join(".env"), "SECRET=changed\n").unwrap();
        assert_eq!(std::fs::read_to_string(main.join(".env")).unwrap(), "SECRET=1\n", "the original is untouched");
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "", "ignored copies are no change");
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A tracked file `.worktreeinclude` lists is the checkout's: not
    /// copied over, even when the main checkout's copy has edits.
    #[test]
    fn wt7_a_listed_tracked_file_is_not_copied() {
        let Some((base, main, root)) = with_secrets("include-tracked") else { return };
        std::fs::write(main.join(INCLUDE_FILE), ".env\nconfig/\n").unwrap();
        // Force-added past .gitignore: tracked, though the rules match it.
        git_ok(&main, &["add", "-f", ".env"]);
        git_ok(&main, &["commit", "-q", "-m", "env"]);
        let w = create(&main, &root, "s1").unwrap();
        // Edited in the main checkout after the worktree was made from it.
        std::fs::write(main.join(".env"), "SECRET=edited\n").unwrap();
        std::fs::write(main.join("config/app.yml"), "edited in the main checkout\n").unwrap();
        prepared(&w);
        assert_eq!(std::fs::read_to_string(w.path.join("config/app.yml")).unwrap(), "tracked\n");
        assert_eq!(std::fs::read_to_string(w.path.join(".env")).unwrap(), "SECRET=1\n");
        assert_eq!(std::fs::read_to_string(w.path.join("config/local.yml")).unwrap(), "db: local\n", "the ignored one beside it is copied: {}", logged(&w));
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// No `.worktreeinclude`: nothing is copied, and nothing logged.
    #[test]
    fn wt7_without_worktreeinclude_nothing_happens() {
        let Some((base, main, root)) = with_secrets("include-none") else { return };
        let w = create(&main, &root, "s1").unwrap();
        assert_eq!(prepared(&w), Vec::<String>::new());
        assert!(!w.path.join(".env").exists() && !w.path.join("config/local.yml").exists());
        assert!(!logged(&w).contains(INCLUDE_FILE), "{}", logged(&w));
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A listed file that is a symlink, or sits under a symlinked
    /// directory, is not followed out of the main checkout; nor is one the
    /// worktree's own symlink would lead out of it. Each is logged, and the
    /// rest copied.
    #[cfg(unix)]
    #[test]
    fn wt7_symlinks_are_never_followed() {
        let Some((base, main, root)) = with_secrets("include-links") else { return };
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "not the checkout's\n").unwrap();
        std::fs::write(main.join(".gitignore"), ".env*\n/config/local.yml\n/linked\n").unwrap();
        // A tracked symlink, so the worktree has it too, leading outside.
        std::os::unix::fs::symlink(&outside, main.join("tracked-out")).unwrap();
        git_ok(&main, &["add", ".gitignore", "tracked-out"]);
        git_ok(&main, &["commit", "-q", "-m", "links"]);
        std::os::unix::fs::symlink(outside.join("secret"), main.join(".env.link")).unwrap();
        std::os::unix::fs::symlink(&outside, main.join("linked")).unwrap();
        std::fs::write(main.join(INCLUDE_FILE), ".env*\nlinked/\nlinked/secret\n").unwrap();
        let w = create(&main, &root, "s1").unwrap();
        prepared(&w);
        assert_eq!(std::fs::read_to_string(w.path.join(".env")).unwrap(), "SECRET=1\n");
        assert!(w.path.join(".env.link").symlink_metadata().is_err(), "a symlinked file is not copied");
        assert!(w.path.join("linked").symlink_metadata().is_err(), "nor a symlinked directory");
        assert!(logged(&w).contains(".env.link not copied: it is a symlink"), "{}", logged(&w));
        // What git lists is never under a link; a path that is, is refused.
        assert_eq!(copy(&main, &w.path, "linked/secret", false), Err("linked is a symlink in the main checkout, and links are not followed".into()));
        // A main checkout where `tracked-out` is a real directory: the
        // worktree's link of that name is not written through.
        let other = base.join("other");
        std::fs::create_dir_all(other.join("tracked-out")).unwrap();
        std::fs::write(other.join("tracked-out/x"), "x\n").unwrap();
        assert_eq!(copy(&other, &w.path, "tracked-out/x", false), Err("tracked-out is not a directory in the worktree".into()));
        assert!(!outside.join("x").exists(), "nothing written through the worktree's link");
        assert_eq!(copy(&main, &w.path, "../escape", false), Err("it is not a path inside the checkout".into()));
        assert_eq!(std::fs::read_to_string(outside.join("secret")).unwrap(), "not the checkout's\n");
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A plain copy keeps mode and mtime, never replaces a file, and does
    /// not follow a link.
    #[test]
    fn wt7_a_plain_copy_keeps_mode_and_mtime_and_overwrites_nothing() {
        let base = std::env::temp_dir().join(format!("krowk-include-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a"), "a\n").unwrap();
        set_mode(&base.join("a"), 0o751);
        let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(base.join("a")).unwrap().set_modified(when).unwrap();
        plain_copy(&base.join("a"), &base.join("b")).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("b")).unwrap(), "a\n");
        assert_eq!(mode(&base.join("b")), mode(&base.join("a")));
        assert_eq!(std::fs::metadata(base.join("b")).unwrap().modified().unwrap(), when);
        std::fs::write(base.join("c"), "c\n").unwrap();
        assert!(plain_copy(&base.join("a"), &base.join("c")).is_err());
        assert_eq!(std::fs::read_to_string(base.join("c")).unwrap(), "c\n");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("a"), base.join("link")).unwrap();
            assert!(plain_copy(&base.join("link"), &base.join("d")).is_err());
            assert!(!base.join("d").exists());
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
