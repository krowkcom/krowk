//! Kept worktrees (WT9): every krowk worktree across repositories listed,
//! one removed without losing work, and what a person's `rm -rf` left of
//! others pruned. Kept worktrees pile up — every agent that changed
//! something leaves one — and a directory deleted by hand strands git's
//! record of it. `krowk worktrees` is the command line around these; a
//! session of its own in a worktree (WT6) and applying one's changes back
//! (WT13) build on them.
//!
//! What krowk records of a worktree when it makes one (`super::create_held`),
//! in the repository's directory under the root, `<root>/<repo-id>/`, which
//! no sandbox can write (the worktree's own admin directory is the agent's
//! to write, WT4):
//!
//! - `common`: the repository's common git directory, so the repository is
//!   found again once every worktree directory of it is gone.
//! - `<hex>.json` (`Record`): its base commit — with WT14 a commit of the
//!   parent's working state, which nothing can work out later — the session
//!   that works in it, and when it was made.
//! - `<hex>.live`: locked (`File::try_lock`) by the process whose session
//!   works in it for as long as it does (`Held`). git's own lock,
//!   `krowk:<session>`, says whose it is but not whether they are alive; the
//!   OS lets go of this one when that process dies, so a crashed session's
//!   worktree is not taken for a live one.
//!
//! The three operations:
//!
//! - **`list`**: each worktree git lists under the root — branch, base,
//!   commits ahead of the base, uncommitted changes, session, age, and
//!   whether a live session holds it.
//! - **`remove`**: refused while a live session holds it, and, unless
//!   forced, while it has uncommitted changes or commits ahead of its base.
//!   A forced removal of one with uncommitted changes saves them first:
//!   every change, untracked files included, staged into an index of
//!   krowk's (never the worktree's own), `git stash create`d, and kept as
//!   `refs/krowk/snapshots/<hex>`. Its branch is always kept, so its
//!   commits are too, and a HEAD its branch does not hold (an agent that
//!   detached it, or switched branch, and committed) is kept as
//!   `refs/krowk/snapshots/<hex>-head`. Ignored files — build output, the
//!   copies `.worktreeinclude` made — are not changes, and go with it.
//! - **`prune`**: per repository, and only while its git directory is
//!   where it was recorded (a repository moved, or on a drive not mounted,
//!   is left alone and reported): git's record of each krowk worktree
//!   whose directory is gone and which no live session holds is cleared —
//!   its one admin directory, never a blanket `git worktree prune`, which
//!   would clear the person's own worktrees on a drive not mounted, their
//!   index, HEAD and reflog with them. Then each worktree directory under
//!   the root that git does not list and whose admin directory is gone is
//!   deleted, when no live session holds it and its files are its
//!   branch's (`unchanged`) — never through a symlink, and never anything
//!   else in the repository's directory (the seeding probe and log, a
//!   WT10 template). Then the `refs/krowk/snapshots/*` whose commit is
//!   over `SNAPSHOT_DAYS` old. `prune_daily` runs it in the background at
//!   most once a day, when a session starts.
//!
//! Every change holds the repository's lock (`super::lock`), as creating
//! and finishing do.

use super::{changes, copy_index, git, has_submodules, lock, query, random_hex, read, repo_id, submodule_changes, Error, Scratch, Worktree, BRANCH_PREFIX, LOCK_REASON};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The file in a repository's directory under the root naming its common
/// git directory.
pub const COMMON_FILE: &str = "common";
/// Where a removed worktree's uncommitted changes are kept, `<hex>` after
/// it; WT13 keeps a finished child's final state there too.
pub const SNAPSHOTS: &str = "refs/krowk/snapshots/";
/// How old a snapshot's commit is before `prune` deletes it.
pub const SNAPSHOT_DAYS: u64 = 30;
/// The file in the root `prune_daily` keeps the last prune's time in.
pub const STAMP_FILE: &str = ".pruned";
/// How often `prune_daily` prunes.
const DAILY: Duration = Duration::from_secs(24 * 60 * 60);

/// What krowk keeps of a worktree it made, `<root>/<repo-id>/<hex>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The commit it was made at (`Worktree::base`).
    pub base: String,
    /// The session that works in it.
    pub session: String,
    /// When it was made, in milliseconds since the epoch.
    pub created_ms: i64,
}

/// `<hex>.json` in the repository's directory `dir`.
fn record_path(dir: &Path, hex: &str) -> PathBuf {
    dir.join(format!("{hex}.json"))
}

/// `<hex>.live` in the repository's directory `dir`.
fn live_path(dir: &Path, hex: &str) -> PathBuf {
    dir.join(format!("{hex}.live"))
}

/// Records `wt`, made for `owner`, now: written whole, then renamed in.
pub(super) fn write_record(wt: &Worktree, owner: &str) -> Result<(), Error> {
    let record = Record { base: wt.base.clone(), session: owner.to_string(), created_ms: now_ms(SystemTime::now()) };
    let at = record_path(wt.repo_dir(), &wt.hex);
    let part = wt.repo_dir().join(format!(".{}.json.{}", wt.hex, random_hex()));
    let body = serde_json::to_vec(&record).map_err(|e| Error::Failed(format!("the worktree's record: {e}")))?;
    std::fs::write(&part, body).and_then(|()| std::fs::rename(&part, &at)).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        Error::Failed(format!("write {}: {e}", at.display()))
    })
}

/// What was recorded of the worktree `hex` in the repository's directory
/// `dir`: none for one made before krowk recorded them, or a record that
/// does not read.
pub fn read_record(dir: &Path, hex: &str) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(record_path(dir, hex)).ok()?).ok()
}

/// Notes `common` in the repository's directory `dir`, unless it is there
/// already. A path that is not UTF-8 is not noted: the repository is then
/// found from its worktrees' `.git` files while any is left.
pub(super) fn note_common(dir: &Path, common: &Path) {
    let Some(text) = common.to_str() else { return };
    let at = dir.join(COMMON_FILE);
    if std::fs::read_to_string(&at).is_ok_and(|t| t == text) {
        return;
    }
    let part = dir.join(format!(".{COMMON_FILE}.{}", random_hex()));
    if std::fs::write(&part, text).and_then(|()| std::fs::rename(&part, &at)).is_err() {
        let _ = std::fs::remove_file(&part);
    }
}

/// The record files of the worktree `hex` gone, its directory having gone.
pub(super) fn forget(dir: &Path, hex: &str) {
    let _ = std::fs::remove_file(record_path(dir, hex));
    let _ = std::fs::remove_file(live_path(dir, hex));
}

/// A worktree held live: its `.live` file locked by this process until
/// this is dropped, or the process dies.
#[derive(Debug)]
pub struct Held {
    _file: File,
}

/// Holds `wt` live for the session about to work in it. Waits for a
/// listing that is looking at it (`live`), which is brief; nobody else
/// holds a worktree this new.
pub fn hold(wt: &Worktree) -> Result<Held, Error> {
    let at = live_path(wt.repo_dir(), &wt.hex);
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&at).map_err(|e| Error::Failed(format!("open {}: {e}", at.display())))?;
    file.lock().map_err(|e| Error::Failed(format!("lock {}: {e}", at.display())))?;
    Ok(Held { _file: file })
}

/// Holds `wt` live again, for a session resumed in it (WT6): none when a
/// live session holds it already. Not waited for, as `hold` is: that
/// session may run for days.
pub fn hold_again(wt: &Worktree) -> Result<Option<Held>, Error> {
    match claim(wt.repo_dir(), &wt.hex) {
        Claim::Live => Ok(None),
        Claim::Free(Some(held)) => Ok(Some(held)),
        // No `.live` yet: one made before krowk held them.
        Claim::Free(None) => hold(wt).map(Some),
    }
}

/// Whether a live session holds the worktree `hex` of the repository whose
/// directory is `dir`, asked without waiting: a shared lock, so listings at
/// once do not take each other for a session.
fn live(dir: &Path, hex: &str) -> bool {
    let Ok(file) = File::open(live_path(dir, hex)) else { return false };
    matches!(file.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock))
}

/// The worktree `hex` of the repository whose directory is `dir`, claimed
/// for a change to it: `Live` when a session holds it, else the lock,
/// held so that none takes it while the caller works. A listing looking
/// at it (`live`) holds it for a moment, so a refusal is asked again a few
/// times first.
enum Claim {
    Live,
    Free(Option<Held>),
}

fn claim(dir: &Path, hex: &str) -> Claim {
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(live_path(dir, hex)) else {
        return Claim::Free(None);
    };
    for _ in 0..5 {
        match file.try_lock() {
            Ok(()) => return Claim::Free(Some(Held { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(20)),
            Err(std::fs::TryLockError::Error(_)) => return Claim::Free(None),
        }
    }
    Claim::Live
}

/// One krowk worktree, as `list` finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Its directory, `<root>/<repo-id>/<hex>`; it may be gone (`missing`).
    pub path: PathBuf,
    pub hex: String,
    pub repo_id: String,
    /// The repository's common git directory, canonical.
    pub common: PathBuf,
    /// The repository's main checkout.
    pub main: PathBuf,
    /// The branch checked out there, its short name: none when detached.
    pub branch: Option<String>,
    /// The commit checked out there.
    pub head: Option<String>,
    /// The commit it was made at, as recorded: none for one made before
    /// krowk recorded it.
    pub base: Option<String>,
    /// Commits from `base` to `head`: none when either is unknown.
    pub ahead: Option<usize>,
    /// Whether it has uncommitted changes, untracked files and submodules'
    /// own changes included: none when its directory is gone, or `status`
    /// failed there.
    pub dirty: Option<bool>,
    /// The session that works in it: the record's, else git's lock reason.
    pub session: Option<String>,
    /// When it was made, in milliseconds since the epoch: the record's,
    /// else its directory's.
    pub created_ms: Option<i64>,
    /// Whether a live session holds it.
    pub live: bool,
    /// Whether its directory is gone, git's record of it left.
    pub missing: bool,
}

impl Listed {
    /// The worktree as `super` works with it; none when its base is not
    /// known, which everything that judges it needs.
    pub fn worktree(&self) -> Option<Worktree> {
        Some(Worktree { path: self.path.clone(), hex: self.hex.clone(), base: self.base.clone()?, common: self.common.clone(), main: self.main.clone(), repo_id: self.repo_id.clone() })
    }

    /// `krowk/<hex>`, the branch it was made on, whatever is checked out now.
    pub fn own_branch(&self) -> String {
        format!("{BRANCH_PREFIX}{}", self.hex)
    }

    /// Whether removing it would lose something a branch does not keep, or
    /// leave commits only that branch has: uncommitted changes, commits
    /// ahead of the base, or either unknown.
    pub fn changed(&self) -> bool {
        (self.dirty != Some(false) && !self.missing) || self.ahead != Some(0)
    }
}

/// One repository krowk made worktrees of: its directory under the root,
/// and its common git directory when it can be found.
struct Repo {
    id: String,
    dir: PathBuf,
    common: Option<PathBuf>,
}

/// Whether `name` is `len` lowercase hex digits: a repository's directory
/// (16) or a worktree's (8). Nothing else under the root is either.
fn hex_name(name: &str, len: usize) -> bool {
    name.len() == len && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The real directories in `dir` named `len` hex digits, sorted: a symlink
/// is not one.
fn hex_dirs(dir: &Path, len: usize) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
        .filter(|(n, _)| hex_name(n, len))
        .collect();
    out.sort();
    out
}

/// The repositories under `root`. Each one's common directory is the one
/// `COMMON_FILE` names, else the one a worktree's `.git` file points into,
/// and only when it is still the directory the repo-id was made from.
fn repos(root: &Path) -> Vec<Repo> {
    hex_dirs(root, 16)
        .into_iter()
        .map(|(id, dir)| {
            let ours = |c: PathBuf| c.canonicalize().ok().filter(|c| repo_id(c) == id);
            let noted = std::fs::read_to_string(dir.join(COMMON_FILE)).ok().map(PathBuf::from).and_then(ours);
            let common = noted.or_else(|| hex_dirs(&dir, 8).into_iter().find_map(|(_, w)| ours(admin_dir(&w)?.parent()?.parent()?.to_path_buf())));
            Repo { id, dir, common }
        })
        .collect()
}

/// The admin directory the `.git` file of the worktree directory `dir`
/// points to, whether or not it is there: none when `.git` is not a file
/// (missing, a directory, a symlink) or does not say.
fn admin_dir(dir: &Path) -> Option<PathBuf> {
    let at = dir.join(".git");
    if !at.symlink_metadata().ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(at).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    Some(if gitdir.is_absolute() { gitdir } else { dir.join(gitdir) })
}

/// One entry of `git worktree list --porcelain -z`.
#[derive(Debug, Default)]
struct Entry {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    /// Its lock reason, empty for a lock without one.
    locked: Option<String>,
}

/// Every worktree git lists for the repository whose common directory is
/// `common`, the main checkout first.
fn entries(common: &Path) -> Result<Vec<Entry>, Error> {
    let out = query(common)?.args(["worktree", "list", "--porcelain", "-z"]).output().map_err(|e| Error::Failed(format!("git worktree list: {e}")))?;
    if !out.status.success() {
        return Err(Error::Failed(format!("git worktree list: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let mut all = Vec::new();
    let mut cur: Option<Entry> = None;
    for field in String::from_utf8_lossy(&out.stdout).split('\0') {
        if field.is_empty() {
            all.extend(cur.take());
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        match key {
            "worktree" => {
                all.extend(cur.take());
                cur = Some(Entry { path: PathBuf::from(value), ..Entry::default() });
            }
            "HEAD" => cur.iter_mut().for_each(|e| e.head = Some(value.to_string())),
            "branch" => cur.iter_mut().for_each(|e| e.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).to_string())),
            "locked" => cur.iter_mut().for_each(|e| e.locked = Some(value.to_string())),
            _ => {}
        }
    }
    all.extend(cur);
    Ok(all)
}

/// The entries of `repo`'s own worktrees under the root, by hex: those
/// whose directory is a hex name in the repository's directory.
fn ours<'a>(repo: &Repo, all: &'a [Entry]) -> Vec<(String, &'a Entry)> {
    let dir = repo.dir.canonicalize().unwrap_or_else(|_| repo.dir.clone());
    all.iter()
        .filter_map(|e| {
            let name = e.path.file_name()?.to_str()?;
            let parent = e.path.parent()?;
            let same = parent == repo.dir || parent.canonicalize().is_ok_and(|p| p == dir);
            (hex_name(name, 8) && same).then(|| (name.to_string(), e))
        })
        .collect()
}

/// Every krowk worktree under `root`, by repository then age. `judge` for
/// what takes a git call in each (`ahead`, `dirty`); without it those are
/// none. A repository git cannot read is left out: its worktrees are
/// `prune`'s.
pub fn list(root: &Path) -> Vec<Listed> {
    listed(root, true)
}

fn listed(root: &Path, judge: bool) -> Vec<Listed> {
    let mut out = Vec::new();
    for repo in repos(root) {
        let Some(common) = &repo.common else { continue };
        let Ok(all) = entries(common) else { continue };
        let main = all.first().map(|e| e.path.clone()).unwrap_or_else(|| common.clone());
        let mut found: Vec<Listed> = ours(&repo, &all).into_iter().map(|(hex, e)| describe(&repo, common, &main, &hex, e, judge)).collect();
        found.sort_by_key(|l| (l.created_ms, l.hex.clone()));
        out.extend(found);
    }
    out
}

/// One worktree of `repo` as git lists it (`e`), with what krowk recorded.
fn describe(repo: &Repo, common: &Path, main: &Path, hex: &str, e: &Entry, judge: bool) -> Listed {
    let path = repo.dir.join(hex);
    let record = read_record(&repo.dir, hex);
    let missing = path.symlink_metadata().map_or(true, |m| !m.is_dir());
    let created_ms = record.as_ref().map(|r| r.created_ms).or_else(|| path.symlink_metadata().and_then(|m| m.modified()).ok().map(now_ms));
    let session = record.as_ref().map(|r| r.session.clone()).or_else(|| e.locked.as_deref()?.strip_prefix(LOCK_REASON).map(String::from));
    let live = live(&repo.dir, hex);
    let mut l = Listed {
        path,
        hex: hex.to_string(),
        repo_id: repo.id.clone(),
        common: common.to_path_buf(),
        main: main.to_path_buf(),
        branch: e.branch.clone(),
        head: e.head.clone(),
        base: record.map(|r| r.base),
        ahead: None,
        dirty: None,
        session,
        created_ms,
        live,
        missing,
    };
    if judge {
        judged(&mut l);
    }
    l
}

/// `l.ahead`, and `l.dirty` where its directory is there, read from git.
fn judged(l: &mut Listed) {
    l.ahead = match (&l.base, &l.head) {
        (Some(base), Some(head)) => query(&l.common).ok().and_then(|mut c| read(c.args(["rev-list", "--count", &format!("{base}..{head}")]), "rev-list").ok()).and_then(|n| n.parse().ok()),
        _ => None,
    };
    // Its status needs no base.
    let wt = Worktree { path: l.path.clone(), hex: l.hex.clone(), base: String::new(), common: l.common.clone(), main: l.main.clone(), repo_id: l.repo_id.clone() };
    l.dirty = if l.missing { None } else { changes(&wt).ok().map(|(_, status)| !status.is_empty()) };
}

/// Why `remove` (or another change to one worktree) did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No krowk worktree is the one named.
    NotFound(String),
    /// More than one is: their directories.
    Ambiguous(Vec<PathBuf>),
    /// A live session holds it.
    Live(Box<Listed>),
    /// It has uncommitted changes, or commits ahead of its base, and the
    /// removal was not forced.
    Changed(Box<Listed>),
    /// git, or the file system, said no.
    Failed(Error),
}

impl From<Error> for Refusal {
    fn from(e: Error) -> Refusal {
        Refusal::Failed(e)
    }
}

/// The worktree `key` names under `root`: its 8 hex digits, its branch
/// `krowk/<hex>`, or its directory. Not judged (`Listed::ahead`, `dirty`).
pub fn find(root: &Path, key: &str) -> Result<Listed, Refusal> {
    let key = key.trim();
    let all = listed(root, false);
    let hex = key.strip_prefix(BRANCH_PREFIX).unwrap_or(key);
    let mut hits: Vec<Listed> = if hex_name(hex, 8) {
        all.into_iter().filter(|l| l.hex == hex).collect()
    } else {
        let asked = std::path::absolute(key).unwrap_or_else(|_| PathBuf::from(key));
        let asked_real = asked.canonicalize().ok();
        all.into_iter().filter(|l| l.path == asked || asked_real.is_some() && l.path.canonicalize().ok() == asked_real).collect()
    };
    match hits.len() {
        0 => Err(Refusal::NotFound(key.to_string())),
        1 => Ok(hits.remove(0)),
        _ => Err(Refusal::Ambiguous(hits.into_iter().map(|l| l.path).collect())),
    }
}

/// How `remove` left a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// It, as it was just before.
    pub listed: Listed,
    /// The ref its uncommitted changes were saved as, when it had any.
    pub snapshot: Option<String>,
    /// The ref its HEAD was kept under (`keep_head`), when that was not on
    /// its branch: commits made on a detached HEAD, or another branch.
    pub head: Option<String>,
}

impl Removed {
    /// The commands that bring it back, run anywhere: its branch checked
    /// out where it was — or, when its HEAD was elsewhere, that HEAD,
    /// detached — then its uncommitted changes applied.
    pub fn restore(&self) -> Vec<String> {
        let l = &self.listed;
        let mut out = vec![match &self.head {
            Some(h) => format!("git -C {} worktree add --detach {} {h}^", quote(&l.main), quote(&l.path)),
            None => format!("git -C {} worktree add {} {}", quote(&l.main), quote(&l.path), l.own_branch()),
        }];
        if let Some(r) = &self.snapshot {
            out.push(format!("git -C {} stash apply {r}", quote(&l.path)));
        }
        out
    }
}

/// `p` as one shell word.
fn quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+:@,".contains(&b)) {
        return s.into_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Removes the worktree `key` names (`find`), its branch kept. Refused
/// while a live session holds it; refused when it has uncommitted changes
/// or commits ahead of its base (or either cannot be told) unless `force`.
/// A forced one with uncommitted changes is snapshotted first (`snapshot`),
/// and is not removed when that fails; a HEAD off its branch is kept
/// (`keep_head`). One whose directory is gone has git's record of it, and
/// only it, cleared (`unregister`). Blocking.
pub fn remove(root: &Path, key: &str, force: bool) -> Result<Removed, Refusal> {
    let found = find(root, key)?;
    let _locked = lock(&found.common)?;
    let dir = found.path.parent().unwrap_or(&found.path).to_path_buf();
    let _claimed = match claim(&dir, &found.hex) {
        Claim::Live => return Err(Refusal::Live(Box::new(found))),
        Claim::Free(held) => held,
    };
    let mut l = found;
    judged(&mut l);
    if !l.missing && l.dirty.is_none() {
        return Err(Refusal::Failed(Error::Failed(format!("git could not read {}'s status, so nothing was removed", l.path.display()))));
    }
    // One whose directory is gone loses nothing more: its commits are on
    // its branch, which stays.
    if !force && !l.missing && l.changed() {
        return Err(Refusal::Changed(Box::new(l)));
    }
    let head = keep_head(&l.common, &l.hex, l.head.as_deref())?;
    if l.missing {
        unregister(&l.common, &l.path)?;
        forget(&dir, &l.hex);
        return Ok(Removed { listed: l, snapshot: None, head });
    }
    let snapshot = match l.dirty {
        Some(true) => Some(snapshot(&l, &dir)?),
        _ => None,
    };
    let _ = read(git(&l.main)?.args(["worktree", "unlock"]).arg(&l.path), "worktree unlock");
    read(git(&l.main)?.args(["-c", "status.showUntrackedFiles=normal", "worktree", "remove", "--force", "--force"]).arg(&l.path), "worktree remove")?;
    forget(&dir, &l.hex);
    Ok(Removed { listed: l, snapshot, head })
}

/// Keeps `head`, a krowk worktree's HEAD, as `refs/krowk/snapshots/<hex>-head`
/// when its branch `krowk/<hex>` does not hold it — an agent that detached
/// HEAD, or switched branch, and committed — and names that ref: what is
/// kept is a commit of krowk's made now, whose parent is `head` and whose
/// tree is `head`'s, so it expires 30 days from now like any snapshot
/// whatever `head`'s own date. None when the branch holds it, or there is
/// no HEAD.
fn keep_head(common: &Path, hex: &str, head: Option<&str>) -> Result<Option<String>, Error> {
    let Some(head) = head.filter(|h| !h.is_empty()) else { return Ok(None) };
    let branch = format!("refs/heads/{BRANCH_PREFIX}{hex}");
    let held = query(common)?.args(["merge-base", "--is-ancestor", head, &branch]).output().is_ok_and(|o| o.status.success());
    if held {
        return Ok(None);
    }
    let message = format!("krowk: HEAD of worktree {hex} when it was removed");
    let tree = format!("{head}^{{tree}}");
    let commit = |identity: &[&str]| read(git(common)?.args(identity).args(["commit-tree", &tree, "-p", head, "-m", &message]), "commit-tree");
    let kept = commit(&[]).or_else(|_| commit(&["-c", "user.name=krowk", "-c", "user.email=krowk@localhost"]))?;
    let name = format!("{SNAPSHOTS}{hex}-head");
    read(git(common)?.args(["update-ref", "-m", &message, &name, &kept]), "update-ref")?;
    Ok(Some(name))
}

/// git's record of the worktree whose directory was `path`, cleared: the
/// one admin directory in `<common>/worktrees/` whose `gitdir` names it,
/// deleted, its lock with it. Never `git worktree prune`, which would
/// clear the person's own worktrees whose directory is away too (on a
/// drive not mounted), and their index, HEAD and reflog with them.
fn unregister(common: &Path, path: &Path) -> Result<(), Error> {
    let admin = registration(common, path).ok_or_else(|| Error::Failed(format!("git's record of {} was not found", path.display())))?;
    std::fs::remove_dir_all(&admin).map_err(|e| Error::Failed(format!("remove {}: {e}", admin.display())))
}

/// The admin directory in `<common>/worktrees/` whose `gitdir` file names
/// `path`'s `.git`: real directories only, never through a link.
fn registration(common: &Path, path: &Path) -> Option<PathBuf> {
    let dir = common.join("worktrees");
    if !dir.symlink_metadata().ok()?.is_dir() {
        return None;
    }
    std::fs::read_dir(&dir).ok()?.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).find(|admin| {
        let named = std::fs::read_to_string(admin.join("gitdir")).ok().map(|t| PathBuf::from(t.trim()));
        named.as_deref().and_then(Path::parent).is_some_and(|p| same_path(p, path))
    })
}

/// Whether `a` and `b` name one path, either of which may be gone: equal,
/// or the same name in the same (canonical) directory.
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let real = |p: &Path| p.parent().and_then(|d| d.canonicalize().ok()).map(|d| d.join(p.file_name().unwrap_or_default()));
    real(a).is_some_and(|r| Some(r) == real(b))
}

/// Saves the uncommitted changes of `l` as `refs/krowk/snapshots/<hex>`
/// and names that ref: every change `git add --all` finds, untracked files
/// included, staged into a scratch index in `dir` (a copy of the
/// worktree's own, which is not touched), then `git stash create`d with it.
/// A submodule's own changes are no change of the worktree's to stash, so
/// a worktree with any is refused rather than removed without them.
fn snapshot(l: &Listed, dir: &Path) -> Result<String, Error> {
    if has_submodules(&l.path) && !submodule_changes(&l.path, "", 0)?.is_empty() {
        return Err(Error::Failed(format!("{} has changes inside its submodules, which a snapshot cannot hold — commit them there first; nothing was removed", l.path.display())));
    }
    let index = Scratch(std::path::absolute(dir.join(format!(".index-{}", random_hex()))).map_err(|e| Error::Failed(format!("the snapshot's index: {e}")))?);
    let staged = || git(&l.path).map(|mut c| {
        c.env("GIT_INDEX_FILE", &index.0).args(["-c", "core.splitIndex=false"]);
        c
    });
    let own = PathBuf::from(read(query(&l.path)?.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]), "rev-parse")?);
    match copy_index(&own, &index.0) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => drop(read(staged()?.args(["read-tree", "HEAD"]), "read-tree")?),
        Err(e) => return Err(Error::Failed(format!("copy {}: {e}", own.display()))),
    }
    read(staged()?.args(["add", "--all", "--", ":/"]), "add")?;
    let message = format!("krowk: snapshot of {} before it was removed", l.hex);
    // The person's identity when git has one, krowk's when it has none.
    let create = |identity: &[&str]| read(staged()?.args(identity).args(["stash", "create", &message]), "stash create");
    let commit = create(&[]).or_else(|_| create(&["-c", "user.name=krowk", "-c", "user.email=krowk@localhost"]))?;
    if commit.is_empty() {
        return Err(Error::Failed(format!("git found nothing to save in {}, though its status shows changes; nothing was removed", l.path.display())));
    }
    let name = format!("{SNAPSHOTS}{}", l.hex);
    read(git(&l.path)?.args(["update-ref", "-m", &message, &name, &commit]), "update-ref")?;
    Ok(name)
}

/// What `prune` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Worktrees whose directory was gone, git's record of them cleared.
    pub registrations: Vec<PathBuf>,
    /// Directories under the root deleted, their admin directory gone.
    pub dirs: Vec<PathBuf>,
    /// Snapshot refs deleted, older than `SNAPSHOT_DAYS`.
    pub snapshots: Vec<String>,
    /// What could not be done, one line each.
    pub failures: Vec<String>,
}

/// Prunes every repository under `root` (see the module's docs), `now`
/// being the time snapshots are judged against. Blocking.
pub fn prune(root: &Path, now: SystemTime) -> Pruned {
    let mut done = Pruned::default();
    for repo in repos(root) {
        if let Err(e) = prune_repo(&repo, now, &mut done) {
            done.failures.push(format!("{}: {e}", repo.dir.display()));
        }
    }
    done
}

fn prune_repo(repo: &Repo, now: SystemTime, done: &mut Pruned) -> Result<(), Error> {
    // A repository whose git directory is not where it was — moved, or on
    // a drive not mounted — cannot say which of its worktrees are gone:
    // all of them are left alone.
    let Some(common) = repo.common.as_ref().filter(|c| c.is_dir()) else {
        let noted = std::fs::read_to_string(repo.dir.join(COMMON_FILE)).unwrap_or_else(|_| "unknown".into());
        done.failures.push(format!("{}: its repository's git directory ({noted}) is not there, so its worktrees were left alone", repo.dir.display()));
        return Ok(());
    };
    let _locked = lock(common)?;
    // krowk's own records whose directory is gone, and which no live
    // session holds: on its `krowk/<hex>` branch, or locked by krowk.
    for (hex, e) in ours(repo, &entries(common)?) {
        let gone = repo.dir.join(&hex).symlink_metadata().is_err();
        let krowks = e.branch.as_deref() == Some(&format!("{BRANCH_PREFIX}{hex}")) || e.locked.as_deref().is_some_and(|r| r.starts_with(LOCK_REASON));
        if !gone || !krowks || live(&repo.dir, &hex) {
            continue;
        }
        match keep_head(common, &hex, e.head.as_deref()).and_then(|_| unregister(common, &e.path)) {
            Ok(()) => done.registrations.push(e.path.clone()),
            Err(e) => done.failures.push(e.to_string()),
        }
    }
    let registered: HashSet<String> = ours(repo, &entries(common)?).into_iter().map(|(hex, _)| hex).collect();
    expire(common, now, done)?;
    for (hex, path) in hex_dirs(&repo.dir, 8) {
        // Gone means git does not list it, and its `.git` file points at no
        // admin directory or it has none: never one whose admin directory
        // is there, or which another repository's `.git` file names.
        let orphan = !registered.contains(&hex) && admin_dir(&path).map_or_else(|| path.join(".git").symlink_metadata().is_err(), |a| a.symlink_metadata().is_err());
        if !orphan || live(&repo.dir, &hex) {
            continue;
        }
        match unchanged(repo, common, &hex, &path) {
            Ok(true) => {}
            Ok(false) => {
                done.failures.push(format!("{}: its files differ from its branch {BRANCH_PREFIX}{hex}, so it was left in place — copy out what you need, then delete it", path.display()));
                continue;
            }
            Err(e) => {
                done.failures.push(format!("{}: left in place, as krowk could not tell whether it holds changes: {e}", path.display()));
                continue;
            }
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => done.dirs.push(path),
            Err(e) => done.failures.push(format!("{}: {e}", path.display())),
        }
    }
    // Records of worktrees neither git nor the directory knows any more.
    let left: HashSet<String> = hex_dirs(&repo.dir, 8).into_iter().map(|(h, _)| h).collect();
    let Ok(files) = std::fs::read_dir(&repo.dir) else { return Ok(()) };
    for f in files.flatten() {
        let name = f.file_name().to_string_lossy().into_owned();
        let Some((hex, kind)) = name.split_once('.') else { continue };
        if !hex_name(hex, 8) || !matches!(kind, "json" | "live") || registered.contains(hex) || left.contains(hex) {
            continue;
        }
        if kind == "live" && matches!(claim(&repo.dir, hex), Claim::Live) {
            continue;
        }
        let _ = std::fs::remove_file(f.path());
    }
    Ok(())
}

/// Whether the worktree directory `path`, which git no longer knows, holds
/// exactly what its branch `krowk/<hex>` (else its recorded base) does:
/// read through a scratch index of krowk's with the repository's git
/// directory, so no modified or untracked file is deleted with it. Ignored
/// files (build output, `.worktreeinclude` copies) are not judged.
fn unchanged(repo: &Repo, common: &Path, hex: &str, path: &Path) -> Result<bool, Error> {
    let branch = format!("refs/heads/{BRANCH_PREFIX}{hex}");
    let rev = match read(query(common)?.args(["rev-parse", "--verify", "--quiet", &format!("{branch}^{{commit}}")]), "rev-parse") {
        Ok(c) => c,
        Err(_) => read_record(&repo.dir, hex).map(|r| r.base).ok_or_else(|| Error::Failed(format!("neither {BRANCH_PREFIX}{hex} nor a recorded base is there to compare it with")))?,
    };
    let index = Scratch(std::path::absolute(repo.dir.join(format!(".index-{}", random_hex()))).map_err(|e| Error::Failed(format!("the index: {e}")))?);
    let at = |c: Result<Command, Error>| c.map(|mut c| {
        c.arg(format!("--git-dir={}", common.display())).arg(format!("--work-tree={}", path.display())).env("GIT_INDEX_FILE", &index.0);
        c
    });
    read(at(git(path))?.args(["read-tree", &rev]), "read-tree")?;
    // Exits 1 when files need refreshing, which is what it is for.
    let _ = at(git(path))?.args(["update-index", "-q", "--refresh"]).output();
    let differs = at(query(path))?.args(["diff-files", "--quiet"]).output().map_err(|e| Error::Failed(format!("git diff-files: {e}")))?;
    let untracked = read(at(query(path))?.args(["ls-files", "--others", "--exclude-standard"]), "ls-files")?;
    Ok(differs.status.success() && untracked.is_empty())
}

/// Deletes the repository's snapshot refs whose commit's committer date is
/// more than `SNAPSHOT_DAYS` before `now`; each deleted only if it still
/// points where it was read to.
fn expire(common: &Path, now: SystemTime, done: &mut Pruned) -> Result<(), Error> {
    let cutoff = now_ms(now) / 1000 - (SNAPSHOT_DAYS * 24 * 60 * 60) as i64;
    let listing = read(query(common)?.args(["for-each-ref", "--format=%(refname)%00%(objectname)%00%(committerdate:unix)", SNAPSHOTS]), "for-each-ref")?;
    for line in listing.lines() {
        let mut parts = line.split('\0');
        let (Some(name), Some(oid), Some(date)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Ok(date) = date.parse::<i64>() else { continue };
        if date >= cutoff {
            continue;
        }
        match read(git(common)?.args(["update-ref", "-d", name, oid]), "update-ref") {
            Ok(_) => done.snapshots.push(name.to_string()),
            Err(e) => done.failures.push(e.to_string()),
        }
    }
    Ok(())
}

/// `prune`, in the background, when the last one under `root` was a day
/// ago or more (`prune_if_due`): what starting a session calls, which it
/// never waits for.
pub fn prune_daily(root: PathBuf) {
    let _ = std::thread::Builder::new().name("krowk-worktrees-prune".into()).spawn(move || prune_if_due(&root, SystemTime::now()));
}

/// `prune`, when the time in `<root>/.pruned` is a day or more before
/// `now`, or there is none; that time set to `now` first, under a lock on
/// the file, so krowks starting at once prune once. Nothing when the root does not exist: no
/// worktree was ever made.
pub fn prune_if_due(root: &Path, now: SystemTime) -> Option<Pruned> {
    if !root.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let at = root.join(STAMP_FILE);
    let mut file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&at).ok()?;
    // Waited for, not tried: another krowk judging it holds it only to
    // write the time (and a process forked meanwhile, for a moment), and
    // then this one finds it not due.
    file.lock().ok()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).ok()?;
    let last = text.trim().parse::<i64>().ok();
    let due = last.is_none_or(|ms| now_ms(now).saturating_sub(ms) >= DAILY.as_millis() as i64 || ms > now_ms(now));
    if !due {
        return None;
    }
    file.set_len(0).ok()?;
    std::io::Seek::rewind(&mut file).ok()?;
    std::io::Write::write_all(&mut file, now_ms(now).to_string().as_bytes()).ok()?;
    drop(file);
    Some(prune(root, now))
}

/// `t` in milliseconds since the epoch.
fn now_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{git_ok, has_git, repo};
    use super::super::{create, create_held, finish, Finished};
    use super::*;

    /// The one `list` finds at `path`.
    fn row(root: &Path, path: &Path) -> Listed {
        list(root).into_iter().find(|l| l.path == path).expect("listed")
    }

    /// A clean, a dirty and an ahead worktree, each with the right flags,
    /// its base, session and age; the one a session holds is live until it
    /// lets go.
    #[test]
    fn wt9_list_shows_clean_dirty_ahead_and_live() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-list");
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let clean = create(&main, &root, "s-clean").unwrap();
        let dirty = create(&main, &root, "s-dirty").unwrap();
        std::fs::write(dirty.path.join("new.txt"), "new\n").unwrap();
        let ahead = create(&main, &root, "s-ahead").unwrap();
        std::fs::write(ahead.path.join("b.txt"), "b\n").unwrap();
        git_ok(&ahead.path, &["add", "b.txt"]);
        git_ok(&ahead.path, &["commit", "-q", "-m", "two"]);
        let (live, held) = create_held(&main, &root, "s-live").unwrap();

        let all = list(&root);
        assert_eq!(all.len(), 4, "{all:#?}");
        let c = row(&root, &clean.path);
        assert_eq!((c.dirty, c.ahead, c.live, c.missing), (Some(false), Some(0), false, false));
        assert_eq!((c.base.as_deref(), c.session.as_deref(), c.branch.clone()), (Some(head.as_str()), Some("s-clean"), Some(clean.branch())));
        assert_eq!(c.main.canonicalize().unwrap(), main.canonicalize().unwrap());
        assert!(c.created_ms.is_some_and(|ms| (now_ms(SystemTime::now()) - ms).abs() < 60_000));
        assert!(!c.changed());
        let d = row(&root, &dirty.path);
        assert_eq!((d.dirty, d.ahead), (Some(true), Some(0)));
        let a = row(&root, &ahead.path);
        assert_eq!((a.dirty, a.ahead), (Some(false), Some(1)));
        assert!(row(&root, &live.path).live);
        drop(held);
        assert!(!row(&root, &live.path).live, "let go");
        // The repository is found from its `common` file alone.
        assert_eq!(std::fs::read_to_string(root.join(&clean.repo_id).join(COMMON_FILE)).unwrap(), clean.common.to_str().unwrap());
        // A finished worktree leaves no record behind.
        assert_eq!(finish(&clean).unwrap(), Finished::Removed);
        assert!(read_record(clean.repo_dir(), &clean.hex).is_none());
        assert!(!live_path(clean.repo_dir(), &clean.hex).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Dirty and not forced: refused, nothing changed. Forced: removed, the
    /// branch kept, and the snapshot brings the change back, the new file
    /// too. A live one is refused even forced.
    #[test]
    fn wt9_remove_refuses_a_dirty_worktree_and_a_forced_one_is_snapshotted() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-remove");
        let w = create(&main, &root, "s1").unwrap();
        std::fs::write(w.path.join("a.txt"), "changed\n").unwrap();
        std::fs::write(w.path.join("untracked.txt"), "untracked\n").unwrap();
        let before = (git_ok(&main, &["worktree", "list", "--porcelain"]), git_ok(&w.path, &["status", "--porcelain"]), git_ok(&main, &["for-each-ref"]));
        match remove(&root, &w.hex, false) {
            Err(Refusal::Changed(l)) => assert_eq!(l.dirty, Some(true)),
            other => panic!("refused: {other:?}"),
        }
        assert_eq!((git_ok(&main, &["worktree", "list", "--porcelain"]), git_ok(&w.path, &["status", "--porcelain"]), git_ok(&main, &["for-each-ref"])), before, "nothing changed");
        assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "changed\n");

        let removed = remove(&root, w.path.to_str().unwrap(), true).unwrap();
        assert_eq!(removed.snapshot.as_deref(), Some(format!("refs/krowk/snapshots/{}", w.hex).as_str()));
        assert!(!w.path.exists());
        assert!(!git_ok(&main, &["worktree", "list"]).contains(&w.hex));
        assert_eq!(git_ok(&main, &["branch", "--list", "--format=%(refname:short)", &w.branch()]), w.branch(), "the branch is kept");
        assert!(read_record(w.repo_dir(), &w.hex).is_none());
        // The restore commands, as printed, bring it back.
        for line in removed.restore() {
            let words: Vec<&str> = line.split(' ').skip(1).collect();
            git_ok(&main, &words);
        }
        assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "changed\n");
        assert_eq!(std::fs::read_to_string(w.path.join("untracked.txt")).unwrap(), "untracked\n");
        // Added back by hand it has no record: its base unknown, so a
        // removal needs --force.
        let back = find(&root, &w.hex).unwrap();
        assert_eq!((back.base, back.session), (None, None));
        assert!(matches!(remove(&root, &w.hex, false), Err(Refusal::Changed(l)) if l.ahead.is_none()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Clean: removed unforced, with no snapshot. Ahead: refused unless
    /// forced, its commits kept on its branch. Held by a live session:
    /// refused, forced or not.
    #[test]
    fn wt9_remove_takes_a_clean_one_and_refuses_an_ahead_or_live_one() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-remove-clean");
        let w = create(&main, &root, "s2").unwrap();
        assert_eq!(remove(&root, &w.branch(), false).unwrap().snapshot, None);
        assert!(!w.path.exists());
        let w = create(&main, &root, "s3").unwrap();
        git_ok(&w.path, &["commit", "-q", "--allow-empty", "-m", "ahead"]);
        assert!(matches!(remove(&root, &w.hex, false), Err(Refusal::Changed(l)) if l.ahead == Some(1)));
        assert_eq!(remove(&root, &w.hex, true).unwrap().snapshot, None);
        assert_eq!(git_ok(&main, &["log", "-1", "--format=%s", &w.branch()]), "ahead");
        let (w, held) = create_held(&main, &root, "s4").unwrap();
        assert!(matches!(remove(&root, &w.hex, true), Err(Refusal::Live(l)) if l.session.as_deref() == Some("s4")));
        assert!(w.path.exists());
        drop(held);
        assert!(remove(&root, &w.hex, false).is_ok());
        assert_eq!(remove(&root, "0badf00d", false), Err(Refusal::NotFound("0badf00d".into())));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A worktree directory deleted by hand, still locked by the session
    /// that crashed in it: prune leaves no `git worktree list` entry and no
    /// record. A directory whose admin directory is gone is deleted; the
    /// repository's own files, a template and a live worktree are not.
    #[test]
    fn wt9_prune_clears_what_rm_rf_left() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-prune");
        let gone = create(&main, &root, "crashed").unwrap();
        std::fs::remove_dir_all(&gone.path).unwrap();
        let orphan = create(&main, &root, "s2").unwrap();
        let admin = admin_dir(&orphan.path).unwrap();
        // git's record gone, the directory left: what `git worktree prune`
        // after a move does.
        std::fs::remove_dir_all(&admin).unwrap();
        let (live, held) = create_held(&main, &root, "s3").unwrap();
        let dir = gone.repo_dir().to_path_buf();
        std::fs::write(dir.join("probe.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("template")).unwrap();
        // A link that looks like a worktree is not followed.
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("deadbeef")).unwrap();

        let done = prune(&root, SystemTime::now());
        assert!(done.failures.is_empty(), "{:?}", done.failures);
        assert_eq!(done.dirs, std::slice::from_ref(&orphan.path));
        assert!(done.registrations.iter().any(|p| p.ends_with(&gone.hex)), "{:?}", done.registrations);
        let listed = git_ok(&main, &["worktree", "list", "--porcelain"]);
        assert!(!listed.contains(&gone.hex) && !listed.contains(&orphan.hex), "{listed}");
        assert!(listed.contains(&live.hex));
        assert!(!orphan.path.exists());
        assert!(read_record(&dir, &gone.hex).is_none() && read_record(&dir, &orphan.hex).is_none());
        assert!(read_record(&dir, &live.hex).is_some() && live.path.is_dir());
        assert!(dir.join("probe.json").is_file() && dir.join("template").is_dir() && dir.join(COMMON_FILE).is_file());
        assert!(outside.join("keep").is_file());
        // Their branches are the person's to delete.
        assert_eq!(git_ok(&main, &["branch", "--list", "--format=%(refname:short)", &gone.branch()]), gone.branch());
        drop(held);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repository whose git directory moved (or whose drive is not
    /// mounted): prune leaves its worktrees, uncommitted work and all,
    /// alone, and says so.
    #[test]
    fn wt9_prune_leaves_a_moved_repository_alone() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-moved");
        let w = create(&main, &root, "s1").unwrap();
        std::fs::write(w.path.join("work.txt"), "work\n").unwrap();
        let moved = base.join("moved");
        std::fs::rename(&main, &moved).unwrap();
        let done = prune(&root, SystemTime::now());
        assert!(done.dirs.is_empty() && done.registrations.is_empty(), "{done:?}");
        assert!(done.failures.iter().any(|f| f.contains("left alone")), "{:?}", done.failures);
        assert_eq!(std::fs::read_to_string(w.path.join("work.txt")).unwrap(), "work\n");
        assert!(read_record(w.repo_dir(), &w.hex).is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The person's own worktree, outside the root, its directory away:
    /// its record is not krowk's to clear. A krowk directory git no longer
    /// knows is deleted only when its files are its branch's and no live
    /// session holds it.
    #[test]
    fn wt9_prune_keeps_the_persons_worktrees_and_orphans_with_changes() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-own");
        let own = base.join("own");
        git_ok(&main, &["worktree", "add", "-q", "-b", "mine", own.to_str().unwrap()]);
        std::fs::rename(&own, base.join("own-away")).unwrap();
        let gone = create(&main, &root, "s1").unwrap();
        std::fs::remove_dir_all(&gone.path).unwrap();
        let orphan = |owner: &str| {
            let w = create(&main, &root, owner).unwrap();
            std::fs::remove_dir_all(admin_dir(&w.path).unwrap()).unwrap();
            w
        };
        let dirty = orphan("s2");
        std::fs::write(dirty.path.join("work.txt"), "work\n").unwrap();
        let edited = orphan("s3");
        std::fs::write(edited.path.join("a.txt"), "edited\n").unwrap();
        let clean = orphan("s4");
        let held = create_held(&main, &root, "s5").unwrap();
        std::fs::remove_dir_all(admin_dir(&held.0.path).unwrap()).unwrap();

        let done = prune(&root, SystemTime::now());
        let listed = git_ok(&main, &["worktree", "list", "--porcelain"]);
        assert!(listed.contains(own.to_str().unwrap()), "the person's own record is kept: {listed}");
        assert!(!listed.contains(&gone.hex), "{listed}");
        assert_eq!(done.dirs, std::slice::from_ref(&clean.path));
        assert_eq!(std::fs::read_to_string(dirty.path.join("work.txt")).unwrap(), "work\n");
        assert_eq!(std::fs::read_to_string(edited.path.join("a.txt")).unwrap(), "edited\n");
        assert!(held.0.path.is_dir(), "a live one is left");
        assert_eq!(done.failures.iter().filter(|f| f.contains("differ from its branch")).count(), 2, "{:?}", done.failures);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A forced removal of a worktree whose agent detached HEAD and
    /// committed there: that commit is kept, and the restore commands bring
    /// back it and the uncommitted change on top.
    #[test]
    fn wt9_a_forced_remove_keeps_a_head_off_its_branch() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-detached");
        let w = create(&main, &root, "s1").unwrap();
        git_ok(&w.path, &["checkout", "-q", "--detach"]);
        std::fs::write(w.path.join("b.txt"), "b\n").unwrap();
        git_ok(&w.path, &["add", "b.txt"]);
        git_ok(&w.path, &["commit", "-q", "-m", "detached"]);
        let head = git_ok(&w.path, &["rev-parse", "HEAD"]);
        std::fs::write(w.path.join("a.txt"), "changed\n").unwrap();
        let removed = remove(&root, &w.hex, true).unwrap();
        let kept = format!("{SNAPSHOTS}{}-head", w.hex);
        assert_eq!(removed.head.as_deref(), Some(kept.as_str()));
        assert_eq!(git_ok(&main, &["rev-parse", &format!("{kept}^")]), head);
        for line in removed.restore() {
            git_ok(&main, &line.split(' ').skip(1).collect::<Vec<_>>());
        }
        assert_eq!(git_ok(&w.path, &["rev-parse", "HEAD"]), head);
        assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "changed\n");
        // On its branch, nothing extra is kept.
        git_ok(&main, &["worktree", "remove", "--force", w.path.to_str().unwrap()]);
        let w = create(&main, &root, "s2").unwrap();
        git_ok(&w.path, &["commit", "-q", "--allow-empty", "-m", "on the branch"]);
        assert_eq!(remove(&root, &w.hex, true).unwrap().head, None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A snapshot whose commit is 31 days old is deleted; one from
    /// yesterday stays. And the daily prune runs once a day.
    #[test]
    fn wt9_prune_expires_snapshots_after_30_days_once_a_day() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt9-expire");
        let w = create(&main, &root, "s1").unwrap();
        let now = SystemTime::now();
        let days_ago = |d: u64| now_ms(now - Duration::from_secs(d * 24 * 60 * 60)) / 1000;
        let tree = git_ok(&main, &["rev-parse", "HEAD^{tree}"]);
        for (name, days) in [("old", 31), ("young", 1)] {
            let o = krowk_api::git::command(&main)
                .unwrap()
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "commit-tree", &tree, "-m", name])
                .env("GIT_COMMITTER_DATE", format!("@{} +0000", days_ago(days)))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            git_ok(&main, &["update-ref", &format!("{SNAPSHOTS}{name}"), String::from_utf8_lossy(&o.stdout).trim()]);
        }
        let done = prune_if_due(&root, now).expect("due: never pruned");
        assert_eq!(done.snapshots, [format!("{SNAPSHOTS}old")]);
        assert_eq!(git_ok(&main, &["for-each-ref", "--format=%(refname)", SNAPSHOTS]), format!("{SNAPSHOTS}young"));
        assert!(prune_if_due(&root, now + Duration::from_secs(60)).is_none(), "not again within a day");
        assert!(prune_if_due(&root, now + DAILY).is_some(), "a day later");
        assert!(w.path.is_dir(), "a worktree that is there is left alone");
        assert!(prune_if_due(&base.join("nowhere"), now).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
