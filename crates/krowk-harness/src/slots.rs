//! Slots shared by every krowk process on the machine: a pool of `N` lock
//! files, `<runtime dir>/<pool>/<n>.lock`, each held by at most one process
//! at a time. Taking a slot is `File::try_lock` on each file in turn, the
//! lowest free one won; a process that holds one and dies — killed,
//! crashed — lets go of it there and then, since the lock is the OS's and
//! goes with the last descriptor. Nothing is written in the files and
//! nothing is cleaned up: a free slot is a file nobody has locked.
//!
//! The bash tool queues build and test commands behind the `build-slots`
//! pool (`crate::builds`); the same pool shape holds anything else a
//! machine should run only so many of at once.
//!
//! The runtime directory is the one the host daemon's socket lives under
//! (canon `engineering/harness.md` → Where it listens):
//! `$XDG_RUNTIME_DIR/krowk`, else `$TMPDIR/krowk-<uid>`, else
//! `/tmp/krowk-<uid>`; on Windows `%LOCALAPPDATA%\krowk\run`. Made `0700`
//! and refused when a symlink or another user's, as krowk's home is
//! (`krowk_api::home::own`). Slots are the user's, machine-wide: unlike the
//! daemon's directory they are not keyed by krowk's home, since two homes
//! still build on one machine.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often a full pool is tried again.
pub const RETRY: Duration = Duration::from_millis(250);

/// krowk's runtime directory, as `env` names it: where the host daemon
/// listens and the slots are. Only named here; `Pool` makes it when a slot
/// is first taken.
pub fn runtime_dir(env: &dyn Fn(&str) -> String) -> PathBuf {
    let absolute = |k: &str| Some(env(k)).filter(|v| !v.is_empty() && Path::new(v).is_absolute()).map(PathBuf::from);
    if let Some(run) = absolute("XDG_RUNTIME_DIR") {
        return run.join("krowk");
    }
    #[cfg(unix)]
    {
        let base = absolute("TMPDIR").unwrap_or_else(|| PathBuf::from("/tmp"));
        // SAFETY: getuid has no preconditions and cannot fail.
        base.join(format!("krowk-{}", unsafe { libc::getuid() }))
    }
    #[cfg(not(unix))]
    {
        match absolute("LOCALAPPDATA") {
            Some(local) => local.join("krowk").join("run"),
            None => std::env::temp_dir().join("krowk-run"),
        }
    }
}

/// A pool of slots: `size` lock files in `<runtime>/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    runtime: PathBuf,
    name: String,
    size: usize,
}

/// One slot, held until dropped.
#[derive(Debug)]
pub struct Slot {
    index: usize,
    _held: std::fs::File,
}

impl Slot {
    /// Which slot it is, from 0: the lowest that was free.
    pub fn index(&self) -> usize {
        self.index
    }
}

impl Pool {
    /// The pool `name` under the runtime directory `runtime`, of `size`
    /// slots (at least one).
    pub fn new(runtime: PathBuf, name: &str, size: usize) -> Pool {
        Pool { runtime, name: name.into(), size: size.max(1) }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// The pool's directory.
    pub fn dir(&self) -> PathBuf {
        self.runtime.join(&self.name)
    }

    /// The lowest free slot, or none when every one is held. Never blocks,
    /// so an async caller can poll it and still hear an interrupt.
    pub fn try_take(&self) -> Result<Option<Slot>, String> {
        krowk_api::home::make(&self.runtime)?;
        let dir = self.dir();
        krowk_api::home::make(&dir)?;
        for index in 0..self.size {
            let path = dir.join(format!("{index}.lock"));
            let mut o = std::fs::OpenOptions::new();
            o.create(true).truncate(false).write(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let f = o.open(&path).map_err(|e| format!("{} could not be opened: {e}", path.display()))?;
            match f.try_lock() {
                Ok(()) => return Ok(Some(Slot { index, _held: f })),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(format!("{} could not be locked: {e}", path.display())),
            }
        }
        Ok(None)
    }

    /// A slot, waited for as long as it takes: tried every `RETRY` while
    /// the pool is full, `waiting` told once, with how many are in use,
    /// when the first try finds none (and awaited, so it can say so on a
    /// channel). Dropping the future stops the wait — an interrupted turn
    /// drops its call. Returns the slot and how long it waited.
    pub async fn take<F: std::future::Future<Output = ()>>(&self, waiting: impl FnOnce(usize) -> F) -> Result<(Slot, Duration), String> {
        let started = Instant::now();
        let mut waiting = Some(waiting);
        loop {
            if let Some(slot) = self.try_take()? {
                return Ok((slot, started.elapsed()));
            }
            if let Some(w) = waiting.take() {
                w(self.size).await;
            }
            tokio::time::sleep(RETRY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("krowk-slots-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[cfg(unix)]
    #[test]
    fn the_runtime_dir_is_the_daemons_without_the_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map_or_else(String::new, |(_, v)| v.to_string());
        assert_eq!(runtime_dir(&env(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("TMPDIR", "/var/tmp")])), PathBuf::from("/run/user/1000/krowk"));
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        assert_eq!(runtime_dir(&env(&[("TMPDIR", "/var/folders/x")])), PathBuf::from(format!("/var/folders/x/krowk-{uid}")));
        assert_eq!(runtime_dir(&env(&[("XDG_RUNTIME_DIR", "relative"), ("TMPDIR", "also")])), PathBuf::from(format!("/tmp/krowk-{uid}")), "only absolute paths count");
    }

    #[test]
    fn the_lowest_free_slot_is_taken_and_a_full_pool_gives_none() {
        let d = dir("full");
        let pool = Pool::new(d.clone(), "build-slots", 2);
        let a = pool.try_take().unwrap().expect("a free slot");
        let b = pool.try_take().unwrap().expect("a second");
        assert_eq!((a.index(), b.index()), (0, 1));
        assert!(pool.try_take().unwrap().is_none(), "both held, even by this process");
        drop(a);
        assert_eq!(pool.try_take().unwrap().expect("freed on drop").index(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(std::fs::metadata(pool.dir()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_pool_directory_is_refused() {
        let d = dir("link");
        std::fs::create_dir_all(d.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(d.join("elsewhere"), d.join("build-slots")).unwrap();
        let err = Pool::new(d.clone(), "build-slots", 1).try_take().unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_full_pool_is_waited_for_and_the_wait_is_said_once() {
        let d = dir("wait");
        let pool = Pool::new(d.clone(), "build-slots", 1);
        let held = pool.try_take().unwrap().unwrap();
        let freer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            drop(held);
        });
        let said = std::sync::Mutex::new(Vec::new());
        let (slot, waited) = pool.take(|n| {
            said.lock().unwrap().push(n);
            async {}
        }).await.unwrap();
        freer.join().unwrap();
        assert_eq!((slot.index(), said.into_inner().unwrap()), (0, vec![1]));
        assert!(waited >= Duration::from_millis(500), "{waited:?}");
        // Free from the start: no wait to tell.
        drop(slot);
        let (_, waited) = pool.take(|_| async { panic!("nothing to wait for") }).await.unwrap();
        assert!(waited < RETRY);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Run by `a_slot_held_by_a_killed_process_is_free_at_once` as a
    /// process of its own: takes the slot, says so, and holds it until
    /// killed. Run any other way it does nothing.
    #[test]
    fn holder() {
        let Ok(runtime) = std::env::var("KROWK_TEST_SLOT_HOLDER") else { return };
        let _slot = Pool::new(PathBuf::from(runtime), "build-slots", 1).try_take().unwrap().expect("the slot is free");
        println!("\nslot-held");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn a_slot_held_by_a_killed_process_is_free_at_once() {
        use std::io::BufRead;
        let d = dir("killed");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "slots::tests::holder", "--nocapture", "--test-threads=1"])
            .env("KROWK_TEST_SLOT_HOLDER", &d)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(lines.any(|l| l.is_ok_and(|l| l.trim() == "slot-held")), "the holder never took the slot");
        let pool = Pool::new(d.clone(), "build-slots", 1);
        assert!(pool.try_take().unwrap().is_none(), "another process holds it");
        child.kill().unwrap();
        child.wait().unwrap();
        // Free as soon as the process is gone: no timeout, no stale file.
        assert_eq!(pool.try_take().unwrap().expect("freed by the kill").index(), 0);
        let _ = std::fs::remove_dir_all(d);
    }
}
