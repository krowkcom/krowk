//! A spinner on stderr while bytes move — transient decoration, drawn only on
//! a terminal, and cleared before anything durable is printed.

use std::io::Write;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const INTERVAL: Duration = Duration::from_millis(80);
/// Back to the start of the line and clear to its end. The spinner never
/// prints a newline, so this is always the line it wrote.
const ERASE: &str = "\r\x1b[K";

/// A spinner that was never started — piped output, JSON — is a no-op, so
/// the caller writes the same lines whether or not anybody is watching.
pub struct Spinner {
    say: Arc<Mutex<String>>,
    running: Option<(Sender<()>, JoinHandle<()>)>,
}

impl Spinner {
    pub fn start(show: bool, say: &str) -> Spinner {
        let say = Arc::new(Mutex::new(say.to_string()));
        if !show {
            return Spinner { say, running: None };
        }
        let (stop, stopped) = channel();
        let words = Arc::clone(&say);
        let handle = std::thread::spawn(move || {
            let mut err = std::io::stderr();
            for i in 0.. {
                let text = words.lock().map(|s| s.clone()).unwrap_or_default();
                let _ = write!(err, "{ERASE}{}", super::paint(true, "2", &format!("{} {text}", FRAMES[i % FRAMES.len()])));
                let _ = err.flush();
                match stopped.recv_timeout(INTERVAL) {
                    Err(RecvTimeoutError::Timeout) => continue,
                    _ => break,
                }
            }
            let _ = write!(err, "{ERASE}");
            let _ = err.flush();
        });
        Spinner { say, running: Some((stop, handle)) }
    }

    /// Follows which file is moving.
    pub fn say(&self, say: &str) {
        if let Ok(mut s) = self.say.lock() {
            *s = say.to_string();
        }
    }
}

impl Drop for Spinner {
    /// Stopping waits for the line to be cleared, so a frame can never land on
    /// top of the result printed after it.
    fn drop(&mut self) {
        if let Some((stop, handle)) = self.running.take() {
            let _ = stop.send(());
            let _ = handle.join();
        }
    }
}
