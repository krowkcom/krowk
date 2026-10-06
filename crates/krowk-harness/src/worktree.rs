//! Worktrees (WT3): a git worktree of its own for an agent, so agents
//! running at once edit their own files on their own branch rather than
//! overwrite each other's in one checkout and fight over one index.
//!
//! No model is involved here: `create` makes one, `prepare` readies it,
//! `finish` removes it when the agent left it as it found it and keeps it
//! otherwise. The subagent tool calls them around a child's turn
//! (`isolation: "worktree"`), and a top-level session can call them around
//! its own.
//!
//! - **Where**: `<root>/<repo-id>/<hex>`, the root being
//!   `krowk_api::home::worktrees_root` (outside krowk's home, which the
//!   sandbox hides), the repo-id the first 16 hex digits of the sha256 of
//!   the canonical common git directory, `<hex>` 8 random hex digits. The
//!   branch is `krowk/<hex>`, which is what the sandbox lets a commit move
//!   (`crate::sandbox::managed_worktree`).
//! - **Base**: the HEAD commit of the directory it is made from.
//! - **One at a time per repository**: `git worktree add` and the like
//!   write files in the common directory (`config`, `worktrees/`), and two
//!   at once fail on its `config.lock`. So every change here holds `lock`:
//!   a mutex for this process's threads, and `File::lock` on
//!   `<common>/krowk-worktree.lock` for every other krowk.
//! - **Locked while it runs**: `git worktree lock --reason krowk:<owner>`,
//!   the owner being the session that works in it, so `git worktree prune`
//!   leaves it alone and a listing can say whose it is.
//! - **Every git through `krowk_api::git`**: hooks off, so `worktree add`
//!   runs no `post-checkout` of the repository's, outside any sandbox.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};

/// What a krowk worktree's branch is named under.
pub const BRANCH_PREFIX: &str = "krowk/";
/// The file in the common directory every krowk locks to change worktrees.
pub const LOCK_FILE: &str = "krowk-worktree.lock";
/// What a krowk worktree's lock reason starts with; the owner follows.
pub const LOCK_REASON: &str = "krowk:";

/// One worktree krowk made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Its directory, `<root>/<repo-id>/<hex>`: the agent's cwd.
    pub path: PathBuf,
    /// The 8 hex digits its directory and branch are named by.
    pub hex: String,
    /// The commit it was made at.
    pub base: String,
    /// The repository's common git directory, canonical.
    pub common: PathBuf,
    /// The repository's main checkout (a bare repository's own directory).
    pub main: PathBuf,
    /// `repo_id(common)`: the directory under the root it is in.
    pub repo_id: String,
}

impl Worktree {
    /// `krowk/<hex>`.
    pub fn branch(&self) -> String {
        format!("{BRANCH_PREFIX}{}", self.hex)
    }
}

/// Why a worktree could not be made, or finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The directory is in no git repository.
    NotARepository,
    /// git, or the file system, said no: what it said.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotARepository => f.write_str("not in a git repository"),
            Error::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

/// How `finish` left a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// Unchanged and clean: the directory, the branch and git's record of
    /// it are gone.
    Removed,
    /// Changed: unlocked, and kept with its branch, for the person.
    Kept { commits: usize, dirty: bool },
}

impl Finished {
    /// The line the agent that started it is told when it is kept.
    pub fn note(&self, wt: &Worktree) -> Option<String> {
        match self {
            Finished::Removed => None,
            Finished::Kept { commits, dirty } => {
                Some(format!("Worktree: {} (branch {}, {commits} commits, uncommitted changes: {})", wt.path.display(), wt.branch(), if *dirty { "yes" } else { "no" }))
            }
        }
    }
}

/// What a prepare step is handed. The inputs later steps need (config,
/// trust) join it as fields, so a step's signature stays one argument.
pub struct Prepare<'a> {
    pub worktree: &'a Worktree,
}

/// One step that readies a new worktree before its agent starts. It does
/// not fail the creation: a step that cannot do its part says why in its
/// own way and the agent starts anyway.
pub type Step = fn(&Prepare<'_>);

/// The prepare steps, run in this order after `create` and before the
/// agent's first turn: submodules, seed, include, setup, as each lands.
pub const STEPS: &[(&str, Step)] = &[];

/// Runs `STEPS`, in order. Blocking: off the async runtime.
pub fn prepare(p: &Prepare<'_>) {
    for (_, step) in STEPS {
        step(p);
    }
}

/// A new worktree of the repository `cwd` is in, at its HEAD, under
/// `root`, locked for `owner` (the session that will work in it).
/// Blocking: off the async runtime.
pub fn create(cwd: &Path, root: &Path, owner: &str) -> Result<Worktree, Error> {
    let common = match read(query(cwd)?.args(["rev-parse", "--path-format=absolute", "--git-common-dir"]), "rev-parse") {
        Ok(c) if !c.is_empty() => PathBuf::from(c).canonicalize().map_err(|e| Error::Failed(format!("the repository's git directory: {e}")))?,
        _ => return Err(Error::NotARepository),
    };
    let base = read(query(cwd)?.args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]), "rev-parse HEAD")
        .ok()
        .filter(|b| !b.is_empty())
        .ok_or_else(|| Error::Failed("the repository has no commit yet to start a worktree from".into()))?;
    let main = main_checkout(cwd, &common)?;
    let repo_id = repo_id(&common);
    let dir = root.join(&repo_id);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Failed(format!("create {}: {e}", dir.display())))?;
    let _held = lock(&common)?;
    let (hex, path) = std::iter::repeat_with(random_hex).map(|h| (h.clone(), dir.join(h))).take(16).find(|(_, p)| !p.exists()).ok_or_else(|| Error::Failed(format!("no free name for a worktree in {}", dir.display())))?;
    let wt = Worktree { path, hex, base, common, main, repo_id };
    read(git(cwd)?.args(["worktree", "add", "--quiet", "--no-track", "-b"]).arg(wt.branch()).arg(&wt.path).arg(&wt.base), "worktree add")?;
    if let Err(e) = read(git(&wt.main)?.args(["worktree", "lock", "--reason"]).arg(format!("{LOCK_REASON}{owner}")).arg(&wt.path), "worktree lock") {
        let _ = remove(&wt, true);
        return Err(e);
    }
    Ok(wt)
}

/// When the agent is done: unchanged (HEAD still the base, `git status`
/// empty) it is removed with its branch; otherwise unlocked and kept.
/// Blocking: off the async runtime.
pub fn finish(wt: &Worktree) -> Result<Finished, Error> {
    let _held = lock(&wt.common)?;
    let head = read(query(&wt.path)?.args(["rev-parse", "HEAD"]), "rev-parse HEAD")?;
    let status = read(query(&wt.path)?.args(["status", "--porcelain"]), "status")?;
    // Unlocked either way: the session is over. One that was never locked
    // is not a reason to stop.
    let _ = read(git(&wt.main)?.args(["worktree", "unlock"]).arg(&wt.path), "worktree unlock");
    if head == wt.base && status.is_empty() {
        remove(wt, false)?;
        return Ok(Finished::Removed);
    }
    let range = format!("{}..{head}", wt.base);
    let commits = read(query(&wt.path)?.args(["rev-list", "--count", &range]), "rev-list")?.parse().unwrap_or(0);
    Ok(Finished::Kept { commits, dirty: !status.is_empty() })
}

/// The worktree and its branch gone, under the caller's `lock`. `force`
/// for one being rolled back, which may hold the checkout's files only.
fn remove(wt: &Worktree, force: bool) -> Result<(), Error> {
    let mut c = git(&wt.main)?;
    c.args(["worktree", "remove"]);
    if force {
        c.args(["--force", "--force"]);
    }
    read(c.arg(&wt.path), "worktree remove")?;
    read(git(&wt.main)?.args(["branch", "--quiet", "-D"]).arg(wt.branch()), "branch -D").map(drop)
}

/// The first 16 hex digits of the sha256 of `common`, the canonical common
/// git directory: one directory under the root per repository.
pub fn repo_id(common: &Path) -> String {
    let digest = Sha256::digest(common.as_os_str().as_encoded_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// 8 random hex digits.
fn random_hex() -> String {
    let mut b = [0u8; 4];
    // The system's RNG does not fail where krowk runs; the clock is a
    // fallback that only makes a name less unlikely to be taken.
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).is_err() {
        b = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The repository's main checkout: the first worktree git lists.
fn main_checkout(cwd: &Path, common: &Path) -> Result<PathBuf, Error> {
    let list = read(query(cwd)?.args(["worktree", "list", "--porcelain"]), "worktree list")?;
    let first = list.lines().find_map(|l| l.strip_prefix("worktree ")).map(PathBuf::from);
    Ok(first.unwrap_or_else(|| common.to_path_buf()))
}

/// Repositories whose worktrees a thread of this process is changing.
static BUSY: (Mutex<Vec<PathBuf>>, Condvar) = (Mutex::new(Vec::new()), Condvar::new());

/// Held while a repository's worktrees change: see `lock`. The file goes
/// first, so the OS lock is free by the time the next thread is let in.
pub struct RepoLock {
    _file: std::fs::File,
    _key: Busy,
}

/// This process's claim on one repository, given up on drop.
struct Busy(PathBuf);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut busy = BUSY.0.lock().unwrap_or_else(|e| e.into_inner());
        busy.retain(|c| c != &self.0);
        BUSY.1.notify_all();
    }
}

/// The per-repository lock every change to krowk's worktrees of the
/// repository whose common directory is `common` holds, waited for: this
/// process's threads queue on a mutex, and every krowk process on
/// `File::lock` of `<common>/krowk-worktree.lock`, which the OS frees when a
/// holder dies. Both, so a thread never depends on how an OS scopes a file
/// lock within one process.
pub fn lock(common: &Path) -> Result<RepoLock, Error> {
    let key = {
        let mut busy = BUSY.0.lock().unwrap_or_else(|e| e.into_inner());
        while busy.iter().any(|c| c == common) {
            busy = BUSY.1.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        busy.push(common.to_path_buf());
        Busy(common.to_path_buf())
    };
    let at = common.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&at).map_err(|e| Error::Failed(format!("open {}: {e}", at.display())))?;
    file.lock().map_err(|e| Error::Failed(format!("lock {}: {e}", at.display())))?;
    Ok(RepoLock { _file: file, _key: key })
}

/// git that changes something in `dir`, through `krowk_api::git`, with no
/// `GIT_DIR` and the like an outer git (a hook running krowk) may have
/// set: every call here names its repository by its directory.
fn git(dir: &Path) -> Result<Command, Error> {
    let mut c = krowk_api::git::command(dir).map_err(|e| Error::Failed(format!("git: {e}")))?;
    clean(&mut c);
    Ok(c)
}

/// `git` for a call that only reads.
fn query(dir: &Path) -> Result<Command, Error> {
    let mut c = krowk_api::git::query(dir).map_err(|e| Error::Failed(format!("git: {e}")))?;
    clean(&mut c);
    Ok(c)
}

fn clean(c: &mut Command) {
    for k in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_PREFIX"] {
        c.env_remove(k);
    }
}

/// What git printed, trimmed, or what it said when it failed.
fn read(c: &mut Command, what: &str) -> Result<String, Error> {
    let out = c.output().map_err(|e| Error::Failed(format!("git {what}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Failed(format!("git {what}: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit, and a worktrees root beside it.
    fn repo(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("krowk-worktree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let main = base.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git_ok(&main, &["init", "-q", "-b", "main"]);
        std::fs::write(main.join("a.txt"), "a\n").unwrap();
        git_ok(&main, &["add", "a.txt"]);
        git_ok(&main, &["commit", "-q", "-m", "one"]);
        (base.clone(), main, base.join("worktrees"))
    }

    /// git in a test repository with none of this machine's config.
    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let o = krowk_api::git::command(dir).unwrap().args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn has_git() -> bool {
        krowk_api::git::query(Path::new(".")).is_ok_and(|mut c| c.arg("--version").output().is_ok_and(|o| o.status.success()))
    }

    /// Eight at once, as a response of eight subagents starts them: each its
    /// own directory and branch, none failing on another's `config.lock`,
    /// each one the sandbox takes for krowk's, and each removed again.
    #[test]
    fn wt3_eight_worktrees_made_at_once_all_succeed() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("eight");
        let made: Vec<Result<Worktree, Error>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|i| {
                let (main, root) = (&main, &root);
                s.spawn(move || create(main, root, &format!("owner-{i}")))
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let made: Vec<Worktree> = made.into_iter().map(|r| r.expect("every creation succeeds")).collect();
        let mut dirs: Vec<&Path> = made.iter().map(|w| w.path.as_path()).collect();
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), 8, "eight directories");
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let common = main.join(".git").canonicalize().unwrap();
        let list = git_ok(&main, &["worktree", "list", "--porcelain"]);
        assert_eq!(list.matches(&format!("locked {LOCK_REASON}owner-")).count(), 8, "each locked for its owner: {list}");
        for w in &made {
            assert_eq!((w.base.as_str(), &w.common, w.main.canonicalize().unwrap(), w.repo_id.len()), (head.as_str(), &common, main.canonicalize().unwrap(), 16));
            assert_eq!(w.path, root.join(&w.repo_id).join(&w.hex));
            assert!(w.path.join("a.txt").is_file());
            assert_eq!(git_ok(&w.path, &["rev-parse", "--abbrev-ref", "HEAD"]), w.branch());
            let managed = crate::sandbox::managed_worktree(&w.path, &root).expect("the sandbox's krowk worktree");
            assert_eq!(managed.hex, w.hex);
        }
        for w in &made {
            assert_eq!(finish(w).unwrap(), Finished::Removed);
        }
        assert_eq!(git_ok(&main, &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count(), 1);
        assert_eq!(git_ok(&main, &["branch", "--list", "krowk/*"]), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Unchanged leaves nothing; an edit, or a commit, keeps the worktree
    /// and its branch, and the note names both.
    #[test]
    fn wt3_an_unchanged_worktree_is_removed_and_a_changed_one_kept() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("finish");
        let w = create(&main, &root, "s1").unwrap();
        // An ignored build output is not a change.
        std::fs::write(main.join(".git/info/exclude"), "target/\n").unwrap();
        std::fs::create_dir_all(w.path.join("target")).unwrap();
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        assert!(!w.path.exists());
        assert_eq!(git_ok(&main, &["branch", "--list", "krowk/*"]), "");
        assert!(!git_ok(&main, &["worktree", "list"]).contains(&w.hex));

        let w = create(&main, &root, "s2").unwrap();
        std::fs::write(w.path.join("a.txt"), "changed\n").unwrap();
        let kept = finish(&w).unwrap();
        assert_eq!(kept, Finished::Kept { commits: 0, dirty: true });
        assert_eq!(kept.note(&w).unwrap(), format!("Worktree: {} (branch krowk/{}, 0 commits, uncommitted changes: yes)", w.path.display(), w.hex));
        assert!(w.path.join("a.txt").is_file());
        assert_eq!(git_ok(&main, &["branch", "--list", "--format=%(refname:short)", &w.branch()]), w.branch());
        let list = git_ok(&main, &["worktree", "list", "--porcelain"]);
        assert!(list.contains(&w.hex) && !list.contains("locked"), "unlocked: {list}");

        let w = create(&main, &root, "s3").unwrap();
        std::fs::write(w.path.join("b.txt"), "b\n").unwrap();
        git_ok(&w.path, &["add", "b.txt"]);
        git_ok(&w.path, &["commit", "-q", "-m", "two"]);
        assert_eq!(finish(&w).unwrap(), Finished::Kept { commits: 1, dirty: false });
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Outside a repository: refused by name, nothing made.
    #[test]
    fn wt3_outside_a_repository_nothing_is_made() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let base = std::env::temp_dir().join(format!("krowk-worktree-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("plain")).unwrap();
        // GIT_CEILING_DIRECTORIES is not ours to set: a temp dir inside a
        // repository would make this one, so only judge when it is not.
        let r = create(&base.join("plain"), &base.join("worktrees"), "s");
        if krowk_api::git::query(&base).unwrap().args(["rev-parse", "--git-dir"]).output().is_ok_and(|o| !o.status.success()) {
            assert_eq!(r, Err(Error::NotARepository));
            assert!(!base.join("worktrees").exists());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wt3_the_repo_id_is_16_hex_digits_of_the_common_dirs_sha256() {
        let id = repo_id(Path::new("/r/.git"));
        assert_eq!(id.len(), 16);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(id, repo_id(Path::new("/s/.git")));
        let hex = random_hex();
        assert!(hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{hex}");
    }
}
