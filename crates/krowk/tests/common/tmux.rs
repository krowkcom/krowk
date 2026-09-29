//! A TUI in tmux, whose scrollback is what a person scrolls back through:
//! shared by `tui.rs` and `tui_daemon.rs`, which each use part of it.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct Tmux {
    socket: String,
}

impl Tmux {
    /// None when tmux is not installed and this is not CI.
    pub fn start(name: &str, cols: u16, rows: u16, cwd: &Path, env: &[(String, String)], args: &[&str]) -> Option<Tmux> {
        Tmux::start_after(name, cols, rows, cwd, env, args, "")
    }

    /// As `start`, with `before` run in the shell first — output a person's
    /// terminal already holds when they type `krowk`.
    pub fn start_after(name: &str, cols: u16, rows: u16, cwd: &Path, env: &[(String, String)], args: &[&str], before: &str) -> Option<Tmux> {
        if Command::new("tmux").arg("-V").output().is_err() {
            assert!(!cfg!(target_os = "linux") || std::env::var_os("CI").is_none(), "tmux is not installed, and CI on Linux must run this check");
            eprintln!("skipped: tmux is not installed");
            return None;
        }
        let socket = format!("krowk-tui-{name}-{}", std::process::id());
        let conf = std::env::temp_dir().join(format!("{socket}.conf"));
        std::fs::write(&conf, "set -g history-limit 100000\nset -g status off\n").unwrap();
        let mut line = format!("cd '{}' && {before} exec env -i", cwd.display());
        for (k, v) in env {
            line += &format!(" {k}='{v}'");
        }
        line += &format!(" '{}'", env!("CARGO_BIN_EXE_krowk"));
        for a in args {
            line += &format!(" '{a}'");
        }
        let st = Command::new("tmux")
            .args(["-L", &socket, "-f"])
            .arg(&conf)
            .args(["new-session", "-d", "-s", "t", "-x", &cols.to_string(), "-y", &rows.to_string(), &line])
            .status()
            .unwrap();
        assert!(st.success(), "tmux new-session: {st}");
        Some(Tmux { socket })
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux").args(["-L", &self.socket]).args(args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn keys(&self, keys: &[&str]) {
        let mut a = vec!["send-keys", "-t", "t"];
        a.extend_from_slice(keys);
        self.tmux(&a);
    }

    pub fn screen(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "t"])
    }

    pub fn history(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "t", "-S", "-", "-E", "-"])
    }

    pub fn wait_for(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if self.screen().contains(needle) {
                return Some(t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        None
    }

    /// As `wait_for`, over the whole history: a fast stream scrolls a line
    /// past the screen before a poll of the screen alone can see it.
    pub fn wait_in_history(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if self.history().contains(needle) {
                return Some(t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        None
    }

    /// The whole history once the TUI has stopped drawing: the same across
    /// two polls, with `ready` true of the screen.
    pub fn wait_still(&self, ready: impl Fn(&str) -> bool, timeout: Duration) -> Option<String> {
        let t0 = Instant::now();
        let mut last = self.history();
        while t0.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(80));
            let now = self.history();
            if now == last && ready(&self.screen()) {
                return Some(now);
            }
            last = now;
        }
        None
    }

    pub fn wait_gone(&self, needle: &str, timeout: Duration) -> Option<Duration> {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if !self.screen().contains(needle) {
                return Some(t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        None
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        // `kill-server` sends the pane a hangup, and a TUI that does not
        // leave on it would be left running, reparented to init, long after
        // the test. The pane's command runs in a session of its own, so its
        // process group goes with it, whatever it did with the hangup.
        let pane: Option<i32> = self.tmux(&["display-message", "-p", "-t", "t", "#{pane_pid}"]).trim().parse().ok();
        self.tmux(&["kill-server"]);
        if let Some(pid) = pane.filter(|p| *p > 1) {
            // SAFETY: a signal to the process group the pane's command leads.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_file(std::env::temp_dir().join(format!("{}.conf", self.socket)));
    }
}
