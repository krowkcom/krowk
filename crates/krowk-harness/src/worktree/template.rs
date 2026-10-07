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
//! - **Many at once** (WT16.1): the repository's lock (`super::lock`) is
//!   held for what changes git's records or the template, not for what
//!   happens in the new worktree alone. Ensuring the template and `worktree
//!   add` hold it; the snapshot and the copy of the template's index do
//!   not, and hold `<repo-id>/template.lock` shared instead, taken under
//!   the repository's lock, so no ensure for another commit changes the
//!   template under them: an ensure that changes it holds that lock whole,
//!   and waits for them. The worktree is held live (`manage::hold`) before
//!   the lock is let go, so a prune meanwhile does not take the snapshot,
//!   which no record names yet, for an orphan. btrfs takes snapshots at
//!   once in one transaction,
//!   so twenty creations do not take twenty transactions in turn. Moving
//!   the `.git` file and `worktree repair`, which looks at every worktree's,
//!   take the repository's lock again; the refresh of the index and the
//!   check that the worktree is clean (`settle`) run once the worktree is
//!   held, locked and recorded, with no lock. Templates are made on Linux
//!   only, where a file lock is the open file's, so threads of one process
//!   exclude each other on it as processes do.
//!
//! Nothing in it is the agent's: no sandbox writes `<root>/<repo-id>`.

use super::seed::{self, Seeded};
use super::{Error, RepoLock, Worktree, last_lines, lock, query, random_hex, read};
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
/// Held shared while a snapshot of it is taken and its index copied, whole
/// while it is changed, beside it.
pub const LOCK_FILE: &str = "template.lock";
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
    if there && held.as_deref() == Some(sha) {
        return Ok(Ensured::Ready(ready));
    }
    // Every change waits for the snapshots being taken of it.
    let _whole = t.lock(false)?;
    match held {
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

    /// `template.lock`, held `shared` or whole until dropped, waited for.
    fn lock(&self, shared: bool) -> Result<std::fs::File, Error> {
        let at = self.dir.with_file_name(LOCK_FILE);
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&at).map_err(|e| Error::Failed(format!("open {}: {e}", at.display())))?;
        if shared { file.lock_shared() } else { file.lock() }.map_err(|e| Error::Failed(format!("lock {}: {e}", at.display())))?;
        Ok(file)
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

/// Where a test makes the next snapshot fail, as any of its steps might.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailAt {
    Nowhere,
    /// Once it is made, the repository's lock let go.
    Snapshot,
    /// In `settle`, the worktree held, locked and recorded.
    Settle,
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_AT: std::cell::Cell<FailAt> = const { std::cell::Cell::new(FailAt::Nowhere) };
    /// Set by a test: run once the next snapshot is made, the repository's
    /// lock let go, as another krowk might at that moment.
    pub(super) static MEANWHILE: std::cell::Cell<Option<fn(&Worktree)>> = const { std::cell::Cell::new(None) };
}

/// Whether the test set this thread's next snapshot to fail `at` here;
/// once.
#[cfg(test)]
fn fails(at: FailAt) -> bool {
    FAIL_AT.with(|f| f.get() == at && f.replace(FailAt::Nowhere) == at)
}

/// A snapshot made into a worktree, registered with git at its path, its
/// index still to be settled (`settle`).
pub(super) struct Taken {
    /// Whether it carried the heavy directories `config` names, which its
    /// seed step then leaves alone.
    pub seeded: bool,
    /// Where git was told it was before the snapshot was there to hold it.
    pub scratch: PathBuf,
    /// The worktree's index, in its admin directory.
    index: PathBuf,
    /// The template's index copied beside it, when the copy was made.
    copied: Option<PathBuf>,
    /// The worktree held live since before the lock was let go, so no
    /// prune took its directory for an orphan; taken by the caller.
    pub hold: Option<super::manage::Held>,
}

/// The new worktree `wt` (its directory not there yet, its branch not
/// made) as a snapshot of the template at `wt.base` (see the module
/// docs), registered with git. `held` is the repository's lock: let go
/// for the snapshot, and given back held again with what was made. None
/// where there is no template, and nothing made. An error has taken back
/// everything it made: the snapshot, git's record of the worktree, its
/// branch. Only the lock not taken again is the outer error. Blocking.
pub(super) fn snapshot(held: RepoLock, wt: &Worktree, config: &WorktreesConfig) -> Result<(RepoLock, Result<Option<Taken>, Error>), Error> {
    let dir = wt.repo_dir();
    let template = match ensure_template(&held, dir, &wt.common, &wt.main, &wt.base, config) {
        Ok(Ensured::Ready(t)) => t,
        Ok(Ensured::Unavailable(_)) => return Ok((held, Ok(None))),
        Err(e) => return Ok((held, Err(e))),
    };
    let Some(btrfs) = on_path("btrfs", std::env::var_os("PATH").as_deref()) else { return Ok((held, Ok(None))) };
    let scratch = match std::path::absolute(dir.join(SCRATCH_DIR).join(&wt.hex)) {
        Ok(s) => s,
        Err(e) => return Ok((held, Err(Error::Failed(format!("the scratch directory: {e}"))))),
    };
    let mut held = Some(held);
    let mut hold = None;
    let made = snapshot_steps(&mut held, &mut hold, wt, &template, &btrfs, &scratch);
    let relocked = held.map_or_else(|| lock(&wt.common), Ok);
    // Under the lock taken again; with none (it could not be taken), as
    // well as can be: the registration is not left behind.
    if made.is_err() {
        take_back(wt, &scratch);
        drop(hold.take());
        super::manage::forget(dir, &wt.hex);
    }
    let seeded = config.seed().iter().any(|n| seed::top_level_name(n) && wt.path.join(n).symlink_metadata().is_ok_and(|m| m.is_dir()));
    Ok((relocked?, made.map(|(index, copied)| Some(Taken { seeded, scratch, index, copied, hold }))))
}

/// The steps of `snapshot`, in order; the first that fails stops them.
/// `held` is let go for the snapshot, and is taken again when they
/// succeed; `hold` is the worktree held live from before it is let go.
/// The worktree's index, and the template's copied beside it.
fn snapshot_steps(held: &mut Option<RepoLock>, hold: &mut Option<super::manage::Held>, wt: &Worktree, template: &Template, btrfs: &Path, scratch: &Path) -> Result<(PathBuf, Option<PathBuf>), Error> {
    std::fs::create_dir_all(scratch.parent().unwrap_or(scratch)).map_err(|e| Error::Failed(format!("create {}: {e}", scratch.display())))?;
    let t = Paths::new(wt.repo_dir());
    // Taken while the repository's lock is held: no ensure comes between
    // the one that made the template ready and the snapshot.
    let shared = t.lock(true)?;
    // `worktree add` takes an empty directory only, and names git's record
    // of it after its name: the worktree's own.
    read(super::git(&wt.main)?.args(["worktree", "add", "--quiet", "--no-checkout", "--no-track", "-b"]).arg(wt.branch()).arg(scratch).arg(&wt.base), "worktree add")?;
    // Until the lock is taken again, git knows the worktree only at
    // `scratch`, and its directory is a snapshot no record names: a prune
    // would take it for an orphan, unless it is held live.
    *hold = Some(super::manage::hold(wt)?);
    *held = None;
    let index = PathBuf::from(read(query(scratch)?.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]), "rev-parse")?);
    let out = Command::new(btrfs).args(["subvolume", "snapshot"]).arg(&template.path).arg(&wt.path).stdin(Stdio::null()).output().map_err(|e| Error::Failed(format!("btrfs subvolume snapshot: {e}")))?;
    if !out.status.success() {
        return Err(Error::Failed(format!("btrfs subvolume snapshot: {}", last_lines(&String::from_utf8_lossy(&out.stderr)))));
    }
    #[cfg(test)]
    if fails(FailAt::Snapshot) {
        return Err(Error::Failed("failed by the test".into()));
    }
    #[cfg(test)]
    if let Some(meanwhile) = MEANWHILE.with(|m| m.take()) {
        meanwhile(wt);
    }
    // The template's index holds `base`'s tree with the stat data of the
    // files the snapshot shares, so the refresh hashes almost nothing.
    // Copied beside the worktree's while the template cannot change.
    let part = index.with_extension(format!("krowk-{}", wt.hex));
    let copied = super::copy_index(&t.index, &part).is_ok().then_some(part);
    drop(shared);
    *held = Some(lock(&wt.common)?);
    // Moved, as `mv` moves across subvolumes, which `rename` cannot: it is
    // the one-line file naming the worktree's admin directory.
    std::fs::copy(scratch.join(".git"), wt.path.join(".git")).and_then(|_| std::fs::remove_file(scratch.join(".git"))).map_err(|e| Error::Failed(format!("move the worktree's .git: {e}")))?;
    std::fs::remove_dir(scratch).map_err(|e| Error::Failed(format!("remove {}: {e}", scratch.display())))?;
    // It writes any worktree's `.git` file it finds wrong: under the lock,
    // so never one another creation is moving.
    read(super::git(&wt.path)?.args(["worktree", "repair"]), "worktree repair")?;
    Ok((index, copied))
}

/// The snapshot `taken` made a worktree of `wt`, with no lock held: the
/// template's index put in place as its own, checked to be `base`'s tree
/// (read from the commit when it is not) and refreshed, and the worktree
/// checked to be clean. All of it is in the worktree's own directory and
/// admin directory. An error leaves the worktree for the caller to take
/// back (`take_back`), under the lock. Blocking.
pub(super) fn settle(wt: &Worktree, taken: &Taken) -> Result<(), Error> {
    let copied = taken.copied.as_ref().is_some_and(|part| {
        let moved = std::fs::rename(part, &taken.index);
        let _ = std::fs::remove_file(part);
        moved.is_ok()
    });
    let tree = read(query(&wt.path)?.args(["rev-parse", &format!("{}^{{tree}}", wt.base)]), "rev-parse")?;
    if !copied || read(super::git(&wt.path)?.arg("write-tree"), "write-tree").ok().as_deref() != Some(tree.as_str()) {
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
    if fails(FailAt::Settle) {
        return Err(Error::Failed("failed by the test".into()));
    }
    Ok(())
}

/// What `snapshot_steps` made of `wt`, gone: git's record of it (wherever
/// its `.git` was, locked or not), the snapshot, the scratch directory,
/// its branch. Under the repository's lock.
pub(super) fn take_back(wt: &Worktree, scratch: &Path) {
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

    /// (btrfs) A snapshot that fails once it is made, the repository's lock
    /// let go: taken back whole, and the worktree checked out instead, with
    /// no stray directory, subvolume, registration or branch.
    #[test]
    fn wt11_a_failed_snapshot_falls_back_to_a_checkout() {
        falls_back("wt11-fail", FailAt::Snapshot);
    }

    /// (btrfs) A snapshot that fails as it settles, once it is held, locked
    /// and recorded with no lock (WT16.1): taken back the same, its record
    /// with it, and checked out.
    #[test]
    fn wt16_a_snapshot_failing_as_it_settles_falls_back_to_a_checkout() {
        falls_back("wt16-settle", FailAt::Settle);
    }

    fn falls_back(name: &str, at: FailAt) {
        let Some((base, main, common, dir)) = fixture(name, None) else { return };
        let root = dir.parent().unwrap().to_path_buf();
        // The template made, and seen to work, first.
        let Some((w, held, _)) = snapshot_of(&main, &root, "s0") else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        drop(held);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        FAIL_AT.with(|f| f.set(at));
        let (w, held, how) = create_fastest(&main, &root, "s1", &WorktreesConfig::default()).unwrap();
        assert_eq!(FAIL_AT.with(|f| f.get()), FailAt::Nowhere, "the hook fired");
        assert_eq!(how, Made::Checkout);
        assert_ne!(std::os::unix::fs::MetadataExt::ino(&w.path.metadata().unwrap()), 256, "a directory, not the snapshot");
        assert!(!w.path.join("target").exists(), "checked out, not yet seeded");
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "");
        assert_eq!(leftovers(&dir, &common, &main), (vec![w.hex.clone(), SCRATCH_DIR.to_string(), TEMPLATE_DIR.to_string()], vec![w.hex.clone()], format!("+ {}", w.branch())));
        assert_eq!(std::fs::read_dir(dir.join(SCRATCH_DIR)).unwrap().count(), 0);
        assert!(git_ok(&main, &["worktree", "list", "--porcelain"]).contains(&format!("locked {}s1", super::super::LOCK_REASON)), "locked for the checkout's owner");
        let log = std::fs::read_to_string(dir.join(seed::LOG_FILE)).unwrap();
        assert!(log.contains(&format!("{} not made as a snapshot of the template, so checked out: failed by the test", w.hex)), "{log}");
        drop(held);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        assert_eq!(leftovers(&dir, &common, &main), (vec![SCRATCH_DIR.to_string(), TEMPLATE_DIR.to_string()], vec![], String::new()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) A prune while a snapshot is made, the repository's lock let
    /// go and git knowing the worktree only in `scratch`: the snapshot is
    /// held live, so not taken for an orphan, nor its registration for one
    /// whose directory is gone (WT16.1).
    #[test]
    fn wt16_a_prune_while_a_snapshot_is_made_leaves_it() {
        let Some((base, main, common, dir)) = fixture("wt16-prune", None) else { return };
        let root = dir.parent().unwrap().to_path_buf();
        let Some((w, held, _)) = snapshot_of(&main, &root, "s0") else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        drop(held);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        fn prune_now(wt: &Worktree) {
            assert!(wt.path.is_dir(), "the snapshot is there");
            let done = super::super::manage::prune(wt.repo_dir().parent().unwrap(), SystemTime::now());
            assert!(done.dirs.is_empty() && done.registrations.is_empty(), "{done:?}");
        }
        MEANWHILE.with(|m| m.set(Some(prune_now)));
        let (w, held, how) = create_fastest(&main, &root, "s1", &WorktreesConfig::default()).unwrap();
        assert!(MEANWHILE.with(|m| m.get()).is_none(), "the hook ran");
        assert!(matches!(how, Made::Snapshot { .. }), "{how:?}");
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "");
        assert_eq!(leftovers(&dir, &common, &main), (vec![w.hex.clone(), SCRATCH_DIR.to_string(), TEMPLATE_DIR.to_string()], vec![w.hex.clone()], format!("+ {}", w.branch())));
        drop(held);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) A snapshot being taken holds the template: an ensure for
    /// another commit (another creation's working state, WT14) waits for
    /// it, and then brings the template to its commit (WT16.1).
    #[test]
    fn wt16_a_template_is_not_changed_while_a_snapshot_is_taken() {
        let Some((base, main, common, dir)) = fixture("wt16-hold", None) else { return };
        let first = git_ok(&main, &["rev-parse", "HEAD"]);
        if ready(&dir, &common, &main, &first).is_none() {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        std::fs::write(main.join("a.txt"), "changed\n").unwrap();
        git_ok(&main, &["commit", "-qam", "next"]);
        let next = git_ok(&main, &["rev-parse", "HEAD"]);
        let shared = Paths::new(&dir).lock(true).unwrap();
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let ensuring = s.spawn(|| {
                let t = ready(&dir, &common, &main, &next);
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                t
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "the template changed under a snapshot");
            assert_eq!(std::fs::read_to_string(dir.join(SHA_FILE)).unwrap().trim(), first);
            drop(shared);
            assert!(ensuring.join().unwrap().is_some());
        });
        assert_eq!(std::fs::read_to_string(dir.join(SHA_FILE)).unwrap().trim(), next);
        assert_eq!(std::fs::read_to_string(dir.join(TEMPLATE_DIR).join("a.txt")).unwrap(), "changed\n");
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
