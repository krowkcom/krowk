//! Seeding (WT5): the second prepare step. A new worktree has no build
//! output, so an agent's first `cargo build` or `npm install` would redo
//! minutes of work the main checkout already did, at the CPU and memory
//! peak that limits how many agents fit on a machine. On a copy-on-write
//! file system a clone of those directories costs almost nothing, so the
//! main checkout's top-level `worktrees.seed` directories (`target` and
//! `node_modules` by default) are cloned into the worktree before its
//! agent starts.
//!
//! - **Only as clones**: never a real copy, which would cost the disk and
//!   the time seeding is meant to save. Whether the worktrees directory
//!   can clone the repository's files is probed once per repository
//!   (`cow`) and remembered for a day in `<root>/<repo-id>/probe.json`,
//!   krowk's own directory, which no sandbox can write.
//! - **Linux**: `cp -a --reflink=always`. **macOS**: `cp -cRp`, and only on
//!   APFS, the same volume on both sides, since `cp -c` silently makes a
//!   full copy anywhere else. **Windows**: nothing is seeded.
//! - **Mtimes kept**: cargo judges freshness by them. The worktree's own
//!   files were just checked out, newer than any build, so once `target` is
//!   seeded each tracked file whose content is the main checkout's gets the
//!   main checkout's mtime (`match_mtimes`), and a build there finds
//!   nothing to do.
//! - **Never a build in progress**: `target` is skipped while a cargo holds
//!   `target/debug/.cargo-lock` or `target/release/.cargo-lock`; both are
//!   held, with cargo's own kind of lock, while it is cloned, so no build
//!   starts mid-copy.
//! - **Never stale packages**: `node_modules` only when the worktree's
//!   lockfiles are byte-identical to the main checkout's.
//! - **Only ignored directories**: one git would see as untracked would
//!   make an unchanged worktree look changed, and keep it.
//!
//! The copy runs as krowk, outside any sandbox; a directory that is a
//! symlink is not seeded, and symlinks inside one are copied as symlinks.
//! Nothing here stops the agent: what was skipped or failed, and why, is
//! appended to `<root>/<repo-id>/seed.log`, and a failed copy removes what
//! it copied.

use super::{Prepare, Worktree, query, random_hex, read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The directories seeded when config names none.
pub const DEFAULT_SEED: &[&str] = &["target", "node_modules"];

/// The lockfiles `node_modules` is judged by: npm's, pnpm's, Yarn's, Bun's.
pub const LOCKFILES: &[&str] = &["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb"];

/// The lock files a running cargo holds in `target`.
const CARGO_LOCKS: &[&str] = &["debug/.cargo-lock", "release/.cargo-lock"];

/// Where a repository's probe result is remembered, in its directory
/// under the worktrees root.
pub const PROBE_FILE: &str = "probe.json";

/// How long a probe result is trusted: a file system can be remounted, or
/// the worktrees root moved.
pub const PROBE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// What seeding says about itself, in the repository's directory.
pub const LOG_FILE: &str = "seed.log";

/// Past this, the log starts again rather than grow for ever.
const LOG_MAX: u64 = 256 * 1024;

/// Whether `name` can be a `worktrees.seed` entry: one directory at the
/// repository's top, never `.git`.
pub fn top_level_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.eq_ignore_ascii_case(".git") && !name.contains(['/', '\\', '\0'])
}

/// The seed step (see the module docs). It says nothing to the agent:
/// seeding only saves work, and a worktree without it is complete.
pub(super) fn seed(p: &Prepare<'_>) -> Option<String> {
    let wt = p.worktree;
    let dir = wt.repo_dir();
    let names: Vec<String> = p.config.seed().into_iter().filter(|n| top_level_name(n) && real_dir(&wt.main.join(n))).collect();
    if names.is_empty() {
        return None;
    }
    if !cow(dir, &wt.common) {
        log(dir, &wt.hex, "nothing seeded: this file system cannot clone the repository's files into the worktrees directory");
        return None;
    }
    let mut target = false;
    for name in &names {
        if !ignored(&wt.path, name) {
            log(dir, &wt.hex, &format!("{name} not seeded: git does not ignore it here, so it would be a change"));
            continue;
        }
        match seed_dir(&wt.main, &wt.path, name) {
            Seeded::Copied => {
                log(dir, &wt.hex, &format!("{name} seeded"));
                target |= name == "target";
            }
            Seeded::Skipped(why) => log(dir, &wt.hex, &format!("{name} not seeded: {why}")),
            Seeded::Failed(why) => log(dir, &wt.hex, &format!("{name} not seeded, the copy failed and was removed: {why}")),
        }
    }
    if target && let Err(e) = match_mtimes(wt) {
        log(dir, &wt.hex, &format!("the worktree's mtimes were not matched to the main checkout's, so cargo may rebuild: {e}"));
    }
    None
}

/// What became of one directory `seed_dir` was asked to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seeded {
    Copied,
    /// Not copied, by the rules: why.
    Skipped(String),
    /// The copy failed, and what it made is gone: why.
    Failed(String),
}

/// Clones `<main>/<name>` to `<into>/<name>` by the rules for it: `target`
/// not while a cargo builds there (its locks held for the copy),
/// `node_modules` only with `into`'s lockfiles the same as `main`'s. The
/// caller has made sure clones are cheap here (`cow`). Blocking.
pub fn seed_dir(main: &Path, into: &Path, name: &str) -> Seeded {
    let (from, to) = (main.join(name), into.join(name));
    if !real_dir(&from) {
        return Seeded::Skipped("the main checkout has no such directory".into());
    }
    if to.symlink_metadata().is_ok() {
        return Seeded::Skipped("the worktree already has it".into());
    }
    let _held = if name == "target" {
        match cargo_locks(&from) {
            Ok(held) => held,
            Err(why) => return Seeded::Skipped(why),
        }
    } else {
        Vec::new()
    };
    if name == "node_modules" && !lockfiles_match(main, into) {
        return Seeded::Skipped(format!("the worktree's lockfile ({}) is not the main checkout's", LOCKFILES.join(", ")));
    }
    match clone_tree(&from, &to) {
        Ok(()) => Seeded::Copied,
        Err(why) => Seeded::Failed(why),
    }
}

/// `from`, a directory, cloned to `to`, which must not exist, with modes
/// and mtimes, symlinks as symlinks: `cp -a --reflink=always`, or `cp -cRp`
/// on macOS. Fails rather than copy where it cannot clone (macOS: see
/// `cow`). On failure whatever it made of `to` is removed.
pub fn clone_tree(from: &Path, to: &Path) -> Result<(), String> {
    let out = clone_command(from, to, true).and_then(|mut c| c.output().map_err(|e| format!("cp: {e}")));
    let failed = match out {
        Ok(o) if o.status.success() => return Ok(()),
        Ok(o) => format!("cp: {}", super::last_lines(&String::from_utf8_lossy(&o.stderr))),
        Err(e) => e,
    };
    match to.symlink_metadata() {
        Ok(m) if m.is_dir() => drop(std::fs::remove_dir_all(to)),
        Ok(_) => drop(std::fs::remove_file(to)),
        Err(_) => {}
    }
    Err(failed)
}

/// The `cp` that clones `from` to `to`: a tree, or one file.
#[allow(unused_variables)]
fn clone_command(from: &Path, to: &Path, tree: bool) -> Result<Command, String> {
    #[cfg(windows)]
    return Err("copy-on-write clones are not made on Windows".into());
    #[cfg(not(windows))]
    {
        let mut c = Command::new("cp");
        #[cfg(target_os = "macos")]
        c.arg(if tree { "-cRp" } else { "-c" });
        #[cfg(not(target_os = "macos"))]
        c.args(if tree { &["-a", "--reflink=always"][..] } else { &["--reflink=always"][..] });
        c.arg(from).arg(to).stdin(std::process::Stdio::null());
        Ok(c)
    }
}

/// Whether files of the repository whose common git directory is `common`
/// can be cloned into `dir`, its directory under the worktrees root,
/// without copying their blocks. Probed by cloning a 1-byte file, and
/// remembered in `dir` for `PROBE_TTL`. Blocking.
pub fn cow(dir: &Path, common: &Path) -> bool {
    let at = dir.join(PROBE_FILE);
    let now = SystemTime::now();
    if let Some(p) = std::fs::read(&at).ok().and_then(|raw| serde_json::from_slice::<Probe>(&raw).ok()) {
        let when = UNIX_EPOCH + Duration::from_secs(p.at);
        // A probe from the future is a clock that moved back: not trusted.
        if now.duration_since(when).is_ok_and(|age| age < PROBE_TTL) {
            return p.cow;
        }
    }
    let cow = probe(dir, common);
    let p = Probe { cow, at: now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) };
    // Whole or not at all: a creation beside this one reads it.
    let tmp = dir.join(format!(".{PROBE_FILE}-{}", random_hex()));
    if std::fs::write(&tmp, serde_json::to_vec(&p).expect("a probe serializes")).is_ok() && std::fs::rename(&tmp, &at).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    cow
}

/// `probe.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Probe {
    cow: bool,
    /// When it was probed, in seconds since the epoch.
    at: u64,
}

/// Clones a 1-byte file from `source` into `dir`, and says whether that
/// worked. Both files are gone after.
fn probe(dir: &Path, source: &Path) -> bool {
    if !same_cow_volume(dir, source) {
        return false;
    }
    let name = format!("krowk-cow-probe-{}", random_hex());
    let (from, to) = (source.join(&name), dir.join(&name));
    if std::fs::write(&from, b"k").is_err() {
        return false;
    }
    let cloned = clone_command(&from, &to, false).is_ok_and(|mut c| c.output().is_ok_and(|o| o.status.success()));
    let _ = std::fs::remove_file(&from);
    let _ = std::fs::remove_file(&to);
    cloned
}

/// Whether `cp -c` from `b` to `a` would clone: both on one APFS volume.
/// Anywhere else `cp -c` copies without saying so.
#[cfg(target_os = "macos")]
fn same_cow_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let apfs = |p: &Path| {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
        // SAFETY: statfs writes the struct it is given, zeroed here, and
        // reads a NUL-terminated path; f_fstypename is NUL-terminated.
        unsafe {
            let mut s: libc::statfs = std::mem::zeroed();
            libc::statfs(c.as_ptr(), &mut s) == 0 && std::ffi::CStr::from_ptr(s.f_fstypename.as_ptr()).to_bytes() == b"apfs"
        }
    };
    let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
    apfs(a) && apfs(b) && dev(a).is_some() && dev(a) == dev(b)
}

/// Elsewhere `cp --reflink=always` fails where it cannot clone, so the
/// probe itself says. btrfs subvolumes differ in `st_dev` and still clone.
#[cfg(not(target_os = "macos"))]
fn same_cow_volume(_: &Path, _: &Path) -> bool {
    cfg!(not(windows))
}

/// `path` is a directory, and not a symlink to one: what is seeded is the
/// checkout's own, never somewhere a link leads.
fn real_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

/// Whether git ignores the directory `name` at the worktree's top.
fn ignored(worktree: &Path, name: &str) -> bool {
    query(worktree).is_ok_and(|mut c| c.args(["check-ignore", "-q", "--", &format!("{name}/")]).output().is_ok_and(|o| o.status.success()))
}

/// `target`'s cargo locks, taken without waiting, as cargo takes them
/// (`flock` on unix, `LockFileEx` on Windows, which is what std's
/// `try_lock` is), each that exists: held, nobody builds there. Why not,
/// when a cargo holds one or it cannot be judged.
fn cargo_locks(target: &Path) -> Result<Vec<std::fs::File>, String> {
    let mut held = Vec::new();
    for lock in CARGO_LOCKS {
        let at = target.join(lock);
        let file = match std::fs::File::open(&at) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("target/{lock} could not be opened: {e}")),
        };
        match file.try_lock() {
            Ok(()) => held.push(file),
            Err(std::fs::TryLockError::WouldBlock) => return Err(format!("a build is running there (target/{lock} is held)")),
            Err(std::fs::TryLockError::Error(e)) => return Err(format!("target/{lock} could not be locked: {e}")),
        }
    }
    Ok(held)
}

/// Whether `into` has the same lockfiles as `main`, byte for byte, and at
/// least one: with none, nothing says the packages fit.
fn lockfiles_match(main: &Path, into: &Path) -> bool {
    let read = |p: PathBuf| -> Result<Option<Vec<u8>>, ()> {
        match p.symlink_metadata() {
            Ok(m) if m.is_file() => std::fs::read(&p).map(Some).map_err(drop),
            Ok(_) => Err(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(()),
        }
    };
    let mut any = false;
    for name in LOCKFILES {
        match (read(main.join(name)), read(into.join(name))) {
            (Ok(a), Ok(b)) if a == b => any |= a.is_some(),
            _ => return false,
        }
    }
    any
}

/// Gives each tracked file of the worktree whose content is the main
/// checkout's the main checkout's mtime, so cargo, which compares source
/// mtimes with its build's, finds the seeded `target` fresh. "The same" is
/// git's judgement in the main checkout (`git diff <base>`, by its index's
/// stat cache, so nothing is hashed that has not changed): a file that
/// differs keeps its new mtime and is rebuilt. Then the worktree's index
/// takes the new stat data, so its `git status` hashes nothing either.
fn match_mtimes(wt: &Worktree) -> Result<(), String> {
    let e = |e: super::Error| e.to_string();
    let differ: std::collections::HashSet<String> =
        read(query(&wt.main).map_err(e)?.args(["diff", "--name-only", "--no-relative", "--no-renames", "-z", &wt.base, "--"]), "diff").map_err(e)?.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect();
    let tracked = read(query(&wt.path).map_err(e)?.args(["ls-files", "-z"]), "ls-files").map_err(e)?;
    for path in tracked.split('\0').filter(|p| !p.is_empty() && !differ.contains(*p)) {
        let (theirs, ours) = (wt.main.join(path), wt.path.join(path));
        let (Ok(m), Ok(o)) = (theirs.symlink_metadata(), ours.symlink_metadata()) else { continue };
        if !m.is_file() || !o.is_file() {
            continue;
        }
        let Ok(when) = m.modified() else { continue };
        if o.modified().is_ok_and(|t| t == when) {
            continue;
        }
        // The owner may set a file's times through any descriptor, so a
        // read-only file is opened for reading.
        if let Ok(f) = std::fs::File::open(&ours) {
            let _ = f.set_modified(when);
        }
    }
    // Only spares a later status the hashing: one that fails costs that.
    let _ = read(super::git(&wt.path).map_err(e)?.args(["update-index", "-q", "--refresh"]), "update-index");
    Ok(())
}

/// Appends `line` to the repository's `seed.log`, for the worktree `hex`.
/// Best effort: a log that cannot be written stops nothing.
fn log(dir: &Path, hex: &str, line: &str) {
    use std::io::Write;
    let at = dir.join(LOG_FILE);
    if std::fs::metadata(&at).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = std::fs::remove_file(&at);
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&at) {
        let _ = writeln!(f, "{secs} {hex} {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{git_ok, has_git, repo, repo_in};
    use super::super::{Finished, create, finish, prepare};
    use super::*;
    use crate::instances::WorktreesConfig;

    /// `prepare` with the default config.
    fn prepared(w: &Worktree) -> Vec<String> {
        prepare(&Prepare { worktree: w, config: &WorktreesConfig::default() })
    }

    /// cargo, with no target directory but the crate's own and nothing
    /// fetched.
    fn cargo(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("cargo").args(args).arg("--offline").current_dir(dir).env_remove("CARGO_TARGET_DIR").env_remove("CARGO_BUILD_TARGET_DIR").output().unwrap()
    }

    fn has_cargo() -> bool {
        Command::new("cargo").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// A repository holding a crate with no dependencies, built in the
    /// main checkout, `target` ignored; None where git or cargo is missing
    /// or the worktrees root cannot clone the repository's files.
    fn built(name: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
        if !has_git() || !has_cargo() {
            eprintln!("git or cargo is not installed: skipping");
            return None;
        }
        let (base, main, root) = repo(name);
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("Cargo.toml"), "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::write(main.join("src/main.rs"), "fn main() {\n    println!(\"fixture\");\n}\n").unwrap();
        std::fs::write(main.join(".gitignore"), "/target\n/node_modules\n").unwrap();
        let out = cargo(&main, &["build", "--quiet"]);
        assert!(out.status.success(), "cargo build: {}", String::from_utf8_lossy(&out.stderr));
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-q", "-m", "crate"]);
        let common = main.join(".git").canonicalize().unwrap();
        let dir = root.join(super::super::repo_id(&common));
        std::fs::create_dir_all(&dir).unwrap();
        if !cow(&dir, &common) {
            eprintln!("{} cannot clone files (no reflink): skipping", root.display());
            let _ = std::fs::remove_dir_all(&base);
            return None;
        }
        Some((base, main, root))
    }

    fn logged(w: &Worktree) -> String {
        std::fs::read_to_string(w.repo_dir().join(LOG_FILE)).unwrap_or_default()
    }

    /// (btrfs) The main checkout's `target`, cloned into a new worktree: a
    /// build there compiles nothing, the clone shares every block with the
    /// original, and the worktree still counts as unchanged.
    #[test]
    fn wt5_a_seeded_target_builds_nothing_and_costs_no_space() {
        let Some((base, main, root)) = built("seed-fresh") else { return };
        let w = create(&main, &root, "s1").unwrap();
        assert!(!w.path.join("target").exists());
        assert_eq!(prepared(&w), Vec::<String>::new());
        assert!(w.path.join("target/debug").is_dir(), "seeded: {}", logged(&w));
        assert!(logged(&w).contains(&format!("{} target seeded", w.hex)), "{}", logged(&w));
        let out = cargo(&w.path, &["build", "-v"]);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{said}");
        assert!(said.contains("Fresh fixture") && !said.contains("Compiling") && !said.contains("Dirty"), "nothing compiled: {said}");
        // btrfs's own count of the blocks only the clone holds.
        match Command::new("btrfs").args(["filesystem", "du", "-s", "--raw"]).arg(w.path.join("target")).output() {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let exclusive: u64 = text.lines().nth(1).and_then(|l| l.split_whitespace().nth(1)).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("{text}"));
                eprintln!("the seeded target's exclusive usage: {exclusive} bytes\n{text}");
                assert!(exclusive < 64 * 1024, "near zero: {text}");
            }
            _ => eprintln!("btrfs filesystem du is not available here: not measured"),
        }
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "");
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (btrfs) A build running in the main checkout: `target` is not
    /// copied, and the worktree is made all the same.
    #[test]
    fn wt5_a_build_in_progress_is_not_seeded() {
        let Some((base, main, root)) = built("seed-locked") else { return };
        let lock = std::fs::File::open(main.join("target/debug/.cargo-lock")).unwrap();
        lock.lock().unwrap();
        let w = create(&main, &root, "s1").unwrap();
        assert_eq!(prepared(&w), Vec::<String>::new());
        assert!(!w.path.join("target").exists());
        assert!(logged(&w).contains("target not seeded: a build is running there"), "{}", logged(&w));
        drop(lock);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `node_modules` follows the lockfile: the worktree's the same as the
    /// main checkout's, it is seeded (where clones work); a different one,
    /// or none at all, and it is not.
    #[test]
    fn wt5_a_different_lockfile_skips_node_modules() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("seed-lockfile");
        std::fs::write(main.join(".gitignore"), "/node_modules\n").unwrap();
        std::fs::write(main.join("package-lock.json"), "{\"lockfileVersion\": 3}\n").unwrap();
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-q", "-m", "lock"]);
        std::fs::create_dir_all(main.join("node_modules/left-pad")).unwrap();
        std::fs::write(main.join("node_modules/left-pad/index.js"), "module.exports = 1;\n").unwrap();

        let w = create(&main, &root, "s1").unwrap();
        std::fs::write(w.path.join("package-lock.json"), "{\"lockfileVersion\": 3, \"changed\": true}\n").unwrap();
        assert_eq!(prepared(&w), Vec::<String>::new());
        assert!(!w.path.join("node_modules").exists(), "a different lockfile: not seeded");
        assert_eq!(seed_dir(&main, &w.path, "node_modules"), Seeded::Skipped("the worktree's lockfile (package-lock.json, pnpm-lock.yaml, yarn.lock, bun.lock, bun.lockb) is not the main checkout's".into()));
        let cloning = cow(w.repo_dir(), &w.common);
        if cloning {
            assert!(logged(&w).contains("node_modules not seeded: the worktree's lockfile"), "{}", logged(&w));
        }
        // No lockfile on either side: nothing says the packages fit.
        std::fs::remove_file(w.path.join("package-lock.json")).unwrap();
        std::fs::remove_file(main.join("package-lock.json")).unwrap();
        assert!(!lockfiles_match(&main, &w.path));
        git_ok(&w.path, &["checkout", "--", "package-lock.json"]);
        git_ok(&main, &["checkout", "--", "package-lock.json"]);
        assert!(lockfiles_match(&main, &w.path));
        assert_eq!(finish(&w).unwrap(), Finished::Removed);

        let w = create(&main, &root, "s2").unwrap();
        prepared(&w);
        if cloning {
            assert_eq!(std::fs::read_to_string(w.path.join("node_modules/left-pad/index.js")).unwrap(), "module.exports = 1;\n", "the same lockfile: seeded");
        } else {
            eprintln!("{} cannot clone files (no reflink): the seeded case is not checked", root.display());
        }
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// On a file system that cannot clone (tmpfs), nothing is copied and
    /// the worktree is made; the probe's answer is kept for a day, and
    /// probed again after.
    #[test]
    fn wt5_without_reflink_nothing_is_copied() {
        let shm = Path::new("/dev/shm");
        if !has_git() || !shm.is_dir() || std::fs::write(shm.join(format!("krowk-probe-{}", std::process::id())), b"").is_err() {
            eprintln!("git or a writable /dev/shm is missing: skipping");
            return;
        }
        let _ = std::fs::remove_file(shm.join(format!("krowk-probe-{}", std::process::id())));
        let (base, main, root) = repo_in(shm, "seed-tmpfs");
        std::fs::write(main.join(".gitignore"), "/target\n").unwrap();
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-q", "-m", "ignore"]);
        std::fs::create_dir_all(main.join("target/debug")).unwrap();
        std::fs::write(main.join("target/debug/out"), "built\n").unwrap();
        let w = create(&main, &root, "s1").unwrap();
        assert_eq!(prepared(&w), Vec::<String>::new());
        assert!(!w.path.join("target").exists(), "nothing copied");
        assert!(logged(&w).contains("nothing seeded: this file system cannot clone"), "{}", logged(&w));
        let probe: serde_json::Value = serde_json::from_slice(&std::fs::read(w.repo_dir().join(PROBE_FILE)).unwrap()).unwrap();
        assert_eq!(probe["cow"], false);
        assert!(std::fs::read_dir(w.repo_dir()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("probe-")), "the probe's files are gone");
        assert!(std::fs::read_dir(&w.common).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("probe-")));

        // A remembered answer is taken as it is, for a day.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        std::fs::write(w.repo_dir().join(PROBE_FILE), format!("{{\"cow\": true, \"at\": {now}}}")).unwrap();
        assert!(cow(w.repo_dir(), &w.common), "within a day: not probed again");
        std::fs::write(w.repo_dir().join(PROBE_FILE), format!("{{\"cow\": true, \"at\": {}}}", now - PROBE_TTL.as_secs() - 1)).unwrap();
        assert!(!cow(w.repo_dir(), &w.common), "a day old: probed again");
        std::fs::write(w.repo_dir().join(PROBE_FILE), format!("{{\"cow\": true, \"at\": {}}}", now + 3600)).unwrap();
        assert!(!cow(w.repo_dir(), &w.common), "from the future: probed again");
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A copy that fails leaves nothing of itself.
    #[test]
    fn wt5_a_failed_copy_removes_what_it_made() {
        let base = std::env::temp_dir().join(format!("krowk-seed-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("from/sub")).unwrap();
        std::fs::write(base.join("from/a"), "a").unwrap();
        // Into a directory that does not exist: cp fails.
        assert!(clone_tree(&base.join("from"), &base.join("missing/to")).is_err());
        assert!(!base.join("missing").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wt5_seed_names_are_one_directory_at_the_top() {
        for good in ["target", "node_modules", ".venv", "build-out"] {
            assert!(top_level_name(good), "{good}");
        }
        for bad in ["", ".", "..", ".git", ".GIT", "a/b", "/abs", "a\\b", "a\0b"] {
            assert!(!top_level_name(bad), "{bad:?}");
        }
    }
}
