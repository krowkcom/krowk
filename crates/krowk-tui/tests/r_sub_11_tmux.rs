//! The child view's inline acceptance check against a real terminal: tmux.
//! What `Term` writes goes, as it is, into a tmux pane (a fifo `cat` reads in
//! raw mode), and the pane is read back with `capture-pane`, history and
//! all. Ignored by default — it needs tmux and takes a few seconds:
//!
//!     cargo test -p krowk-tui --test r_sub_11_tmux -- --ignored
//!
//! Without tmux it passes having checked nothing, and says so.

use krowk_tui::term::Term;
use ratatui::layout::Size;
use ratatui::text::Line;
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

/// What a `Term` writes, for the test to send on.
#[derive(Clone, Default)]
struct Out(Rc<RefCell<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A tmux server of its own, one pane reading `fifo`; gone when dropped.
struct Pane {
    socket: String,
    dir: PathBuf,
    fifo: PathBuf,
    /// Every byte sent, for what must never be.
    sent: Vec<u8>,
}

impl Pane {
    fn open(w: u16, h: u16) -> Option<Pane> {
        Command::new("tmux").arg("-V").output().ok()?;
        let dir = std::env::temp_dir().join(format!("krowk-sv4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let fifo = dir.join("in");
        assert!(Command::new("mkfifo").arg(&fifo).status().ok()?.success());
        let pane = Pane { socket: format!("krowk-sv4-{}", std::process::id()), dir, fifo, sent: Vec::new() };
        let cmd = format!("stty raw -echo; while :; do cat '{}'; done", pane.fifo.display());
        pane.tmux(&["-f", "/dev/null", "new-session", "-d", "-x", &w.to_string(), "-y", &h.to_string(), &cmd]);
        std::thread::sleep(Duration::from_millis(400));
        Some(pane)
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux").arg("-L").arg(&self.socket).args(args).output().expect("tmux");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.sent.extend_from_slice(bytes);
        let mut f = std::fs::OpenOptions::new().write(true).open(&self.fifo).unwrap();
        f.write_all(bytes).unwrap();
        drop(f);
        std::thread::sleep(Duration::from_millis(250));
    }

    fn pump(&mut self, out: &Out) {
        let bytes = std::mem::take(&mut *out.0.borrow_mut());
        if !bytes.is_empty() {
            self.send(&bytes);
        }
    }

    fn cursor_row(&self) -> u16 {
        self.tmux(&["display", "-p", "#{cursor_y}"]).trim().parse().unwrap()
    }

    fn resize(&self, w: u16, h: u16) {
        self.tmux(&["resize-window", "-x", &w.to_string(), "-y", &h.to_string()]);
        std::thread::sleep(Duration::from_millis(250));
    }

    /// The screen's rows.
    fn screen(&self) -> Vec<String> {
        self.tmux(&["capture-pane", "-p"]).lines().map(|l| l.trim_end().to_string()).collect()
    }

    /// History only.
    fn history(&self) -> Vec<String> {
        self.tmux(&["capture-pane", "-p", "-S", "-", "-E", "-1"]).lines().map(|l| l.trim_end().to_string()).collect()
    }

    /// History and screen, blank rows left out.
    fn all(&self) -> Vec<String> {
        self.tmux(&["capture-pane", "-p", "-S", "-"]).lines().map(|l| l.trim_end().to_string()).filter(|l| !l.is_empty()).collect()
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn lines(n: usize, what: &str) -> Vec<Line<'static>> {
    (0..n).map(|i| Line::from(format!("{what} {i}"))).collect()
}

fn view(h: u16) -> Vec<Line<'static>> {
    (0..h).map(|i| Line::from(format!("view {i}"))).collect()
}

#[test]
#[ignore = "needs tmux; run with --ignored"]
fn r_sub_11_in_tmux_the_view_fills_the_pane_and_closing_leaves_scrollback_and_prompt_as_they_were() {
    let (w, h) = (60u16, 16u16);
    let Some(mut pane) = Pane::open(w, h) else {
        eprintln!("no tmux: nothing checked");
        return;
    };
    let shell: Vec<u8> = (0..30).flat_map(|i| format!("shell line {i}\r\n").into_bytes()).collect();
    pane.send(&shell);
    let out = Out::default();
    let mut t = Term::new(out.clone(), Size { width: w, height: h }, pane.cursor_row(), 3).unwrap();
    let prompt = [Line::from("› prompt"), Line::default(), Line::from("status")];
    t.frame(&lines(40, "line"), &prompt, (2, 0)).unwrap();
    pane.pump(&out);
    let (history, before) = (pane.history(), pane.all());
    assert_eq!(pane.screen().last().map(String::as_str), Some("status"));

    // Open: the pane is the view's, history as it was.
    t.view(&[], &view(h)).unwrap();
    pane.pump(&out);
    assert_eq!(pane.screen(), (0..h).map(|i| format!("view {i}")).collect::<Vec<_>>());
    assert_eq!(pane.history(), history, "nothing printed into scrollback while open");

    // Resized while open: shorter, taller, narrower within its rows, wider.
    for (w, h) in [(60, 10), (60, 20), (50, 20), (70, 16)] {
        pane.resize(w, h);
        t.resize(Size { width: w, height: h }, Some(pane.cursor_row())).unwrap();
        t.view(&[], &view(h)).unwrap();
        pane.pump(&out);
        assert_eq!(pane.screen(), (0..h).map(|i| format!("view {i}")).collect::<Vec<_>>(), "redrawn at {w}x{h}");
    }
    pane.resize(60, 16);
    t.resize(Size { width: 60, height: 16 }, Some(pane.cursor_row())).unwrap();
    t.view(&[], &view(16)).unwrap();
    pane.pump(&out);

    // Closed: every row as it was, no stray row of the view, the prompt at
    // the bottom.
    t.frame(&[], &prompt, (2, 0)).unwrap();
    pane.pump(&out);
    assert_eq!(pane.all(), before);
    assert_eq!(pane.screen().last().map(String::as_str), Some("status"));
    let sent = String::from_utf8_lossy(&pane.sent).into_owned();
    for seq in ["\x1b[?1049", "\x1b[?1047", "\x1b[?47", "\x1b[?1000", "\x1b[?1002", "\x1b[?1003", "\x1b[?1006", "\x1b[2J"] {
        assert!(!sent.contains(seq), "{seq:?} sent");
    }
}
