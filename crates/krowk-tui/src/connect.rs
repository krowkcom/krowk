//! `/connect`, `/disconnect` and `/rename`: the harness's one sign-in
//! (`krowk_harness::connect`), asked and told through an overlay.
//!
//! The sign-in is synchronous and asks as it goes — which vendor, which
//! way in, which account, a key — so it runs on a thread of its own, and
//! its `AuthInteraction` (`Asker`) turns each question into a message to
//! the loop and waits for the answer. The loop never waits on it: a
//! vendor's status check or a browser sign-in takes as long as it takes,
//! and keys, resizes and the running clock go on meanwhile.
//!
//! A vendor's own login (`claude auth login`, `codex login`) and a key's
//! `!command` need the real terminal. `Asker::terminal` asks the loop to
//! give it up — the live region cleared, raw mode off, the key reader
//! stopped, as a job stop does — runs the command, and hands it back; the
//! loop then redraws where the cursor is. Everything else stays in the
//! overlay: a SuperGrok sign-in's page and device code, and a pasted key,
//! which is shown as bullets and never drawn back.

use crate::app::{clip, dim};
use crate::look;
use krowk_harness::connect::{Answer, AuthInteraction, Connected, Disconnected, Notice, Options, Prompt, ProviderAuth, Renamed};
use krowk_harness::engine::EngineError;
use krowk_harness::instances::Registry;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::mpsc as sync;
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

/// What to run.
pub enum Job {
    /// A vendor picked here, or an instance or vendor named
    /// (`/connect claude:work`).
    Connect(Option<String>),
    /// The instance named, else one picked.
    Disconnect(Option<String>),
    /// The instance named, else one picked, and its new name, else asked
    /// (`/rename claude:work claude:personal`).
    Rename(Option<String>, Option<String>),
}

/// What a job did.
pub enum Done {
    Connected(Box<Connected>),
    Disconnected(Disconnected),
    Renamed(Renamed),
}

/// From the sign-in's thread to the loop.
pub enum Msg {
    /// A question; the answer, or none for "cancelled", goes back on `reply`.
    Ask { ask: Ask, reply: sync::Sender<Option<Answer>> },
    Note(Note),
    /// The terminal is wanted: the loop gives it up, then says so on `ready`.
    Suspend { ready: sync::Sender<()> },
    /// The command that wanted it is done: the loop takes it back.
    Resume,
    /// The job's end, with the instances read again after it (config.json
    /// and the credentials file) — none when they could not be.
    Done(Result<Done, EngineError>, Option<Box<Registry>>),
}

pub enum Ask {
    Select { message: String, options: Vec<String> },
    /// `initial` is what the input starts as, typed after.
    Text { message: String, secret: bool, initial: String },
}

/// What the sign-in tells the person, owned to cross threads.
pub enum Note {
    Info(String),
    Url { message: String, url: String },
    Code { url: String, code: String, message: String },
}

/// Where the sign-in reads and writes.
#[derive(Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub credentials: PathBuf,
}

/// Runs `job` on a thread of its own; what it asks and tells, and its end,
/// arrive on the receiver.
pub fn start(job: Job, paths: Paths) -> mpsc::UnboundedReceiver<Msg> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let pa = ProviderAuth { config: paths.config, credentials: paths.credentials, env: &env };
        let mut ui = Asker { tx: tx.clone() };
        let done = match job {
            Job::Connect(target) => pa.request(target.as_deref(), None, Options::default(), &mut ui).and_then(|req| pa.connect(&req, &mut ui)).map(|c| Done::Connected(Box::new(c))),
            Job::Disconnect(target) => pa.disconnect_target(target.as_deref(), &mut ui).and_then(|i| pa.disconnect(&i, false, false, &mut ui)).map(Done::Disconnected),
            Job::Rename(target, new) => pa.rename_target(target.as_deref(), new.as_deref(), &mut ui).and_then(|(from, new)| pa.rename(&from, &new)).map(Done::Renamed),
        };
        let registry = pa.definitions().ok().map(|d| Box::new(Registry::resolve(&d, &env)));
        let _ = tx.send(Msg::Done(done, registry));
    });
    rx
}

/// The person, as the sign-in's thread reaches them: through the loop.
struct Asker {
    tx: mpsc::UnboundedSender<Msg>,
}

/// Hands the terminal back however the command that had it ended.
struct GiveBack<'a>(&'a mpsc::UnboundedSender<Msg>);

impl Drop for GiveBack<'_> {
    fn drop(&mut self) {
        let _ = self.0.send(Msg::Resume);
    }
}

fn cancelled() -> EngineError {
    EngineError::new("selection_cancelled", "nothing was chosen and nothing was changed")
}

impl AuthInteraction for Asker {
    fn prompt(&mut self, prompt: Prompt<'_>) -> Result<Answer, EngineError> {
        let ask = match prompt {
            Prompt::Text { message, initial, .. } => Ask::Text { message: message.into(), secret: false, initial: initial.into() },
            Prompt::Secret { message, .. } => Ask::Text { message: message.into(), secret: true, initial: String::new() },
            Prompt::Select { message, options, .. } => Ask::Select { message: message.into(), options: options.iter().map(|o| o.to_string()).collect() },
        };
        let (reply, answer) = sync::channel();
        self.tx.send(Msg::Ask { ask, reply }).map_err(|_| cancelled())?;
        answer.recv().ok().flatten().ok_or_else(cancelled)
    }

    fn notify(&mut self, notice: Notice<'_>) {
        let note = match notice {
            Notice::Info(s) | Notice::Progress(s) => Note::Info(s.into()),
            Notice::AuthUrl { url, message } => Note::Url { message: message.into(), url: url.into() },
            Notice::DeviceCode { url, code, message } => Note::Code { url: url.into(), code: code.into(), message: message.into() },
        };
        let _ = self.tx.send(Msg::Note(note));
    }

    fn terminal(&mut self, run: &mut dyn FnMut() -> Result<ExitStatus, String>) -> Result<ExitStatus, String> {
        let (ready, given) = sync::channel();
        self.tx.send(Msg::Suspend { ready }).map_err(|_| "the TUI is gone".to_string())?;
        given.recv().map_err(|_| "the TUI did not give up the terminal".to_string())?;
        let _back = GiveBack(&self.tx);
        run()
    }
}

/// The overlay while a job runs: its title, the first-run words, the
/// question up now and where the answer goes.
pub struct Flow {
    pub title: &'static str,
    pub intro: Vec<String>,
    pub ask: Option<Asking>,
    /// What it is doing while nothing is asked.
    pub busy: String,
    /// An option a pick starts on: `/disconnect` alone starts on the
    /// session's instance.
    pub prefer: Option<String>,
    /// When the question up now was first shown: keys before a moment
    /// has passed were typed ahead, and answer nothing.
    shown_at: Option<std::time::Instant>,
}

pub struct Asking {
    pub message: String,
    pub kind: Kind,
    reply: sync::Sender<Option<Answer>>,
}

pub enum Kind {
    Select { options: Vec<String>, at: usize },
    /// Typed or pasted; a secret's is never drawn.
    Text { input: String, secret: bool },
}

impl Flow {
    pub fn new(title: &'static str, intro: Vec<String>, prefer: Option<String>) -> Flow {
        Flow { title, intro, ask: None, busy: "checking what is connected…".into(), prefer, shown_at: None }
    }

    /// The question up now is on screen, from now.
    pub fn shown(&mut self) {
        if self.ask.is_some() {
            self.shown_at = Some(std::time::Instant::now());
        }
    }

    /// Whether a question is on screen and has been for `settle`: only then
    /// does a key answer it.
    pub fn settled(&self, settle: std::time::Duration) -> bool {
        self.ask.is_some() && self.shown_at.is_some_and(|t| t.elapsed() >= settle)
    }

    /// A question arrived: shown, a pick starting on the preferred option.
    pub fn asked(&mut self, ask: Ask, reply: sync::Sender<Option<Answer>>) {
        let kind = match ask {
            Ask::Select { message, options } => {
                let at = self.prefer.as_ref().and_then(|p| options.iter().position(|o| o == p || o.starts_with(&format!("{p} (")))).unwrap_or(0);
                (message, Kind::Select { options, at })
            }
            Ask::Text { message, secret, initial } => (message, Kind::Text { input: initial, secret }),
        };
        self.ask = Some(Asking { message: kind.0, kind: kind.1, reply });
        self.shown_at = None;
    }

    /// Answers the question up now, or cancels it (`None`).
    pub fn answer(&mut self, answer: Option<Answer>) {
        if let Some(a) = self.ask.take() {
            self.busy = if answer.is_some() { "working…".into() } else { "cancelling…".into() };
            let _ = a.reply.send(answer);
        }
    }

    /// Enter: the choice, or the text typed.
    pub fn enter(&mut self) {
        let answer = match self.ask.as_mut().map(|a| &mut a.kind) {
            Some(Kind::Select { at, .. }) => Answer::Choice(*at),
            Some(Kind::Text { input, .. }) => Answer::Text(std::mem::take(input)),
            None => return,
        };
        self.answer(Some(answer));
    }

    pub fn step(&mut self, by: isize) {
        if let Some(Asking { kind: Kind::Select { options, at }, .. }) = &mut self.ask {
            *at = at.saturating_add_signed(by).min(options.len().saturating_sub(1));
        }
    }

    /// Text typed or pasted into a text question; true when it took it.
    /// A secret keeps what was pasted as it was, line breaks and all, for
    /// the sign-in to refuse as `krowk connect` does ("a key is one line");
    /// joined up it would be another key. A name or a URL keeps no control
    /// character.
    pub fn type_str(&mut self, s: &str) -> bool {
        match &mut self.ask {
            Some(Asking { kind: Kind::Text { input, secret: true }, .. }) => {
                input.push_str(s);
                true
            }
            Some(Asking { kind: Kind::Text { input, .. }, .. }) => {
                input.extend(s.chars().filter(|c| !c.is_control()));
                true
            }
            _ => false,
        }
    }

    pub fn backspace(&mut self) {
        if let Some(Asking { kind: Kind::Text { input, .. }, .. }) = &mut self.ask {
            input.pop();
        }
    }

    pub fn clear_input(&mut self) {
        if let Some(Asking { kind: Kind::Text { input, .. }, .. }) = &mut self.ask {
            input.clear();
        }
    }

    pub fn typing(&self) -> bool {
        matches!(&self.ask, Some(Asking { kind: Kind::Text { .. }, .. }))
    }

    /// The overlay's rows, and the column and row of the caret when a text
    /// question takes the keys.
    pub fn rows(&self, width: usize) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
        let blue = Style::new().fg(ratatui::style::Color::Blue);
        let mut rows = vec![Line::from(Span::styled(clip(self.title, width), look::accent().add_modifier(Modifier::BOLD)))];
        for l in &self.intro {
            rows.extend(crate::app::wrap(l, width).into_iter().map(|r| Line::from(Span::styled(r, dim()))));
        }
        if !self.intro.is_empty() {
            rows.push(Line::default());
        }
        let mut caret = None;
        let hint = match &self.ask {
            None => {
                rows.push(Line::from(Span::styled(clip(&self.busy, width), dim())));
                "esc hides this · it goes on, and says here when it is done"
            }
            Some(a) => {
                rows.extend(crate::app::wrap(&crate::app::clean(&a.message), width).into_iter().map(Line::from));
                match &a.kind {
                    Kind::Select { options, at } => {
                        for (i, o) in options.iter().enumerate() {
                            let chosen = i == *at;
                            let text = clip(&format!("{}{o}", if chosen { "❯ " } else { "  " }), width);
                            rows.push(Line::from(Span::styled(text, if chosen { look::accent() } else { blue })));
                        }
                        "↑ ↓ choose · enter picks · esc cancels"
                    }
                    Kind::Text { input, secret } => {
                        // A secret is a bullet a character, never itself.
                        let shown = if *secret { "•".repeat(input.chars().count()) } else { crate::app::clean(input) };
                        let room = width.saturating_sub(2).max(1);
                        // The end of what was typed, where the caret is.
                        let tail: String = {
                            let mut w = 0;
                            let mut kept: Vec<char> = shown.chars().rev().take_while(|c| {
                                w += unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
                                w < room
                            }).collect();
                            kept.reverse();
                            kept.into_iter().collect()
                        };
                        caret = Some(((2 + tail.width()) as u16, rows.len() as u16));
                        rows.push(Line::from(vec![Span::styled(look::ARROW, look::prompt()), Span::raw(tail)]));
                        if *secret { "enter stores it · esc cancels · what is pasted is never shown" } else { "enter answers · esc cancels" }
                    }
                }
            }
        };
        rows.push(Line::from(Span::styled(clip(hint, width), dim())));
        (rows, caret)
    }
}

impl Drop for Flow {
    /// A question nobody will answer now is a cancel, so the sign-in's
    /// thread is never left waiting.
    fn drop(&mut self) {
        self.answer(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Line]) -> String {
        rows.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn a_pasted_key_is_never_drawn_and_goes_to_the_sign_in_whole() {
        let mut f = Flow::new("Connect a provider", Vec::new(), None);
        let (reply, answer) = sync::channel();
        f.asked(Ask::Text { message: "Paste a key".into(), secret: true, initial: String::new() }, reply);
        assert!(f.type_str("sk-ant-secret\n-123"));
        let (rows, caret) = f.rows(60);
        let shown = text(&rows);
        assert!(!shown.contains("secret") && !shown.contains("123") && shown.contains("•••"), "{shown}");
        assert_eq!(caret.map(|c| c.1), Some(2), "the caret on the input row");
        f.enter();
        assert!(matches!(answer.recv().unwrap(), Some(Answer::Text(t)) if t == "sk-ant-secret\n-123"), "kept as pasted, for the sign-in to refuse");
    }

    #[test]
    fn a_pick_starts_on_the_preferred_option_and_an_abandoned_question_is_a_cancel() {
        let mut f = Flow::new("Disconnect", Vec::new(), Some("claude:work".into()));
        let (reply, answer) = sync::channel();
        f.asked(Ask::Select { message: "Which?".into(), options: vec!["anthropic (Anthropic API key)".into(), "claude:work (Claude subscription)".into()] }, reply);
        assert!(text(&f.rows(60).0).contains("❯ claude:work"));
        drop(f);
        assert!(answer.recv().unwrap().is_none(), "dropped unanswered, it cancels");
    }
}
