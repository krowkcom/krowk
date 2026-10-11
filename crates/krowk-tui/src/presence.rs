//! What the session is doing, told to whatever hosts the terminal: the
//! window title (the one before krowk's saved at start and put back when
//! it leaves or stops) (`✳ krowk` waiting, `◑ <what was asked>` working, `✋` when
//! a call waits for a yes) and, inside herdr (a workspace manager for
//! coding agents, which sets `HERDR_PANE_ID`), the pane's agent state, so
//! herdr lists krowk as an agent with its status and notifies on it.
//! Beside the title, in the same write, the same state as the terminal's
//! root record for krowk (OSC 7501, `osc7501`), and a record of its own for
//! each subagent of the turn: what a terminal or an inbox that keeps them
//! reads instead of the title's glyphs, and one thing they don't say: how
//! the last turn ended, done or error, until the next key.

use crate::osc7501;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    /// An approval waits.
    Blocked,
    /// The agent's questions wait.
    Asking,
    /// `/connect`'s overlay is up, waiting on the person's sign-in.
    SigningIn,
    /// The last turn completed, and no key was pressed since: idle to the
    /// title and herdr, done to the root record.
    Done,
    /// The last turn failed, and no key was pressed since; its topic is
    /// the failure.
    Error,
}

impl State {
    /// The session's state: a call waiting on the person (`Some(true)`
    /// when it is the agent's questions) before a sign-in waiting on them,
    /// and either before a turn running. Answered, the sign-in still
    /// waiting is what is told again.
    pub fn of(waiting: Option<bool>, signing_in: bool, running: bool) -> State {
        match waiting {
            Some(true) => State::Asking,
            Some(false) => State::Blocked,
            None if signing_in => State::SigningIn,
            None if running => State::Working,
            None => State::Idle,
        }
    }

    fn herdr(self) -> &'static str {
        match self {
            State::Idle | State::Done | State::Error => "idle",
            State::Working => "working",
            State::Blocked | State::Asking | State::SigningIn => "blocked",
        }
    }

    fn record(self) -> osc7501::State {
        match self {
            State::Idle => osc7501::State::Idle,
            State::Working => osc7501::State::Working,
            State::Blocked => osc7501::State::Blocked(osc7501::Kind::Permission),
            State::Asking => osc7501::State::Blocked(osc7501::Kind::Question),
            State::SigningIn => osc7501::State::Blocked(osc7501::Kind::Auth),
            State::Done => osc7501::State::Done,
            State::Error => osc7501::State::Error,
        }
    }
}

/// What to write for a change, together: the window title and the root
/// record's report.
pub struct Told {
    pub title: String,
    pub report: String,
}

/// A child of the session as its own record (R-PRES-4): the session ids
/// from the session's child down to it, its title, and its state, `Clear`
/// once it was interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub path: Vec<String>,
    pub title: String,
    pub state: osc7501::State,
}

impl Child {
    /// Its report in `state`: the id the last 12 hex digits of each session
    /// id on its path (a UUIDv7's random part; the leading digits are a
    /// time its siblings share), a grandchild's under its parent's. None
    /// for a path past the id's limits: it goes unreported.
    fn report(&self, state: osc7501::State) -> Option<String> {
        let segments: Vec<String> = self
            .path
            .iter()
            .map(|sid| {
                let hex: Vec<char> = sid.chars().filter(char::is_ascii_hexdigit).collect();
                hex[hex.len().saturating_sub(12)..].iter().collect()
            })
            .collect();
        let id: Vec<&str> = segments.iter().map(String::as_str).collect();
        osc7501::encode(&osc7501::Report { state, id: &id, title: &self.title, ..Default::default() })
    }
}

/// The last state told, so each change is told once.
#[derive(Default)]
pub struct Presence {
    told: Option<(State, String)>,
    /// The children's records as last told.
    children: Vec<Child>,
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
            let mut started: Option<std::process::Child> = None;
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
        Presence { told: None, children: Vec::new(), pane, herdr }
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
        self.children.clear();
    }

    /// The reports that bring the children's records to `now` (R-PRES-4):
    /// one for each child whose state or title changed since last told, a
    /// clear for one interrupted, and a clear for each told no longer in
    /// `now` — all of them once the parent's turn has ended and `now` is
    /// empty. Written ahead of the root's report, so a turn's children are
    /// gone before the root says the turn ended.
    pub fn tell_children(&mut self, now: Vec<Child>) -> Vec<String> {
        use osc7501::State::Clear;
        let gone = self.children.iter().filter(|c| c.state != Clear && !now.iter().any(|n| n.path == c.path));
        let mut out: Vec<String> = gone.filter_map(|c| c.report(Clear)).collect();
        for c in &now {
            let was = self.children.iter().find(|t| t.path == c.path);
            let changed = match c.state {
                Clear => was.is_some_and(|t| t.state != Clear),
                _ => was != Some(c),
            };
            if changed {
                out.extend(c.report(c.state));
            }
        }
        self.children = now;
        out
    }

    /// The report that clears every record krowk made, when it made one:
    /// written on the way out and into a job stop, when the person is back
    /// at the shell and nothing krowk reported is current. `pause` or
    /// `finish` after it has the state told again from scratch.
    pub fn clear(&self) -> Option<String> {
        self.told.as_ref()?;
        osc7501::encode(&osc7501::Report { state: osc7501::State::Clear, ..Default::default() })
    }

    /// The title alone, of `tell`: what the title's own tests hold.
    #[cfg(test)]
    pub fn update(&mut self, state: State, topic: &str) -> Option<String> {
        self.tell(state, topic).map(|t| t.title)
    }

    /// The title to set now and the root record's report, if `state` and
    /// `topic` differ from what was last told; herdr is told in the
    /// background, and never waited on. The report's msg is the topic the
    /// title shows: none while idle, the prompt done with; the provider
    /// signed in to while signing in; for an error the failure's first
    /// line, whole, which the title doesn't show.
    pub fn tell(&mut self, state: State, topic: &str) -> Option<Told> {
        let topic: String = match state {
            State::Idle => String::new(),
            State::Error => crate::card::clean(topic.lines().next().unwrap_or_default()),
            _ => crate::card::clean(topic.lines().next().unwrap_or_default()).chars().take(48).collect(),
        };
        let now = (state, topic);
        if self.told.as_ref() == Some(&now) {
            return None;
        }
        let state_changed = self.told.as_ref().is_none_or(|(s, _)| s.herdr() != now.0.herdr());
        if state_changed && let (Some(pane), Some(r)) = (&self.pane, &self.herdr) {
            r.send(&["pane", "report-agent", pane, "--source", "krowk", "--agent", "krowk", "--state", now.0.herdr()]);
        }
        let title = match (now.0, now.1.is_empty()) {
            (State::Idle | State::Done | State::Error, _) | (_, true) => match now.0 {
                State::Idle | State::Done | State::Error => "✳ krowk".to_string(),
                State::Working => "◑ krowk".to_string(),
                State::Blocked | State::Asking | State::SigningIn => "✋ krowk".to_string(),
            },
            (State::Working, false) => format!("◑ {}", now.1),
            (State::Blocked | State::Asking | State::SigningIn, false) => format!("✋ {}", now.1),
        };
        let report = osc7501::encode(&osc7501::Report { state: now.0.record(), msg: &now.1, ..Default::default() }).unwrap_or_default();
        self.told = Some(now);
        Some(Told { title, report })
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
    fn r_pres_2_the_root_record_follows_the_title_and_is_cleared_once_told() {
        use osc7501::{Kind, Report, State as Rec};
        let root = |state, msg| osc7501::encode(&Report { state, msg, ..Default::default() }).unwrap();
        let mut p = Presence::from_env(&|_| String::new());
        assert_eq!(p.clear(), None, "nothing told, nothing to clear");
        assert_eq!(p.tell(State::Idle, "an old prompt").unwrap().report, root(Rec::Idle, ""), "idle names no prompt");
        assert!(p.tell(State::Idle, "another").is_none(), "nor is told again for one");
        assert_eq!(p.tell(State::Working, "fix it\nnow").unwrap().report, root(Rec::Working, "fix it"));
        assert_eq!(p.tell(State::Blocked, "fix it").unwrap().report, root(Rec::Blocked(Kind::Permission), "fix it"));
        assert_eq!(p.tell(State::Asking, "fix it").unwrap().report, root(Rec::Blocked(Kind::Question), "fix it"));
        assert_eq!(p.clear(), Some(root(Rec::Clear, "")));
        p.release();
        assert_eq!(p.clear(), None, "a job stop clears once");
        assert_eq!(p.tell(State::Asking, "fix it").unwrap().title, "✋ fix it", "told again from scratch");
    }

    #[test]
    fn r_pres_5_an_approval_wins_over_a_sign_in_and_the_sign_in_comes_back_once_it_is_answered() {
        use osc7501::{Kind, Report, State as Rec};
        let root = |state, msg| osc7501::encode(&Report { state, msg, ..Default::default() }).unwrap();
        let (sign_in, ask) = ("Sign in to anthropic", "fix it");
        let mut p = Presence::from_env(&|_| String::new());
        let mut tell = |waiting, running| {
            let state = State::of(waiting, true, running);
            p.tell(state, if state == State::SigningIn { sign_in } else { ask }).map(|t| (t.title, t.report))
        };
        assert_eq!(tell(None, false), Some(("✋ Sign in to anthropic".into(), root(Rec::Blocked(Kind::Auth), sign_in))));
        assert_eq!(tell(None, true).map(|t| t.1), None, "a turn running waits on the sign-in too");
        assert_eq!(tell(Some(false), true).map(|t| t.1), Some(root(Rec::Blocked(Kind::Permission), ask)));
        assert_eq!(tell(None, true).map(|t| t.1), Some(root(Rec::Blocked(Kind::Auth), sign_in)), "answered, the sign-in again");
        assert_eq!(tell(Some(true), true).map(|t| t.1), Some(root(Rec::Blocked(Kind::Question), ask)));
        assert_eq!(tell(None, false).map(|t| t.1), Some(root(Rec::Blocked(Kind::Auth), sign_in)));
        assert_eq!(State::of(None, false, true), State::Working, "the overlay closed, what it would otherwise be");
        assert_eq!(State::of(None, false, false), State::Idle);
    }

    #[test]
    fn r_pres_3_a_turn_ends_done_or_error_on_the_record_and_idle_to_the_title_and_herdr() {
        use osc7501::{Report, State as Rec};
        let root = |state, msg| osc7501::encode(&Report { state, msg, ..Default::default() }).unwrap();
        let mut p = Presence::from_env(&|_| String::new());
        let done = p.tell(State::Done, "fix it\nnow").unwrap();
        assert_eq!((done.title.as_str(), done.report), ("✳ krowk", root(Rec::Done, "fix it")));
        let failure = format!("{} — run it again\nwhy", "x".repeat(60));
        let error = p.tell(State::Error, &failure).unwrap();
        assert_eq!((error.title.as_str(), error.report), ("✳ krowk", root(Rec::Error, &failure[..failure.find('\n').unwrap()])), "the whole first line, longer than the title's");
        assert_eq!(p.tell(State::Idle, "fix it").unwrap().report, root(Rec::Idle, ""));
        assert!([State::Done, State::Error].iter().all(|s| s.herdr() == "idle"), "herdr hears idle, as before");
    }

    #[test]
    fn r_pres_4_each_child_is_told_once_per_change_and_cleared_when_gone() {
        use osc7501::{Kind, Report, State as Rec};
        let sid = "0199a3c2-7e10-7a3b-9c41-5f2e8d6b1a07";
        let child = |path: &[&str], title: &str, state| Child { path: path.iter().map(|s| s.to_string()).collect(), title: title.into(), state };
        let rec = |id: &[&str], title, state| osc7501::encode(&Report { state, id, title, ..Default::default() }).unwrap();
        let mut p = Presence::from_env(&|_| String::new());
        assert_eq!(p.tell_children(vec![child(&[sid], "review", Rec::Working)]), [rec(&["5f2e8d6b1a07"], "review", Rec::Working)], "the last 12 hex digits");
        assert!(p.tell_children(vec![child(&[sid], "review", Rec::Working)]).is_empty(), "nothing new, nothing told");
        let deep = child(&[sid, "0199a3c2-7e11-7000-8000-00000000beef"], "dig", Rec::Blocked(Kind::Permission));
        assert_eq!(p.tell_children(vec![child(&[sid], "review", Rec::Working), deep.clone()]), [rec(&["5f2e8d6b1a07", "00000000beef"], "dig", Rec::Blocked(Kind::Permission))], "a grandchild under its parent");
        let nine: Vec<String> = (0..9).map(|i| format!("0199a3c2-7e11-7000-8000-00000000000{i}")).collect();
        let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
        assert!(p.tell_children(vec![child(&[sid], "review", Rec::Working), deep.clone(), child(&nine, "too deep", Rec::Working)]).is_empty(), "past the limits, unreported");
        assert_eq!(p.tell_children(vec![child(&[sid], "review", Rec::Done), deep.clone()]), [rec(&["5f2e8d6b1a07"], "review", Rec::Done)]);
        assert_eq!(p.tell_children(vec![child(&[sid], "review", Rec::Done), child(&deep.path.iter().map(String::as_str).collect::<Vec<_>>(), "dig", Rec::Clear)]), [rec(&["5f2e8d6b1a07", "00000000beef"], "", Rec::Clear)], "interrupted, cleared");
        assert_eq!(p.tell_children(Vec::new()), [rec(&["5f2e8d6b1a07"], "", Rec::Clear)], "the turn over: the rest cleared, the cleared not again");
        assert!(p.tell_children(Vec::new()).is_empty());
        p.tell_children(vec![child(&[sid], "review", Rec::Working)]);
        p.release();
        assert_eq!(p.tell_children(vec![child(&[sid], "review", Rec::Working)]), [rec(&["5f2e8d6b1a07"], "review", Rec::Working)], "told again from scratch after a job stop");
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
