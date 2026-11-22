//! Disk admission (WT12): a worktree is made only when its file system has
//! room for it and then some. A full disk breaks every agent on it
//! mid-write, so a creation that would leave less than `RESERVE` free —
//! after twice the worktree's expected size, since its builds grow it — is
//! refused before anything is made, naming the command that frees space.
//!
//! The expected size is the last worktree's of the repository, recorded in
//! `<root>/<repo-id>/size` when it was made, else the size of the files the
//! repository tracks at the commit it starts from. Both are git's sizes of
//! the commit a worktree starts from (`git ls-tree -r -l`), working-state
//! changes included: what a checkout writes. A snapshot shares its
//! template's blocks, its seeded build output with them, so this
//! overstates one, and errs the safe way. The record spares the next
//! creation the listing before it; it is a number and nothing more, so one
//! that does not read is as good as none.

use super::{query, read, Error};
use std::path::Path;

/// Space a worktree's creation leaves free beyond twice its expected size.
pub const RESERVE: u64 = 4 << 30;

/// The repository's last worktree's size, in its directory under the root.
pub const SIZE_FILE: &str = "size";

/// Refuses a worktree of the repository `cwd` is in, at `head`, under the
/// repository's directory `dir` (which may not exist yet), when its file
/// system has less than `RESERVE` plus twice its expected size free. A
/// free space that cannot be read admits it: the check is for the
/// machine's sake, and never a reason to refuse one on a machine that
/// will not say.
pub(super) fn admit(cwd: &Path, head: &str, dir: &Path) -> Result<(), Error> {
    let Some(free) = free(dir) else { return Ok(()) };
    let expected = match std::fs::read_to_string(dir.join(SIZE_FILE)).ok().and_then(|s| s.trim().parse().ok()) {
        Some(size) => size,
        None => tracked(cwd, head)?,
    };
    let needed = RESERVE.saturating_add(expected.saturating_mul(2));
    if free < needed {
        return Err(Error::Failed(format!(
            "not enough disk for a worktree: {} free, {} needed — `krowk worktrees prune` clears what deleted worktrees left, and `krowk worktrees remove` a kept one you are done with",
            gib(free),
            gib(needed)
        )));
    }
    Ok(())
}

/// Records the size of the worktree just made from `base` as the
/// repository's last. Best effort: a size that is not recorded is
/// measured again next time.
pub(super) fn record(cwd: &Path, base: &str, dir: &Path) {
    if let Ok(size) = tracked(cwd, base) {
        let _ = std::fs::write(dir.join(SIZE_FILE), size.to_string());
    }
}

/// The size of the files `rev` tracks, submodules (which have none in the
/// listing) aside.
fn tracked(cwd: &Path, rev: &str) -> Result<u64, Error> {
    let listing = read(query(cwd)?.args(["ls-tree", "-r", "-l", "-z", rev]), "ls-tree")?;
    Ok(listing.split('\0').filter_map(|e| e.split_once('\t')).filter_map(|(meta, _)| meta.split_whitespace().nth(3)?.parse::<u64>().ok()).sum())
}

/// `n` bytes, as a person reads a disk's space.
fn gib(n: u64) -> String {
    format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64)
}

#[cfg(test)]
thread_local! {
    /// What `free` reports on this thread, for the tests.
    pub(super) static FREE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The space free to this user on the file system `path` is on, or would
/// be: its nearest ancestor that exists.
fn free(path: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(free) = FREE.with(|f| f.get()) {
        return Some(free);
    }
    let at = path.ancestors().find(|p| p.exists())?;
    available(at)
}

#[cfg(unix)]
fn available(at: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(at.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a NUL-terminated path, and `stat` a statvfs the call fills.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    Some(u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize)))
}

#[cfg(windows)]
fn available(at: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, available: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let wide: Vec<u16> = at.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is a NUL-terminated path; the totals it may leave null.
    (unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) } != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
fn available(_: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::super::tests::{git_ok, has_git, repo};
    use super::*;

    /// With 1 GiB free nothing is made — no directory, no branch, no
    /// record — and the person is told how much is free, how much is
    /// needed, and what frees it.
    #[test]
    fn wt12_a_worktree_is_refused_when_the_disk_is_low_and_nothing_is_made() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("lowdisk");
        FREE.with(|f| f.set(Some(1 << 30)));
        let refused = super::super::create(&main, &root, "owner");
        FREE.with(|f| f.set(None));
        let Err(Error::Failed(why)) = refused else { panic!("made on a full disk: {refused:?}") };
        assert!(why.starts_with("not enough disk for a worktree: 1.0 GiB free, 4.0 GiB needed"), "{why}");
        assert!(why.contains("krowk worktrees prune"), "{why}");
        assert!(!root.exists(), "nothing under the root");
        assert_eq!(git_ok(&main, &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count(), 1);
        assert_eq!(git_ok(&main, &["branch", "--list", "krowk/*"]), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Made, its size is recorded, and the next one is expected to be as
    /// big: twice that, beyond the reserve, is what it needs.
    #[test]
    fn wt12_the_last_worktrees_size_is_what_the_next_one_needs_room_for() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("disksize");
        let wt = super::super::create(&main, &root, "owner").unwrap();
        let dir = wt.repo_dir().to_path_buf();
        assert_eq!(std::fs::read_to_string(dir.join(SIZE_FILE)).unwrap(), "2", "a.txt's two bytes");
        std::fs::write(dir.join(SIZE_FILE), (1u64 << 30).to_string()).unwrap();
        FREE.with(|f| f.set(Some(RESERVE + (2 << 30) - 1)));
        let Err(Error::Failed(why)) = admit(&main, "HEAD", &dir) else { panic!("admitted") };
        assert!(why.contains("6.0 GiB needed"), "{why}");
        FREE.with(|f| f.set(Some(RESERVE + (2 << 30))));
        assert_eq!(admit(&main, "HEAD", &dir), Ok(()));
        FREE.with(|f| f.set(None));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_free_space_of_a_path_not_made_yet_is_its_file_systems() {
        let d = std::env::temp_dir().join(format!("krowk-disk-{}", std::process::id()));
        assert!(free(&d.join("a").join("b")).is_some_and(|f| f > 0));
        assert_eq!(gib(3 << 29), "1.5 GiB");
    }
}
