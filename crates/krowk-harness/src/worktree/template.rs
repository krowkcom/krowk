//! The template (WT10): a copy of the repository's source tree that a new
//! worktree can be made from in no time. Even seeded, a worktree checks out
//! every file, which takes seconds on a large repository; a btrfs snapshot
//! of a subvolume takes the same moment whatever its size. The repository
//! is not a subvolume, but a directory krowk owns can be one, made and
//! removed without privileges. So krowk keeps one per repository,
//! `<root>/<repo-id>/template`: the tree at a commit and the main checkout's
//! heavy directories, with no `.git`.
//!
//! - **Only where it is cheap**: the worktrees root on btrfs, `btrfs` on
//!   PATH, and the root able to clone the repository's files (`seed::cow`),
//!   which a btrfs file system other than the repository's cannot.
//!   Anywhere else it is `Unavailable`, and a worktree is made the usual
//!   way.
//! - **Made** with `btrfs subvolume create` and filled with `git read-tree
//!   -u <commit>` through an index of its own, `<repo-id>/template.index`,
//!   with the repository's git directory: the person's checkout filters
//!   run, as for any checkout, and no hook does (`krowk_api::git`).
//! - **Brought to another commit** with `git read-tree -m -u <old> <new>`,
//!   which rewrites what changed and removes what was deleted. The commit
//!   it holds is `<repo-id>/template.sha`, outside the template, removed
//!   before any change and written whole after it: a template without one
//!   is in a state nobody knows, so it is deleted and made again. Asked for
//!   the commit it holds, nothing is touched.
//! - **Heavy directories** (`worktrees.seed`) cloned in from the main
//!   checkout by WT5's rules (`seed::seed_dir`), when the main checkout's
//!   copy is newer than the template's and git ignores it there. While
//!   they are, `<repo-id>/template.seeding` says so: a template it is
//!   found beside was cut short mid-clone, an old copy set aside or a new
//!   one half made inside it, and is deleted and made again.
//!
//! - **Fresh to cargo**: once `target` is cloned in, its files get the main
//!   checkout's mtimes where their bytes are the same, and a later one
//!   elsewhere (`seed::set_mtimes`), as a seeded worktree's do; a snapshot
//!   keeps them. Then the index takes their stat data, a moment after the
//!   files were written, so git does not take them for racily clean and
//!   hash them again in every snapshot.
//! - **A worktree from it** (WT11, `snapshot`): `git worktree add
//!   --no-checkout` into an empty directory under `<repo-id>/scratch/`, a
//!   snapshot of the template where the worktree goes, the `.git` file
//!   moved into it and `git worktree repair`ed, and the template's index
//!   copied in as the worktree's and refreshed: the snapshot's files have
//!   another `st_dev`, and only a refresh makes git take them for
//!   unchanged. A failure at any step takes all of it back.
//!
//! The caller holds the repository's lock (`super::lock`) throughout, as for
//! any change to its worktrees. Nothing in it is the agent's: no sandbox
//! writes `<root>/<repo-id>`.

use super::seed::{self, Seeded};
use super::{Error, RepoLock, Worktree, last_lines, query, random_hex, read};
use crate::instances::WorktreesConfig;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// The template, in the repository's directory under the root.
pub const TEMPLATE_DIR: &str = "template";
/// Its index, beside it.
pub const INDEX_FILE: &str = "template.index";
/// The commit it holds, beside it.
pub const SHA_FILE: &str = "template.sha";
/// There while the heavy directories are cloned in, beside it.
pub const SEEDING_FILE: &str = "template.seeding";
/// Where a worktree made from it is registered with git before the
/// snapshot is there to hold it, beside it: empty between creations.
pub const SCRATCH_DIR: &str = "scratch";

/// How long the template's index is written after its files: past the
/// file system clock's tick, so git takes no file for racily clean.
const TICK: std::time::Duration = std::time::Duration::from_millis(20);

/// A template holding a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// `<root>/<repo-id>/template`, a btrfs subvolume.
    pub path: PathBuf,
    /// The commit its files are.
    pub sha: String,
}

/// Why there is no template here: none of these is an error, a worktree is
/// made without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The worktrees root is not on btrfs.
    NotBtrfs,
    /// No `btrfs` on PATH to make a subvolume with.
    NoBtrfs,
    /// The root cannot clone the repository's files: another file system.
    OtherFilesystem,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Unavailable::NotBtrfs => "the worktrees directory is not on btrfs",
            Unavailable::NoBtrfs => "there is no btrfs program on PATH",
            Unavailable::OtherFilesystem => "the worktrees directory is not on the repository's file system",
        })
    }
}

/// What `ensure_template` made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    Ready(Template),
    Unavailable(Unavailable),
}

/// The template of the repository whose common git directory is `common`
/// and main checkout `main`, in its directory under the root `repo_dir`,
/// holding the commit `sha`: made, brought to `sha`, or left as it is when
/// it holds `sha` already (see the module docs). `_held` is the
/// repository's lock, which the caller holds. An error leaves no template.
/// Blocking: off the async runtime.
pub fn ensure_template(_held: &RepoLock, repo_dir: &Path, common: &Path, main: &Path, sha: &str, config: &WorktreesConfig) -> Result<Ensured, Error> {
    ensure(std::env::var_os("PATH").as_deref(), repo_dir, common, main, sha, config)
}

/// `ensure_template`, `btrfs` looked for on `path`.
fn ensure(path: Option<&OsStr>, repo_dir: &Path, common: &Path, main: &Path, sha: &str, config: &WorktreesConfig) -> Result<Ensured, Error> {
    std::fs::create_dir_all(repo_dir).map_err(|e| Error::Failed(format!("create {}: {e}", repo_dir.display())))?;
    if !on_btrfs(repo_dir) {
        return Ok(Ensured::Unavailable(Unavailable::NotBtrfs));
    }
    let Some(btrfs) = on_path("btrfs", path) else { return Ok(Ensured::Unavailable(Unavailable::NoBtrfs)) };
    if !seed::cow(repo_dir, common) {
        return Ok(Ensured::Unavailable(Unavailable::OtherFilesystem));
    }
    let t = Paths::new(repo_dir);
    // A reseed cut short left the template with what it was cloning: no
    // commit vouches for it.
    let cut = t.seeding.symlink_metadata().is_ok();
    let held = std::fs::read_to_string(&t.sha).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && !cut);
    let there = t.dir.symlink_metadata().is_ok_and(|m| m.is_dir()) && t.index.is_file();
    let ready = Template { path: t.dir.clone(), sha: sha.to_string() };
    match held {
        Some(old) if there && old == sha => return Ok(Ensured::Ready(ready)),
        Some(old) if there => {
            // Gone before the first change: a refresh cut short leaves a
            // template no record vouches for.
            remove(&t.sha)?;
            if t.git(common)?.args(["read-tree", "--no-sparse-checkout", "-m", "-u", &old, sha]).output().is_ok_and(|o| o.status.success()) {
                return finish(&t, common, main, sha, config, false).map(|()| Ensured::Ready(ready));
            }
            // A file in the way (one the main checkout's build put where
            // the new commit has a tracked one), or a template changed
            // under it: made again from nothing.
        }
        _ => {}
    }
    discard(&t);
    let made = create(&t, &btrfs, common, sha).and_then(|()| finish(&t, common, main, sha, config, true));
    if made.is_err() {
        discard(&t);
    }
    made.map(|()| Ensured::Ready(ready))
}

/// The template's files beside it.
struct Paths {
    dir: PathBuf,
    index: PathBuf,
    sha: PathBuf,
    seeding: PathBuf,
}

impl Paths {
    fn new(repo_dir: &Path) -> Paths {
        let abs = std::path::absolute(repo_dir).unwrap_or_else(|_| repo_dir.to_path_buf());
        Paths { dir: abs.join(TEMPLATE_DIR), index: abs.join(INDEX_FILE), sha: abs.join(SHA_FILE), seeding: abs.join(SEEDING_FILE) }
    }

    /// git on the template: the repository's git directory, the template
    /// as its work tree, its own index, written whole (no shared index
    /// beside it), and no sparse checkout the repository may have, which
    /// would leave files out.
    fn git(&self, common: &Path) -> Result<Command, Error> {
        let mut c = super::git(&self.dir)?;
        c.env("GIT_DIR", common).env("GIT_WORK_TREE", &self.dir).env("GIT_INDEX_FILE", &self.index).args(["-c", "core.splitIndex=false"]);
        Ok(c)
    }
}

/// A new subvolume, filled with `sha`'s files.
fn create(t: &Paths, btrfs: &Path, common: &Path, sha: &str) -> Result<(), Error> {
    let out = Command::new(btrfs).args(["subvolume", "create"]).arg(&t.dir).stdin(Stdio::null()).output().map_err(|e| Error::Failed(format!("btrfs subvolume create: {e}")))?;
    if !out.status.success() {
        return Err(Error::Failed(format!("btrfs subvolume create: {}", last_lines(&String::from_utf8_lossy(&out.stderr)))));
    }
    read(t.git(common)?.args(["read-tree", "--no-sparse-checkout", "-m", "-u", sha]), "read-tree").map(drop)
}

/// After the files are `sha`'s: the heavy directories brought up to date,
/// the mtimes matched when `target` was cloned in, the index refreshed
/// when every file was just written (`made`) or its mtimes were, then
/// `sha` recorded, whole or not at all.
fn finish(t: &Paths, common: &Path, main: &Path, sha: &str, config: &WorktreesConfig, made: bool) -> Result<(), Error> {
    let names: Vec<String> = config.seed().into_iter().filter(|n| seed::top_level_name(n)).collect();
    let mut target = false;
    // No marker, no clone: a crash mid-clone must be found.
    if !names.is_empty() && std::fs::write(&t.seeding, b"").is_ok() {
        for name in &names {
            target |= reseed(t, common, main, name) && name == "target";
        }
        remove(&t.seeding)?;
    }
    if target && let Ok(tracked) = read(t.git(common)?.args(["ls-files", "-z"]), "ls-files") {
        seed::set_mtimes(&tracked, &t.dir, main);
    }
    if made || target {
        // Only spares each snapshot's refresh the hashing.
        std::thread::sleep(TICK);
        let _ = read(t.git(common)?.args(["update-index", "-q", "--refresh"]), "update-index");
    }
    let part = t.sha.with_file_name(format!(".{SHA_FILE}.{}", random_hex()));
    let written = std::fs::write(&part, format!("{sha}\n")).and_then(|()| std::fs::rename(&part, &t.sha));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&part);
        Error::Failed(format!("write {}: {e}", t.sha.display()))
    })
}

/// `<main>/<name>` cloned into the template by WT5's rules, when it is
/// newer than the template's copy (or the template has none) and git
/// ignores it in the template: a tracked directory of that name is the
/// commit's. The old copy is set aside while the new one is cloned, and
/// put back when it is not. Seeding only saves work: nothing here fails.
/// Whether it was cloned.
fn reseed(t: &Paths, common: &Path, main: &Path, name: &str) -> bool {
    let (from, to) = (main.join(name), t.dir.join(name));
    let Some(theirs) = newest(&from) else { return false };
    let ours = to.symlink_metadata().ok();
    if ours.as_ref().is_some_and(|m| !m.is_dir()) || (ours.is_some() && newest(&to).is_some_and(|n| n >= theirs)) {
        return false;
    }
    let ignored = t.git(common).is_ok_and(|mut c| c.args(["check-ignore", "-q", "--", &format!("{name}/")]).output().is_ok_and(|o| o.status.success()));
    if !ignored {
        return false;
    }
    let aside = t.dir.join(format!(".krowk-old-{name}-{}", random_hex()));
    if ours.is_some() && std::fs::rename(&to, &aside).is_err() {
        return false;
    }
    match seed::seed_dir(main, &t.dir, name) {
        Seeded::Copied => {
            let _ = std::fs::remove_dir_all(&aside);
            true
        }
        Seeded::Skipped(_) | Seeded::Failed(_) => {
            if ours.is_some() {
                let _ = std::fs::rename(&aside, &to);
            }
            false
        }
    }
}

/// Whether a template can be made in `dir`, the repository's directory
/// under the root, if its files can be cloned there (`seed::cow`): btrfs,
/// and `btrfs` on PATH.
pub(super) fn snapshots(dir: &Path) -> bool {
    on_btrfs(dir) && on_path("btrfs", std::env::var_os("PATH").as_deref()).is_some()
}

#[cfg(test)]
thread_local! {
    /// Set by a test: the next snapshot fails once it is made, as any of
    /// its steps might.
    pub(super) static FAIL_AFTER_SNAPSHOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The new worktree `wt` (its directory not there yet, its branch not
/// made) as a snapshot of the template at `wt.base` (see the module
/// docs): whether it carried the heavy directories `config` names, which
/// its seed step then leaves alone. None where there is no template, and
/// nothing made. An error has taken back everything it made: the
/// snapshot, git's record of the worktree, its branch. `held` is the
/// repository's lock. Blocking.
pub(super) fn snapshot(held: &RepoLock, wt: &Worktree, config: &WorktreesConfig) -> Result<Option<bool>, Error> {
    let dir = wt.repo_dir();
    let template = match ensure_template(held, dir, &wt.common, &wt.main, &wt.base, config)? {
        Ensured::Ready(t) => t,
        Ensured::Unavailable(_) => return Ok(None),
    };
    let Some(btrfs) = on_path("btrfs", std::env::var_os("PATH").as_deref()) else { return Ok(None) };
    let scratch = std::path::absolute(dir.join(SCRATCH_DIR).join(&wt.hex)).map_err(|e| Error::Failed(format!("the scratch directory: {e}")))?;
    let made = snapshot_steps(wt, &template, &btrfs, &scratch);
    if made.is_err() {
        take_back(wt, &scratch);
    }
    made.map(|()| Some(config.seed().iter().any(|n| seed::top_level_name(n) && wt.path.join(n).symlink_metadata().is_ok_and(|m| m.is_dir()))))
}

/// The steps of `snapshot`, in order; the first that fails stops them.
fn snapshot_steps(wt: &Worktree, template: &Template, btrfs: &Path, scratch: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(scratch.parent().unwrap_or(scratch)).map_err(|e| Error::Failed(format!("create {}: {e}", scratch.display())))?;
    // `worktree add` takes an empty directory only, and names git's record
    // of it after its name: the worktree's own.
    read(super::git(&wt.main)?.args(["worktree", "add", "--quiet", "--no-checkout", "--no-track", "-b"]).arg(wt.branch()).arg(scratch).arg(&wt.base), "worktree add")?;
    let out = Command::new(btrfs).args(["subvolume", "snapshot"]).arg(&template.path).arg(&wt.path).stdin(Stdio::null()).output().map_err(|e| Error::Failed(format!("btrfs subvolume snapshot: {e}")))?;
    if !out.status.success() {
        return Err(Error::Failed(format!("btrfs subvolume snapshot: {}", last_lines(&String::from_utf8_lossy(&out.stderr)))));
    }
    // Moved, as `mv` moves across subvolumes, which `rename` cannot: it is
    // the one-line file naming the worktree's admin directory.
    std::fs::copy(scratch.join(".git"), wt.path.join(".git")).and_then(|_| std::fs::remove_file(scratch.join(".git"))).map_err(|e| Error::Failed(format!("move the worktree's .git: {e}")))?;
    std::fs::remove_dir(scratch).map_err(|e| Error::Failed(format!("remove {}: {e}", scratch.display())))?;
    read(super::git(&wt.path)?.args(["worktree", "repair"]), "worktree repair")?;
    // The template's index holds `base`'s tree with the stat data of the
    // files the snapshot shares, so the refresh hashes almost nothing.
    // Checked, and read from the commit when it is not that tree.
    let index = PathBuf::from(read(query(&wt.path)?.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]), "rev-parse")?);
    let part = index.with_extension(format!("krowk-{}", wt.hex));
    let copied = super::copy_index(&Paths::new(wt.repo_dir()).index, &part).and_then(|()| std::fs::rename(&part, &index));
    let _ = std::fs::remove_file(&part);
    let tree = read(query(&wt.path)?.args(["rev-parse", &format!("{}^{{tree}}", wt.base)]), "rev-parse")?;
    if copied.is_err() || read(super::git(&wt.path)?.arg("write-tree"), "write-tree").ok().as_deref() != Some(tree.as_str()) {
        read(super::git(&wt.path)?.args(["read-tree", &wt.base]), "read-tree")?;
    }
    let _ = read(super::git(&wt.path)?.args(["update-index", "-q", "--refresh"]), "update-index");
    // A template a stray file got into would make the worktree look
    // changed, and keep it.
    let status = read(query(&wt.path)?.args(["status", "--porcelain", "--untracked-files=normal"]), "status")?;
    if !status.is_empty() {
        return Err(Error::Failed(format!("the snapshot differs from {}: {}", wt.base, last_lines(&status))));
    }
    #[cfg(test)]
    if FAIL_AFTER_SNAPSHOT.with(|f| f.replace(false)) {
        return Err(Error::Failed("failed by the test".into()));
    }
    Ok(())
}

/// What `snapshot_steps` made of `wt`, gone: git's record of it (wherever
/// its `.git` was), the snapshot, the scratch directory, its branch.
fn take_back(wt: &Worktree, scratch: &Path) {
    for at in [wt.path.as_path(), scratch] {
        if at.join(".git").symlink_metadata().is_ok() && let Ok(mut c) = super::git(&wt.main) {
            let _ = read(c.args(["worktree", "remove", "--force", "--force"]).arg(at), "worktree remove");
        }
    }
    for at in [wt.path.as_path(), scratch] {
        if at.symlink_metadata().is_ok() {
            let _ = std::fs::remove_dir_all(at);
        }
        if let Some(admin) = super::manage::registration(&wt.common, at) {
            let _ = std::fs::remove_dir_all(admin);
        }
    }
    if let Ok(mut c) = super::git(&wt.main) {
        let _ = read(c.args(["branch", "--quiet", "-D"]).arg(wt.branch()), "branch -D");
    }
}

/// The newest mtime of the directory `dir` and of what is at most two
/// levels inside it, links not followed; none when `dir` is not a real
/// directory. A build or an install changes something there (cargo's
/// `target/debug/deps`, npm's `node_modules/.package-lock.json`), and a
/// clone keeps mtimes, so a copy is as new as its original until the
/// original changes.
fn newest(dir: &Path) -> Option<SystemTime> {
    fn walk(dir: &Path, depth: usize, best: &mut SystemTime) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if let Ok(t) = m.modified() {
                *best = (*best).max(t);
            }
            if depth > 1 && m.is_dir() {
                walk(&e.path(), depth - 1, best);
            }
        }
    }
    let m = dir.symlink_metadata().ok().filter(|m| m.is_dir())?;
    let mut best = m.modified().ok()?;
    walk(dir, 2, &mut best);
    Some(best)
}

/// The template, its index (and a lock a git cut short left) and its
/// commit, gone. A subvolume its owner made is removed like a directory.
fn discard(t: &Paths) {
    let _ = std::fs::remove_file(&t.sha);
    let _ = std::fs::remove_dir_all(&t.dir);
    let _ = std::fs::remove_file(&t.seeding);
    let _ = std::fs::remove_file(&t.index);
    let _ = std::fs::remove_file(t.index.with_file_name(format!("{INDEX_FILE}.lock")));
}

/// `path` gone, or never there.
fn remove(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::Failed(format!("remove {}: {e}", path.display()))),
        _ => Ok(()),
    }
}

/// Whether `dir` is on btrfs.
#[cfg(target_os = "linux")]
fn on_btrfs(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: statfs writes the struct it is given, zeroed here, and reads
    // a NUL-terminated path.
    let s = unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut s) != 0 {
            return false;
        }
        s
    };
    // BTRFS_SUPER_MAGIC, whose type differs between libcs.
    s.f_type as u32 == 0x9123_683e
}

#[cfg(not(target_os = "linux"))]
fn on_btrfs(_: &Path) -> bool {
    false
}

/// The executable `name` in the PATH `path`, as a shell would find it.
fn on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?).filter(|d| d.is_absolute()).map(|d| d.join(name)).find(|p| executable(p))
}

#[cfg(unix)]
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_: &Path) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::{git_ok, has_git, repo, repo_in};
    use super::super::{Finished, Made, create_fastest, finish, lock, repo_id};
    use super::*;
    use std::collections::BTreeMap;

    /// A repository and its directory under the root, with an ignored
    /// `target` built in the main checkout.
    fn fixture(name: &str, within: Option<&Path>) -> Option<(PathBuf, PathBuf, PathBuf, PathBuf)> {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return None;
        }
        let (base, main, root) = match within {
            Some(dir) => repo_in(dir, name),
            None => repo(name),
        };
        std::fs::write(main.join(".gitignore"), "/target\n/node_modules\n").unwrap();
        std::fs::create_dir_all(main.join("target/debug/deps")).unwrap();
        std::fs::write(main.join("target/debug/deps/out"), "built\n").unwrap();
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-q", "-m", "ignore"]);
        let common = main.join(".git").canonicalize().unwrap();
        let dir = root.join(repo_id(&common));
        Some((base, main, common, dir))
    }

    fn ensured(dir: &Path, common: &Path, main: &Path, sha: &str) -> Ensured {
        let held = lock(common).unwrap();
        ensure_template(&held, dir, common, main, sha, &WorktreesConfig::default()).unwrap()
    }

    /// The template, or None (and a note) where it is unavailable.
    fn ready(dir: &Path, common: &Path, main: &Path, sha: &str) -> Option<Template> {
        match ensured(dir, common, main, sha) {
            Ensured::Ready(t) => Some(t),
            Ensured::Unavailable(why) => {
                eprintln!("no template here ({why}): skipping");
                None
            }
        }
    }

    /// Every entry under `dir` but the top-level `skip`: a file's bytes and
    /// whether it is executable, a link's target, a directory.
    fn tree(dir: &Path, skip: &[&str]) -> BTreeMap<String, String> {
        use std::os::unix::fs::PermissionsExt;
        fn walk(top: &Path, dir: &Path, skip: &[&str], out: &mut BTreeMap<String, String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let rel = e.path().strip_prefix(top).unwrap().to_string_lossy().into_owned();
                if skip.contains(&rel.as_str()) {
                    continue;
                }
                let m = e.path().symlink_metadata().unwrap();
                let what = if m.is_symlink() {
                    format!("link {}", std::fs::read_link(e.path()).unwrap().display())
                } else if m.is_dir() {
                    walk(top, &e.path(), skip, out);
                    "dir".into()
                } else {
                    format!("file {:o} {}", m.permissions().mode() & 0o111, String::from_utf8_lossy(&std::fs::read(e.path()).unwrap()))
                };
                out.insert(rel, what);
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, skip, &mut out);
        out
    }

    /// Each entry's inode, mtime and ctime: what touching it changes.
    fn stamps(dir: &Path) -> BTreeMap<PathBuf, (u64, i64, i64, i64, i64)> {
        use std::os::unix::fs::MetadataExt;
        let mut out = BTreeMap::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            let m = d.symlink_metadata().unwrap();
            out.insert(d.clone(), (m.ino(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()));
            if m.is_dir() {
                todo.extend(std::fs::read_dir(&d).unwrap().flatten().map(|e| e.path()));
            }
        }
        out
    }

    /// (btrfs) A template made at A, then brought to B, which adds,
    /// modifies and deletes files: the template is exactly `git archive B`,
    /// with the main checkout's `target` beside it.
    #[test]
    fn wt10_a_refreshed_template_is_the_new_commit() {
        let Some((base, main, common, dir)) = fixture("wt10-refresh", None) else { return };
        std::fs::create_dir_all(main.join("dir/deep")).unwrap();
        std::fs::write(main.join("dir/deep/gone.txt"), "gone\n").unwrap();
        std::fs::write(main.join("keep.txt"), "one\n").unwrap();
        std::fs::write(main.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink("keep.txt", main.join("link")).unwrap();
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-q", "-m", "A"]);
        let a = git_ok(&main, &["rev-parse", "HEAD"]);
        let Some(t) = ready(&dir, &common, &main, &a) else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        assert_eq!(t.path, dir.join(TEMPLATE_DIR));
        assert_eq!(std::fs::read_to_string(t.path.join("keep.txt")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(t.path.join("target/debug/deps/out")).unwrap(), "built\n", "target seeded");
        assert_eq!(std::fs::read_to_string(dir.join(SHA_FILE)).unwrap().trim(), a);
        // A subvolume: its root's inode is btrfs's first.
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&t.path.metadata().unwrap()), 256);

        std::fs::remove_dir_all(main.join("dir")).unwrap();
        std::fs::write(main.join("keep.txt"), "two\n").unwrap();
        std::fs::set_permissions(main.join("run.sh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(main.join("new")).unwrap();
        std::fs::write(main.join("new/added.txt"), "added\n").unwrap();
        git_ok(&main, &["add", "-A"]);
        git_ok(&main, &["commit", "-q", "-m", "B"]);
        let b = git_ok(&main, &["rev-parse", "HEAD"]);
        let t = ready(&dir, &common, &main, &b).unwrap();
        assert_eq!(t.sha, b);
        let expected = base.join("archive");
        std::fs::create_dir_all(&expected).unwrap();
        let tar = base.join("b.tar");
        git_ok(&main, &["archive", "-o", tar.to_str().unwrap(), &b]);
        assert!(Command::new("tar").arg("-xf").arg(&tar).arg("-C").arg(&expected).status().unwrap().success());
        assert_eq!(tree(&t.path, &["target", "node_modules"]), tree(&expected, &[]));
        assert!(!t.path.join("dir").exists() && !t.path.join(".git").exists());
        assert_eq!(std::fs::read_to_string(dir.join(SHA_FILE)).unwrap().trim(), b);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) Asked for the commit it holds, the template is left as it
    /// is: not a file, its index or its record touched.
    #[test]
    fn wt10_the_same_commit_touches_nothing() {
        let Some((base, main, common, dir)) = fixture("wt10-same", None) else { return };
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let Some(t) = ready(&dir, &common, &main, &head) else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let before = (stamps(&t.path), stamps(&dir.join(INDEX_FILE)), stamps(&dir.join(SHA_FILE)));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(ready(&dir, &common, &main, &head).unwrap(), t);
        assert_eq!((stamps(&t.path), stamps(&dir.join(INDEX_FILE)), stamps(&dir.join(SHA_FILE))), before);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) A template no record vouches for (a refresh cut short) is
    /// made again; the main checkout's newer build is cloned in, an
    /// unchanged one is not.
    #[test]
    fn wt10_an_unknown_template_is_remade_and_a_newer_build_reseeded() {
        let Some((base, main, common, dir)) = fixture("wt10-remake", None) else { return };
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let Some(t) = ready(&dir, &common, &main, &head) else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        std::fs::write(t.path.join("stray"), "x").unwrap();
        std::fs::remove_file(dir.join(SHA_FILE)).unwrap();
        let t = ready(&dir, &common, &main, &head).unwrap();
        assert!(!t.path.join("stray").exists(), "made again");
        assert_eq!(std::fs::read_to_string(dir.join(SHA_FILE)).unwrap().trim(), head);

        // A refresh that cannot apply (an index that is not git's) is made
        // again from nothing too.
        std::fs::write(t.path.join("stray"), "x").unwrap();
        std::fs::write(dir.join(INDEX_FILE), "garbage").unwrap();
        std::fs::write(main.join("b.txt"), "b\n").unwrap();
        git_ok(&main, &["add", "b.txt"]);
        git_ok(&main, &["commit", "-q", "-m", "b"]);
        let next = git_ok(&main, &["rev-parse", "HEAD"]);
        let t = ready(&dir, &common, &main, &next).unwrap();
        assert!(!t.path.join("stray").exists() && t.path.join("b.txt").is_file());

        // The main checkout's build moved on: cloned in again at the next
        // refresh. Unchanged: the template's copy is kept.
        let kept = stamps(&t.path.join("target"));
        std::fs::write(main.join("c.txt"), "c\n").unwrap();
        git_ok(&main, &["add", "c.txt"]);
        git_ok(&main, &["commit", "-q", "-m", "c"]);
        let c = git_ok(&main, &["rev-parse", "HEAD"]);
        let t = ready(&dir, &common, &main, &c).unwrap();
        assert_eq!(stamps(&t.path.join("target")), kept, "an unchanged build is not cloned again");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(main.join("target/debug/deps/next"), "rebuilt\n").unwrap();
        git_ok(&main, &["commit", "-q", "--allow-empty", "-m", "d"]);
        let d = git_ok(&main, &["rev-parse", "HEAD"]);
        let t = ready(&dir, &common, &main, &d).unwrap();
        assert_eq!(std::fs::read_to_string(t.path.join("target/debug/deps/next")).unwrap(), "rebuilt\n");
        assert!(std::fs::read_dir(&t.path).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".krowk-old")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A worktree made the fastest way; None (and a note) where that is
    /// not a snapshot here.
    fn snapshot_of(main: &Path, root: &Path, owner: &str) -> Option<(Worktree, super::super::manage::Held, bool)> {
        match create_fastest(main, root, owner, &WorktreesConfig::default()).unwrap() {
            (w, held, Made::Snapshot { seeded }) => Some((w, held, seeded)),
            (w, _, Made::Checkout) => {
                eprintln!("no snapshots here: skipping");
                let _ = finish(&w);
                None
            }
        }
    }

    /// What krowk and git hold of worktrees in `dir` and `common`: the
    /// entries of `dir` that are not files, the admin directories, the
    /// `krowk/` branches.
    fn leftovers(dir: &Path, common: &Path, main: &Path) -> (Vec<String>, Vec<String>, String) {
        let names = |d: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d).map(|r| r.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
            v.sort();
            v
        };
        (names(dir), names(&common.join("worktrees")), git_ok(main, &["branch", "--list", "krowk/*"]))
    }

    /// (btrfs) A worktree made from the template: clean, at its base, with
    /// the main checkout's `target`; a commit in it is the repository's;
    /// removed through `finish`, nothing of it is left. From a checkout
    /// with uncommitted changes too, whose working state the template
    /// moves to.
    #[test]
    fn wt11_a_snapshot_is_a_worktree_like_any_other() {
        let Some((base, main, common, dir)) = fixture("wt11-snap", None) else { return };
        let root = dir.parent().unwrap().to_path_buf();
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let Some((w, held, seeded)) = snapshot_of(&main, &root, "s1") else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        assert!(seeded, "the template carried target");
        let probe: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(seed::PROBE_FILE)).unwrap()).unwrap();
        assert_eq!(probe["method"], "snapshot", "chosen once, and remembered");
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&w.path.metadata().unwrap()), 256, "a subvolume");
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "");
        assert_eq!((git_ok(&w.path, &["rev-parse", "HEAD"]), w.base.clone()), (head.clone(), head.clone()));
        assert_eq!(git_ok(&w.path, &["rev-parse", "--abbrev-ref", "HEAD"]), w.branch());
        assert_eq!(std::fs::read_to_string(w.path.join("target/debug/deps/out")).unwrap(), "built\n");
        assert_eq!(std::fs::read_dir(dir.join(SCRATCH_DIR)).unwrap().count(), 0, "scratch is empty");
        assert!(git_ok(&main, &["worktree", "list", "--porcelain"]).contains(&format!("worktree {}\nHEAD {head}\nbranch refs/heads/{}\nlocked krowk:s1", w.path.display(), w.branch())));

        std::fs::write(w.path.join("a.txt"), "changed\n").unwrap();
        git_ok(&w.path, &["commit", "-q", "-am", "in the worktree"]);
        let made = git_ok(&w.path, &["rev-parse", "HEAD"]);
        assert_eq!(git_ok(&main, &["rev-parse", &w.branch()]), made, "seen from the main checkout");
        assert_eq!(git_ok(&main, &["show", &format!("{made}:a.txt")]), "changed");
        git_ok(&w.path, &["reset", "-q", "--hard", &head]);
        drop(held);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        assert!(!w.path.exists());
        assert_eq!(leftovers(&dir, &common, &main), (vec![SCRATCH_DIR.to_string(), TEMPLATE_DIR.to_string()], vec![], String::new()));
        assert!(!dir.join(format!("{}.json", w.hex)).exists() && !dir.join(format!("{}.live", w.hex)).exists());

        // Uncommitted changes: the base is their commit, and so is the
        // template.
        std::fs::write(main.join("a.txt"), "edited\n").unwrap();
        std::fs::write(main.join("new.txt"), "new\n").unwrap();
        let (w, _held, _) = snapshot_of(&main, &root, "s2").unwrap();
        assert_ne!(w.base, head);
        assert_eq!(std::fs::read_to_string(dir.join(SHA_FILE)).unwrap().trim(), w.base);
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "");
        assert_eq!(std::fs::read_to_string(w.path.join("new.txt")).unwrap(), "new\n");
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) A worktree of this repository, from a clone of it: the
    /// first makes the template, the next are snapshots of it, each well
    /// under a second.
    #[test]
    fn wt11_a_worktree_of_this_repository_takes_under_a_second() {
        let this = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        if !has_git() || !this.join(".git").exists() {
            eprintln!("not in a checkout of krowk: skipping");
            return;
        }
        let base = std::env::temp_dir().join(format!("krowk-worktree-wt11-this-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let main = base.join("main");
        git_ok(&base, &["clone", "-q", "--local", this.to_str().unwrap(), main.to_str().unwrap()]);
        let root = base.join("worktrees");
        let timed = |owner: &str| {
            let at = std::time::Instant::now();
            let made = create_fastest(&main, &root, owner, &WorktreesConfig::default()).unwrap();
            (made, at.elapsed())
        };
        let ((first, _h1, how), took) = timed("s1");
        eprintln!("the first worktree of this repository ({how:?}, the template made): {took:?}");
        if how == Made::Checkout {
            eprintln!("no snapshots here: not measured");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let mut times = Vec::new();
        for i in 0..3 {
            let ((w, _h, how), took) = timed(&format!("s{}", i + 2));
            eprintln!("a snapshot worktree of this repository ({how:?}): {took:?}");
            assert!(matches!(how, Made::Snapshot { .. }));
            assert_eq!(git_ok(&w.path, &["status", "--porcelain"]), "");
            times.push(took);
        }
        assert!(times.iter().all(|t| *t < std::time::Duration::from_secs(1)), "{times:?}");
        drop(first);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) A snapshot that fails once it is made: taken back whole, and
    /// the worktree checked out instead, with no stray directory,
    /// subvolume, registration or branch.
    #[test]
    fn wt11_a_failed_snapshot_falls_back_to_a_checkout() {
        let Some((base, main, common, dir)) = fixture("wt11-fail", None) else { return };
        let root = dir.parent().unwrap().to_path_buf();
        // The template made, and seen to work, first.
        let Some((w, held, _)) = snapshot_of(&main, &root, "s0") else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        drop(held);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        FAIL_AFTER_SNAPSHOT.with(|f| f.set(true));
        let (w, held, how) = create_fastest(&main, &root, "s1", &WorktreesConfig::default()).unwrap();
        assert!(!FAIL_AFTER_SNAPSHOT.with(|f| f.get()), "the hook fired");
        assert_eq!(how, Made::Checkout);
        assert_ne!(std::os::unix::fs::MetadataExt::ino(&w.path.metadata().unwrap()), 256, "a directory, not the snapshot");
        assert!(!w.path.join("target").exists(), "checked out, not yet seeded");
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "");
        assert_eq!(leftovers(&dir, &common, &main), (vec![w.hex.clone(), SCRATCH_DIR.to_string(), TEMPLATE_DIR.to_string()], vec![w.hex.clone()], format!("+ {}", w.branch())));
        assert_eq!(std::fs::read_dir(dir.join(SCRATCH_DIR)).unwrap().count(), 0);
        let log = std::fs::read_to_string(dir.join(seed::LOG_FILE)).unwrap();
        assert!(log.contains(&format!("{} not made as a snapshot of the template, so checked out: failed by the test", w.hex)), "{log}");
        drop(held);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        assert_eq!(leftovers(&dir, &common, &main), (vec![SCRATCH_DIR.to_string(), TEMPLATE_DIR.to_string()], vec![], String::new()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) A reseed cut short (a crash mid-clone) left its marker, the
    /// old `target` set aside and a half-made one: the next ensure, even at
    /// the same commit, makes the template again, clean.
    #[test]
    fn wt10_a_reseed_cut_short_is_remade() {
        let Some((base, main, common, dir)) = fixture("wt10-cut", None) else { return };
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let Some(t) = ready(&dir, &common, &main, &head) else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        assert!(!dir.join(SEEDING_FILE).exists(), "gone once seeded");
        std::fs::rename(t.path.join("target"), t.path.join(".krowk-old-target-0badf00d")).unwrap();
        std::fs::create_dir_all(t.path.join("target/debug")).unwrap();
        std::fs::write(dir.join(SEEDING_FILE), "").unwrap();
        let t = ready(&dir, &common, &main, &head).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&t.path).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, [".gitignore", "a.txt", "target"]);
        assert_eq!(std::fs::read_to_string(t.path.join("target/debug/deps/out")).unwrap(), "built\n");
        assert!(!dir.join(SEEDING_FILE).exists());
        assert_eq!(std::fs::read_to_string(dir.join(SHA_FILE)).unwrap().trim(), head);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Not on btrfs (tmpfs), or no `btrfs` program: unavailable, no error,
    /// nothing made.
    #[test]
    fn wt10_without_btrfs_it_is_unavailable() {
        let shm = Path::new("/dev/shm");
        if shm.is_dir() && let Some((base, main, common, dir)) = fixture("wt10-tmpfs", Some(shm)) {
            let head = git_ok(&main, &["rev-parse", "HEAD"]);
            assert_eq!(ensured(&dir, &common, &main, &head), Ensured::Unavailable(Unavailable::NotBtrfs));
            assert!(!dir.join(TEMPLATE_DIR).exists() && !dir.join(SHA_FILE).exists());
            let _ = std::fs::remove_dir_all(&base);
        }
        let Some((base, main, common, dir)) = fixture("wt10-nobin", None) else { return };
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let none = ensure(Some(OsStr::new("/nonexistent:relative/bin")), &dir, &common, &main, &head, &WorktreesConfig::default()).unwrap();
        if on_btrfs(&dir) {
            assert_eq!(none, Ensured::Unavailable(Unavailable::NoBtrfs));
        } else {
            assert_eq!(none, Ensured::Unavailable(Unavailable::NotBtrfs));
        }
        assert!(!dir.join(TEMPLATE_DIR).exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
