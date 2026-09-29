//! What the session is doing, told to whatever hosts the terminal: the
//! window title (the one before krowk's saved at start and put back when
//! it leaves or stops) (`✳ krowk` waiting, `◑ <what was asked>` working, `✋` when
//! a call waits for a yes) and, inside herdr (a workspace manager for
//! coding agents, which sets `HERDR_PANE_ID`), the pane's agent state, so
//! herdr lists krowk as an agent with its status and notifies on it.

use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    fn herdr(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked => "blocked",
        }
    }
}

/// The last state told, so each change is told once.
#[derive(Default)]
pub struct Presence {
    told: Option<(State, String)>,
    pane: Option<String>,
    herdr: Option<Reporter>,
}

/// herdr's commands, run one at a time and in order on a thread of their
/// own — each waited for, so none is left a zombie and a later state never
/// overtakes an earlier one — and never waited on by the TUI. The release
/// is started and let go: krowk leaving, or stopping for a job stop, waits
/// for it to be under way, not for herdr to answer it (~100 ms at times).
/// A command after it waits for it first, so the order still holds.
struct Reporter {
    tx: mpsc::Sender<Job>,
    done: mpsc::Receiver<()>,
}

enum Job {
    Run(Vec<String>),
    /// Started, and waited for only by the next command.
    Start(Vec<String>),
    /// Answered once everything before it has run.
    Flush(mpsc::Sender<()>),
}

impl Reporter {
    fn start() -> Reporter {
        let (tx, rx) = mpsc::channel::<Job>();
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let herdr = |args: &[String]| Command::new("herdr").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
            let mut started: Option<Child> = None;
            for job in rx {
                match job {
                    Job::Run(args) => {
                        if let Some(mut c) = started.take() {
                            let _ = c.wait();
                        }
                        if let Some(mut c) = herdr(&args) {
                            let _ = c.wait();
                        }
                    }
                    Job::Start(args) => {
                        if let Some(mut c) = started.take() {
                            let _ = c.wait();
                        }
                        started = herdr(&args);
                    }
                    Job::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
            let _ = done_tx.send(());
        });
        Reporter { tx, done }
    }

    fn send(&self, args: &[&str]) {
        let _ = self.tx.send(Job::Run(args.iter().map(|a| a.to_string()).collect()));
    }

    fn send_detached(&self, args: &[&str]) {
        let _ = self.tx.send(Job::Start(args.iter().map(|a| a.to_string()).collect()));
    }

    /// Waits, at most `limit`, for what is queued to have run.
    fn flush(&self, limit: Duration) {
        let (ack, wait) = mpsc::channel();
        if self.tx.send(Job::Flush(ack)).is_ok() {
            let _ = wait.recv_timeout(limit);
        }
    }
}

impl Presence {
    /// herdr's pane, when krowk's terminal is that pane: inside tmux or
    /// screen started in it, the variable is inherited from the pane
    /// around them, and the state is not krowk's to report.
    pub fn from_env(env: &dyn Fn(&str) -> String) -> Presence {
        let pane = env("HERDR_PANE_ID");
        let nested = !env("TMUX").is_empty() || !env("STY").is_empty();
        let pane = (!pane.is_empty() && !nested).then_some(pane);
        let herdr = pane.as_ref().map(|_| Reporter::start());
        Presence { told: None, pane, herdr }
    }

    /// The last word to herdr on the way out: released, and a moment given
    /// for what is still queued to be under way.
    pub fn finish(&mut self) {
        self.release();
        if let Some(r) = self.herdr.take() {
            drop(r.tx);
            let _ = r.done.recv_timeout(Duration::from_millis(500));
        }
    }

    /// Before a job stop, which stops the reporting thread with the rest of
    /// the process: released, and the release given a moment to start.
    pub fn pause(&mut self) {
        self.release();
        if let Some(r) = &self.herdr {
            r.flush(Duration::from_millis(300));
        }
    }

    /// Gives herdr its own detection of the pane back, on the way out or
    /// into a job stop; told again from scratch afterwards.
    pub fn release(&mut self) {
        if let (Some(pane), Some(r)) = (&self.pane, &self.herdr)
            && self.told.is_some()
        {
            r.send_detached(&["pane", "release-agent", pane, "--source", "krowk", "--agent", "krowk"]);
        }
        self.told = None;
    }

    /// The title to set now, if `state` and `topic` differ from what was
    /// last told; herdr is told in the background, and never waited on.
    pub fn update(&mut self, state: State, topic: &str) -> Option<String> {
        let topic: String = crate::card::clean(topic.lines().next().unwrap_or_default()).chars().take(48).collect();
        let now = (state, topic);
        if self.told.as_ref() == Some(&now) {
            return None;
        }
        let state_changed = self.told.as_ref().is_none_or(|(s, _)| *s != now.0);
        if state_changed && let (Some(pane), Some(r)) = (&self.pane, &self.herdr) {
            r.send(&["pane", "report-agent", pane, "--source", "krowk", "--agent", "krowk", "--state", now.0.herdr()]);
        }
        let title = match (now.0, now.1.is_empty()) {
            (State::Idle, _) | (_, true) => match now.0 {
                State::Idle => "✳ krowk".to_string(),
                State::Working => "◑ krowk".to_string(),
                State::Blocked => "✋ krowk".to_string(),
            },
            (State::Working, false) => format!("◑ {}", now.1),
            (State::Blocked, false) => format!("✋ {}", now.1),
        };
        self.told = Some(now);
        Some(title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_change_is_told_once_and_the_title_names_what_was_asked() {
        let mut p = Presence::from_env(&|_| String::new());
        assert_eq!(p.update(State::Idle, "").as_deref(), Some("✳ krowk"));
        assert_eq!(p.update(State::Idle, ""), None, "nothing new");
        assert_eq!(p.update(State::Working, "fix the failing test\nand more").as_deref(), Some("◑ fix the failing test"));
        assert_eq!(p.update(State::Blocked, "fix the failing test").as_deref(), Some("✋ fix the failing test"));
        assert_eq!(p.update(State::Working, "evil\x1b]0;x\x07").as_deref(), Some("◑ evil]0;x"), "no escape reaches the title");
        assert_eq!(p.update(State::Working, "txt.\u{202E}exe").as_deref(), Some("◑ txt.exe"), "nor a control that reorders it");
    }

    #[test]
    fn herdr_is_told_only_about_its_own_pane() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()).unwrap_or_default();
        assert_eq!(Presence::from_env(&env(&[("HERDR_PANE_ID", "w1:p2")])).pane.as_deref(), Some("w1:p2"));
        assert_eq!(Presence::from_env(&env(&[("HERDR_PANE_ID", "w1:p2"), ("TMUX", "/tmp/tmux-1000/default,1,0")])).pane, None, "tmux inside the pane");
        assert_eq!(Presence::from_env(&env(&[("HERDR_PANE_ID", "w1:p2"), ("STY", "123.pts-0")])).pane, None, "screen inside the pane");
        assert_eq!(Presence::from_env(&env(&[])).pane, None);
    }
}
