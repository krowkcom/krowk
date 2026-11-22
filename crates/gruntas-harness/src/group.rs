//! The process groups of every backend process this krowk has running, kept
//! process-wide: a backend runs in its own group (so a Ctrl-C at the
//! terminal is krowk's to turn into an interrupt), which also means nothing
//! stops it when krowk leaves without letting go of it. Stopping a backend
//! normally releases its group here; leaving at once — a second Ctrl-C,
//! `std::process::exit` — calls `kill_all` first, so no vendor process, and
//! nothing it started, outlives krowk.

use std::sync::Mutex;

static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn groups() -> std::sync::MutexGuard<'static, Vec<i32>> {
    GROUPS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A backend process started in a group of its own (its pid is the group's
/// id).
pub fn register(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
        groups().push(pid);
    }
}

/// The group was stopped; it is not krowk's to kill any more.
pub fn release(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        groups().retain(|g| *g != pid);
    }
}

/// The groups still running.
pub fn running() -> Vec<i32> {
    groups().clone()
}

/// SIGKILLs every group still registered: for a krowk about to exit
/// without the time to stop its backends politely.
pub fn kill_all() {
    for pid in std::mem::take(&mut *groups()) {
        #[cfg(unix)]
        // SAFETY: a signal to a process group this process created.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = pid;
    }
}
