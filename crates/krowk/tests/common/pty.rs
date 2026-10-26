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
    /// The master side read to its end: every process on the terminal
    /// has let go of it, and all it wrote is in `out`.
    closed: bool,
    /// Turns of the reader's loop, each counted once what it read is in
    /// `out`: one past a moment means everything read before it is there.
    turns: u64,
    /// Set by `hang_up`: the reader lets go of its master descriptors at
    /// its next turn.
    hang_up: bool,
}

pub struct Pty {
    master: File,
    pub child: Child,
    seen: Arc<Mutex<Seen>>,
    pub started: Instant,
    /// The size it was opened at, columns by rows; `resize` does not
    /// change it.
    pub size: (u16, u16),
}

fn open_pty(cols: u16, rows: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: plain libc calls on descriptors this function owns; every
    // return value is checked before the descriptor is used.
    unsafe {
        // Close-on-exec, as the standard library opens everything: a child
        // that inherited the master would hold its own terminal open past
        // `hang_up`. Asked for as it is opened, so a fork on another of the
        // test's threads in between cannot take it; macOS's posix_openpt
        // takes no such flag, so there it is set just after.
        #[cfg(target_os = "linux")]
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        #[cfg(not(target_os = "linux"))]
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(master);
        #[cfg(not(target_os = "linux"))]
        cloexec(master.as_raw_fd())?;
        if libc::grantpt(master.as_raw_fd()) != 0 || libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let name = libc::ptsname(master.as_raw_fd());
        if name.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if slave < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = OwnedFd::from_raw_fd(slave);
        set_size(master.as_raw_fd(), cols, rows);
        Ok((master, slave))
    }
}

/// Sets close-on-exec on `fd`, where it cannot be asked for at open.
///
/// # Safety
///
/// `fd` is an open descriptor.
#[cfg(not(target_os = "linux"))]
unsafe fn cloexec(fd: i32) -> std::io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
        cmd.stdin(Stdio::from(slave.try_clone().unwrap())).stdout(Stdio::from(slave.try_clone().unwrap()));
        Pty::start(cmd, master, slave, (cols, rows))
    }

    /// Starts `cmd` with only its stderr on a new terminal, still its
    /// controlling one (Ctrl-C written here interrupts it): stdin and
    /// stdout are what `cmd` says.
    pub fn spawn_stderr(cmd: Command, cols: u16, rows: u16) -> Pty {
        let (master, slave) = open_pty(cols, rows).expect("open a pseudo-terminal");
        Pty::start(cmd, master, slave, (cols, rows))
    }

    fn start(mut cmd: Command, master: OwnedFd, slave: OwnedFd, size: (u16, u16)) -> Pty {
        let slave_fd = slave.as_raw_fd();
        cmd.stderr(Stdio::from(slave));
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
                // A bounded wait for input, not a blocking read, so the loop
                // turns even when nothing comes and `drain` can tell when
                // it has caught up.
                let mut fd = libc::pollfd { fd: reader.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                // SAFETY: one pollfd, on a descriptor this thread owns.
                let ready = unsafe { libc::poll(&mut fd, 1, 10) } > 0;
                if log.lock().unwrap().hang_up {
                    let mut s = log.lock().unwrap();
                    s.closed = true;
                    s.turns += 1;
                    return;
                }
                if !ready {
                    log.lock().unwrap().turns += 1;
                    continue;
                }
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let mut s = log.lock().unwrap();
                        s.closed = true;
                        s.turns += 1;
                        return;
                    }
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
        Pty { master, child, seen, started, size }
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

    /// Closes the terminal from its far side, as a terminal emulator's
    /// window closing does: every master descriptor goes, so the child's
    /// session gets SIGHUP and its reads of the terminal fail. The reader
    /// drops its own at its next turn; this one is swapped for /dev/null
    /// in place, so the descriptor `write` and `drain` use stays valid.
    pub fn hang_up(&mut self) {
        self.drain();
        self.seen.lock().unwrap().hang_up = true;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.seen.lock().unwrap().closed && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let null = File::open("/dev/null").unwrap();
        // SAFETY: both descriptors are open and this struct's; dup2 closes
        // the master's in the same step.
        unsafe {
            libc::dup2(null.as_raw_fd(), self.master.as_raw_fd());
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

    /// As `wait_for`, on the output's words: its escape sequences taken out
    /// and every run of spaces and line breaks one space, so a hint the TUI
    /// wrapped (at a width a long path pushes it to) still matches.
    pub fn wait_for_words(&self, needle: &str, timeout: Duration) -> Option<Instant> {
        let needle = words(needle);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if words(&self.text()).contains(&needle) {
                return Some(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// The screen the output draws, a row a line, as far as the TUI's own
    /// drawing goes (`screen`). What `wait_for` cannot see: a frame draws
    /// only the cells that changed, so a row's words can arrive with a cell
    /// the frame before left in place between them, never in one run.
    pub fn screen(&self) -> String {
        screen(&self.output(), self.size)
    }

    /// As `wait_for`, on the screen.
    pub fn wait_on_screen(&self, needle: &str, timeout: Duration) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.screen().contains(needle) {
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
                self.drain();
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.kill();
        None
    }

    /// Waits until everything the terminal holds is in `output`. Exited is
    /// not read: the last bytes the child wrote can still be in the
    /// terminal or on their way through the reader thread, and a loaded
    /// machine lets a test look before they land. Nothing waiting on the
    /// master side, then one more turn of the reader's loop, says they have
    /// landed. The terminal closing is not waited for: a daemon the TUI
    /// started can hold it open long after.
    fn drain(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let turn = || self.seen.lock().unwrap().turns;
        loop {
            let mut waiting: libc::c_int = 0;
            // SAFETY: FIONREAD writes one int, the bytes ready to be read.
            let ok = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::FIONREAD as _, &mut waiting) } == 0;
            if self.seen.lock().unwrap().closed || (ok && waiting == 0) || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // Counted after nothing was left waiting: a turn that took the last
        // bytes and has not yet stored them has not been counted either.
        let from = turn();
        while turn() <= from && !self.seen.lock().unwrap().closed && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Kills the child and everything it started. It leads a session and
    /// process group of its own (`setsid`), so the group goes with it: a
    /// backend or helper left running would outlive the test, reparented
    /// to init, holding a sandbox that no longer exists.
    fn kill(&mut self) {
        // SAFETY: a signal to the process group our own child leads.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.kill();
    }
}

/// `text` without its escape sequences (CSI, OSC and two-byte ones), its
/// words one space apart.
pub fn words(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters up to a final byte, @ to ~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next().is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `out` played onto a `cols` by `rows` screen that starts blank with the
/// cursor at the top left (where the CSI 6n answer puts it): text, CR, LF
/// (scrolling at the bottom), backspace, the cursor moves and erases the
/// TUI draws with (CSI A B C D G H J K, ESC 7 and 8, autowrap on and off);
/// every other sequence, colour and OSC included, is read and dropped. A
/// character is one cell. The rows, trailing blanks trimmed, a line each.
pub fn screen(out: &[u8], (cols, rows): (u16, u16)) -> String {
    let (w, h) = (cols as usize, rows as usize);
    let mut grid = vec![vec![' '; w]; h];
    let (mut x, mut y, mut saved, mut wrap) = (0usize, 0usize, (0usize, 0usize), true);
    let text = String::from_utf8_lossy(out);
    let mut chars = text.chars().peekable();
    let down = |grid: &mut Vec<Vec<char>>, y: &mut usize| {
        if *y + 1 < h {
            *y += 1;
        } else {
            grid.remove(0);
            grid.push(vec![' '; w]);
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\r' => x = 0,
            '\n' => down(&mut grid, &mut y),
            '\x08' => x = x.saturating_sub(1),
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let fin = loop {
                        match chars.next() {
                            Some(p) if ('\x20'..='\x3f').contains(&p) => params.push(p),
                            other => break other,
                        }
                    };
                    let private = params.starts_with(['?', '>', '<', '=']);
                    let n: Vec<usize> = params.trim_start_matches(['?', '>', '<', '=']).split(';').map(|p| p.parse().unwrap_or(0)).collect();
                    let n0 = n[0].max(1);
                    match fin {
                        Some('h' | 'l') if private && params == "?7" => wrap = fin == Some('h'),
                        _ if private => {}
                        Some('A') => y = y.saturating_sub(n0),
                        Some('B') => y = (y + n0).min(h - 1),
                        Some('C') => x = (x + n0).min(w - 1),
                        Some('D') => x = x.saturating_sub(n0),
                        Some('G') => x = (n0 - 1).min(w - 1),
                        Some('H' | 'f') => (y, x) = ((n0 - 1).min(h - 1), (n.get(1).copied().unwrap_or(1).max(1) - 1).min(w - 1)),
                        Some('J') => {
                            let (from, to) = match n[0] {
                                0 => (y * w + x, w * h),
                                1 => (0, y * w + x + 1),
                                _ => (0, w * h),
                            };
                            for i in from..to.min(w * h) {
                                grid[i / w][i % w] = ' ';
                            }
                        }
                        Some('K') => {
                            let (from, to) = match n[0] {
                                0 => (x, w),
                                1 => (0, x + 1),
                                _ => (0, w),
                            };
                            grid[y][from..to.min(w)].fill(' ');
                        }
                        _ => {}
                    }
                }
                // OSC: to BEL or ST.
                Some(']') => {
                    while let Some(o) = chars.next() {
                        if o == '\x07' || (o == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                Some('7') => saved = (x, y),
                Some('8') => (x, y) = saved,
                _ => {}
            },
            c if c < ' ' => {}
            c => {
                if x >= w {
                    if wrap {
                        x = 0;
                        down(&mut grid, &mut y);
                    } else {
                        x = w - 1;
                    }
                }
                grid[y][x] = c;
                x += 1;
            }
        }
    }
    grid.iter().map(|r| r.iter().collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
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
