//! A pseudo-terminal to run the TUI on, for the tests and the budgets: the
//! child gets the slave side as its stdin, stdout, stderr and controlling
//! terminal, and everything it draws is collected from the master side.
//!
//! Nothing emulates a screen here — tmux does that where a test needs one.
//! The one thing a terminal must do for the TUI to start is answer "where
//! is the cursor" (CSI 6n), so the reader does: row 1, column 1, which is
//! where a fresh terminal's cursor is. Each synchronized-update start (CSI
//! ?2026h) is timestamped as it arrives: one per frame drawn.
//!
//! Standard library and libc only, Linux and macOS.
#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

#[derive(Default)]
struct Seen {
    out: Vec<u8>,
    /// When each frame started and ended arriving.
    frames: Vec<Instant>,
    frame_ends: Vec<Instant>,
}

pub struct Pty {
    master: File,
    pub child: Child,
    seen: Arc<Mutex<Seen>>,
    pub started: Instant,
}

fn open_pty(cols: u16, rows: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: plain libc calls on descriptors this function owns; every
    // return value is checked before the descriptor is used.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(master);
        if libc::grantpt(master.as_raw_fd()) != 0 || libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let name = libc::ptsname(master.as_raw_fd());
        if name.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
        if slave < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = OwnedFd::from_raw_fd(slave);
        set_size(master.as_raw_fd(), cols, rows);
        Ok((master, slave))
    }
}

fn set_size(fd: i32, cols: u16, rows: u16) {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCSWINSZ reads one winsize from the pointer.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

impl Pty {
    /// Starts `cmd` on a new terminal `cols` by `rows`.
    pub fn spawn(mut cmd: Command, cols: u16, rows: u16) -> Pty {
        let (master, slave) = open_pty(cols, rows).expect("open a pseudo-terminal");
        let slave_fd = slave.as_raw_fd();
        cmd.stdin(Stdio::from(slave.try_clone().unwrap())).stdout(Stdio::from(slave.try_clone().unwrap())).stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let child = cmd.spawn().expect("spawn under the pty");
        let master = File::from(master);
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let (mut reader, mut answerer) = (master.try_clone().unwrap(), master.try_clone().unwrap());
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let now = Instant::now();
                let mut s = log.lock().unwrap();
                let before = s.out.len();
                s.out.extend_from_slice(&buf[..n]);
                // Each needle is looked for from where one could have begun
                // in the previous read, so a marker split across two reads
                // counts once.
                let seen_in = |needle: &[u8]| count(&s.out[before.saturating_sub(needle.len() - 1)..], needle);
                let (begins, ends, asks) = (seen_in(SYNC_BEGIN), seen_in(SYNC_END), seen_in(b"\x1b[6n"));
                for _ in 0..begins {
                    s.frames.push(now);
                }
                for _ in 0..ends {
                    s.frame_ends.push(now);
                }
                drop(s);
                for _ in 0..asks {
                    let _ = answerer.write_all(b"\x1b[1;1R");
                }
            }
        });
        Pty { master, child, seen, started }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        set_size(self.master.as_raw_fd(), cols, rows);
        // SAFETY: a signal to our own child.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGWINCH);
        }
    }

    pub fn output(&self) -> Vec<u8> {
        self.seen.lock().unwrap().out.clone()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output()).into_owned()
    }

    pub fn frames(&self) -> Vec<Instant> {
        self.seen.lock().unwrap().frames.clone()
    }

    pub fn frame_ends(&self) -> Vec<Instant> {
        self.seen.lock().unwrap().frame_ends.clone()
    }

    /// Waits until the output holds `needle`; the time it arrived, or none.
    pub fn wait_for(&self, needle: &str, timeout: Duration) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.text().contains(needle) {
                return Some(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// Whether the child is still running, asked without waiting for it
    /// or touching it.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Waits for the child to exit, killing it after `timeout`.
    pub fn wait(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(st)) = self.child.try_wait() {
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// The most frames that started inside any one second.
pub fn peak_fps(frames: &[Instant]) -> usize {
    let mut best = 0;
    let mut lo = 0;
    for hi in 0..frames.len() {
        while frames[hi].duration_since(frames[lo]) >= Duration::from_secs(1) {
            lo += 1;
        }
        best = best.max(hi - lo + 1);
    }
    best
}
