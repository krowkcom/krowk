//! The child view (R-SUB-11): a full-height view of one child's
//! transcript — a header, a window of its lines that scrolls, a footer —
//! drawn where the screen gives it room (`Screen::view_rows`): inline, the
//! whole terminal as the live region; fullscreen, the conversation's rows
//! above the prompt and the status line. Neither switches screens for it.
//!
//! Its body is a second, headless App on the child's session that keeps
//! what it would print (`App::keep`): the child's history read from its
//! log — the parent's prompt first — then its lines as they arrive, which
//! the main App hands on (`on_line`) while it folds them into the child's
//! row as ever. Under the kept lines, while the window follows the end,
//! what the child has not finished yet (`App::tail`): the text streaming,
//! the call out.

use crate::app::App;
use crate::look;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use krowk_harness::protocol::{LogBody, LogEvent, StreamLine};
use ratatui::text::{Line, Span};
use std::collections::HashSet;

pub struct ChildView {
    title: String,
    /// The child's session.
    session_id: String,
    /// The child's App: its transcript, kept.
    app: Box<App>,
    /// The ids of the events its log gave: the same events arriving live
    /// (read and sent while the view opened) are not drawn twice.
    read: HashSet<String>,
    /// The columns the body is laid out in.
    width: u16,
}

/// What a key did to the view.
#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    /// Taken: scrolled, or nothing to do.
    Taken,
    /// Esc: the view closes.
    Close,
    /// Not the view's: Ctrl-C and Ctrl-D go on as anywhere.
    Pass,
}

impl ChildView {
    /// A view titled `title` on child `session_id`, drawn by `app`: its
    /// history first, then the item it is in the middle of (`under_way`:
    /// its id, its start and what it has streamed so far), unless its log
    /// already has it whole, then live. A log that could not be read is
    /// said, and what arrives from now on is still shown.
    pub fn open(title: &str, session_id: &str, mut app: App, history: Result<Vec<LogEvent>, String>, under_way: Option<(String, [StreamLine; 2])>) -> ChildView {
        app.keep();
        let mut read = HashSet::new();
        let mut done = false;
        match history {
            Ok(events) => {
                let head = events.last().map(|e| e.id.clone()).unwrap_or_default();
                app.replay(&krowk_harness::log::branch(&events, &head));
                done = under_way.as_ref().is_some_and(|(id, _)| events.iter().any(|e| matches!(&e.body, LogBody::ItemCompleted { item_id, .. } if item_id == id)));
                read.extend(events.into_iter().map(|e| e.id));
            }
            Err(e) => app.say(&format!("its history could not be read here ({e}) — what it does from now on is shown"), look::dim()),
        }
        app.session_id = Some(session_id.to_string());
        for line in under_way.filter(|_| !done).into_iter().flat_map(|(_, lines)| lines) {
            app.on_line(&line);
        }
        // None yet: the first frame lays it out at the width it is drawn at.
        ChildView { title: crate::card::clean(title), session_id: session_id.to_string(), app: Box::new(app), read, width: 0 }
    }

    /// A view titled `title` on `lines` alone, at `width` columns.
    #[cfg(test)]
    pub fn new(title: &str, lines: impl IntoIterator<Item = Line<'static>>, width: u16) -> ChildView {
        let mut app = App::new(crate::editor::Editor::new(None), width, crate::settings::Settings::default(), None, None);
        for line in lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            app.say(&text, ratatui::style::Style::new());
        }
        ChildView::open(title, "child", app, Ok(Vec::new()), None)
    }

    /// A line of the stream: the child's own are its App's, once. Whether
    /// it was: the view is drawn again for it.
    pub fn on_line(&mut self, line: &StreamLine) -> bool {
        let ours = match line {
            StreamLine::Log(ev) => ev.session_id == self.session_id && !self.read.remove(&ev.id),
            StreamLine::Live(_) => crate::app::line_session(line) == Some(self.session_id.as_str()),
        };
        if ours {
            self.app.on_line(line);
        }
        ours
    }

    /// The view's `height` rows at `width` columns: its header, the body's
    /// window, its footer. The body is wrapped again when the width changed.
    /// `waiting`, when an approval waits on the person, takes the footer's
    /// place: it says so and how to get to it.
    pub fn rows(&mut self, width: u16, height: u16, waiting: Option<&str>) -> Vec<Line<'static>> {
        let width = width.max(1);
        if width != self.width {
            self.width = width;
            self.app.set_width(width);
        }
        self.app.take_pending();
        let h = usize::from(height);
        let mut rows = vec![Line::from(Span::styled(clip(&self.title, width), look::bold()))];
        if h < 3 {
            rows.resize(h, Line::default());
            return rows;
        }
        let body = height - 2;
        let following = self.app.kept().is_none_or(|k| k.following());
        // What is unfinished, under the kept lines, at most half the body.
        let mut tail = if following { self.app.tail(std::time::Instant::now()) } else { Vec::new() };
        tail.drain(..tail.len().saturating_sub(usize::from(body / 2)));
        let Some(kept) = self.app.kept() else { return rows };
        kept.set_height(body - tail.len() as u16);
        rows.extend(kept.window().cloned());
        let below = kept.below();
        rows.extend(tail);
        if below > 0 {
            rows.push(Line::from(Span::styled(format!("↓ {below} more below · PgDn"), look::dim())));
        }
        rows.resize(h - 1, Line::default());
        rows.push(Line::from(match waiting {
            Some(note) => look::keys(note, look::warning()),
            None => look::keys("`esc` back · `↑` `↓` `PgUp` `PgDn` scroll", look::dim()),
        }));
        rows
    }

    /// ↑ ↓ PgUp PgDn Home End scroll the body, Esc closes, Ctrl-C and
    /// Ctrl-D go on; anything else is taken and does nothing.
    pub fn key(&mut self, k: KeyEvent) -> Key {
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'd')) {
            return Key::Pass;
        }
        if k.code == KeyCode::Esc {
            return Key::Close;
        }
        if let Some(body) = self.app.kept() {
            match k.code {
                KeyCode::Up => body.up(),
                KeyCode::Down => body.down(),
                KeyCode::PageUp => body.page_up(),
                KeyCode::PageDown => body.page_down(),
                KeyCode::Home => body.home(),
                KeyCode::End => body.end(),
                _ => {}
            }
        }
        Key::Taken
    }

    /// The wheel: `by` rows, up when positive.
    pub fn scroll(&mut self, by: isize) {
        if let Some(body) = self.app.kept() {
            body.scroll(by);
        }
    }
}

fn clip(text: &str, width: u16) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut out = String::new();
    let mut col = 0;
    for g in text.graphemes(true) {
        col += g.width();
        if col > usize::from(width) {
            break;
        }
        out.push_str(g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn r_sub_11_the_view_is_its_header_a_window_of_its_body_and_its_footer() {
        let lines = (0..30).map(|i| Line::from(format!("line {i}")));
        let mut v = ChildView::new("explore the repo", lines, 40);
        let rows = text(&v.rows(40, 10, None));
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0], "explore the repo");
        assert_eq!(rows[1..9], (22..30).map(|i| format!("line {i}")).collect::<Vec<_>>()[..], "the end, followed: {rows:?}");
        assert!(rows[9].contains("esc") && rows[9].contains("back"), "{rows:?}");
        assert_eq!(v.key(KeyEvent::from(KeyCode::PageUp)), Key::Taken);
        let rows = text(&v.rows(40, 10, None));
        assert_eq!(rows[7], "line 23", "a page up, a line in common: {rows:?}");
        assert!(rows[8].starts_with("↓ 6 more below"), "{rows:?}");
        v.key(KeyEvent::from(KeyCode::End));
        assert_eq!(text(&v.rows(40, 10, None))[8], "line 29");
        // Taller, shorter, narrower: as many rows as asked, always.
        for (w, h) in [(40, 20), (12, 4), (5, 2), (40, 1)] {
            assert_eq!(v.rows(w, h, None).len(), usize::from(h), "{w}x{h}");
        }
        let rows = text(&v.rows(40, 10, Some("⚠ approval waiting · `esc` to answer it")));
        assert_eq!(rows[9], "⚠ approval waiting · esc to answer it", "{rows:?}");
        assert_eq!(v.key(KeyEvent::from(KeyCode::Esc)), Key::Close);
        assert_eq!(v.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Key::Pass);
        assert_eq!(v.key(KeyEvent::from(KeyCode::Char('q'))), Key::Taken, "nothing reaches the prompt");
    }
}
