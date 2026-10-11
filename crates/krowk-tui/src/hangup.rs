//! The TUI's way out when its terminal goes, which crossterm's key reader
//! does not give it.
//!
//! On a terminal that has hung up — a window closed, an SSH link dropped,
//! every descriptor of a pseudo-terminal's far side gone — a read answers
//! end-of-file or EIO at once, every time. crossterm 0.29's reader loops on
//! exactly that, never returning, and holds its global event-reader lock
//! while it does: the key stream never ends, and the next call that takes
//! the lock blocking (`EventStream::new`, as a resize starts one) waits on
//! it for good. The TUI's thread, parked there, never looks at the SIGHUP
//! and SIGTERM it is sent again, and the process stays — its thread in a
//! futex wait, crossterm's spinning a core — holding its session until
//! somebody `kill -9`s it.
//!
//! So the terminal is watched from here instead, never through crossterm:
//! `gone` asks the terminal itself whether it has hung up (before a new key
//! reader is started, which would wait on the lock), and `went` wakes the
//! TUI's loop once it has, which then leaves as with no one to answer.
//!
//! Under it, for a loop stuck some other way, a watchdog on a thread and
//! runtime of its own, which a stuck TUI thread cannot hold up. It watches
//! only while the loop runs (`Armed`): before it, and once the loop has
//! returned — the session recorded, backends let go, a worktree finished —
//! it does nothing, and the way out is the TUI's own, however long it
//! takes. While the loop runs, each turn of it is a beat (`beat`). After
//! SIGHUP, SIGTERM or the terminal hanging up, a loop that goes on beating
//! is taking it — stopping cleanly, the running turn interrupted and its
//! end waited for, its ticks beating meanwhile — and is left to; one that
//! does not beat once in `STUCK` is stuck, and the watchdog ends the
//! process: the terminal restored as far as it still can be, every
//! backend's process group killed, exit 130, as a second Ctrl-C leaves.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long the loop, while it runs, may go without a beat after a hangup
/// or a signal before it is taken to be stuck. Past any one turn of the
/// loop that is not stuck: its slowest waits (a command to the host) are
/// bounded at two seconds.
pub const STUCK: Duration = Duration::from_secs(5);

/// Set once the terminal is seen to have hung up.
static GONE: AtomicBool = AtomicBool::new(false);
/// Whether the loop runs, and the watchdog watches.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Turns of the loop.
static BEATS: AtomicU64 = AtomicU64::new(0);
/// Set as the watchdog ends the process: nothing more is drawn.
static ENDING: AtomicBool = AtomicBool::new(false);
static WENT: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// The descriptor crossterm reads keys from, so the one whose hangup
/// matters: stdin when it is a terminal, else `/dev/tty`, opened once and
/// kept. None when there is neither.
fn tty() -> Option<RawFd> {
    static TTY: OnceLock<Option<std::fs::File>> = OnceLock::new();
    // SAFETY: isatty only reads the descriptor's type.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        return Some(libc::STDIN_FILENO);
    }
    TTY.get_or_init(|| std::fs::File::open("/dev/tty").ok()).as_ref().map(AsRawFd::as_raw_fd)
}

/// Whether `fd` has hung up, waiting at most `ms` (-1: until it does).
/// Asks for no events, so input waiting is not an answer: a hangup and an
/// error are reported whatever is asked for. Linux only: macOS's poll(2)
/// answers POLLNVAL for any terminal, hung up or not.
#[cfg(target_os = "linux")]
fn hung_up(fd: RawFd, ms: i32) -> bool {
    let mut p = libc::pollfd { fd, events: 0, revents: 0 };
    loop {
        // SAFETY: one pollfd, owned here, for the length of the call.
        match unsafe { libc::poll(&mut p, 1, ms) } {
            n if n > 0 => return p.revents & (libc::POLLHUP | libc::POLLERR) != 0,
            0 => return false,
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => continue,
            _ => return false,
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn hung_up(_: RawFd, _: i32) -> bool {
    false
}

/// Whether the terminal has hung up: seen to already, or asked now. A
/// key reader must not be started once it has.
pub fn gone() -> bool {
    if GONE.load(Ordering::SeqCst) {
        return true;
    }
    let now = tty().is_some_and(|fd| hung_up(fd, 0));
    if now {
        went_now();
    }
    now
}

fn went_now() {
    GONE.store(true, Ordering::SeqCst);
    WENT.notify_waiters();
}

/// Resolves once the terminal has hung up, for the TUI's loop to leave on.
pub async fn went() {
    loop {
        let wait = WENT.notified();
        tokio::pin!(wait);
        wait.as_mut().enable();
        if GONE.load(Ordering::SeqCst) {
            return;
        }
        wait.await;
    }
}

/// One turn of the loop.
pub fn beat() {
    BEATS.fetch_add(1, Ordering::SeqCst);
}

/// Whether the watchdog is ending the process, and nothing is to be drawn
/// over the terminal it put back.
pub fn ending() -> bool {
    ENDING.load(Ordering::SeqCst)
}

/// The loop running, watched while it is held: the watchdog is started
/// the first time, and stands down when this is dropped, as the loop
/// returns.
pub struct Armed;

impl Drop for Armed {
    fn drop(&mut self) {
        ARMED.store(false, Ordering::SeqCst);
    }
}

/// Arms the watchdog for the loop's run. `restore` puts the terminal back;
/// it is given a moment on a thread of its own, since a terminal nobody
/// reads can block a write to it.
pub fn arm(restore: fn()) -> Armed {
    static STARTED: AtomicBool = AtomicBool::new(false);
    beat();
    ARMED.store(true, Ordering::SeqCst);
    if !STARTED.swap(true, Ordering::SeqCst) {
        let tty = tty();
        let _ = std::thread::Builder::new().name("krowk-hangup".into()).spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(watchdog(tty));
            ENDING.store(true, Ordering::SeqCst);
            let (done, restored) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                restore();
                let _ = done.send(());
            });
            let _ = restored.recv_timeout(Duration::from_millis(250));
            krowk_harness::group::kill_all();
            // SAFETY: _exit ends the process at once, running nothing of
            // it: what a stuck thread holds is not waited on.
            unsafe { libc::_exit(130) }
        });
    }
    Armed
}

/// Returns once the process is to be ended: the loop running, and no beat
/// of it in `STUCK` after SIGHUP, SIGTERM or the terminal hanging up.
/// Never otherwise.
async fn watchdog(tty: Option<RawFd>) {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else { return std::future::pending().await };
    // Ends only once the terminal has hung up; not watchable here (macOS,
    // no terminal), it never does, and SIGHUP says it instead.
    let mut hangup = tokio::task::spawn_blocking(move || {
        if tty.is_some_and(|fd| hung_up(fd, -1)) {
            went_now();
            return;
        }
        loop {
            std::thread::park();
        }
    });
    let mut hung = false;
    loop {
        tokio::select! {
            _ = hup.recv() => {}
            _ = term.recv() => {}
            _ = &mut hangup, if !hung => hung = true,
        }
        // Watched while the loop runs and beats; stood down once it has
        // returned, or for good if it never ran.
        let (mut seen, mut at) = (BEATS.load(Ordering::SeqCst), Instant::now());
        while ARMED.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let now = BEATS.load(Ordering::SeqCst);
            if now != seen {
                (seen, at) = (now, Instant::now());
            } else if at.elapsed() >= STUCK && ARMED.load(Ordering::SeqCst) {
                return;
            }
        }
    }
}
