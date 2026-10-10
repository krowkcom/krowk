//! The child view (R-SUB-11): a full-height view of one child's
//! transcript — a header, a window of its lines that scrolls, a footer —
//! drawn where the screen gives it room (`Screen::view_rows`): inline, the
//! whole terminal as the live region; fullscreen, the conversation's rows
//! above the prompt and the status line. Neither switches screens for it.
//!
//! Its body is a `Kept`, the lines and the window fullscreen's conversation
//! scrolls with. Until a child's transcript fills it, a fixed list of lines
//! does (`placeholder`), so the view can be opened and looked at alone.

use crate::full::Kept;
use crate::look;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};

pub struct ChildView {
    title: String,
    body: Kept,
    /// The columns `body` is wrapped to.
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
    /// A view titled `title` on `lines`, at `width` columns.
    pub fn new(title: &str, lines: impl IntoIterator<Item = Line<'static>>, width: u16) -> ChildView {
        let mut body = Kept::new(None, 1);
        for line in lines {
            body.push(&line, width);
        }
        ChildView { title: crate::card::clean(title), body, width }
    }

    /// A fixed transcript to look at the view with: the developer's entry
    /// (F12 in a debug build) until a child's lines fill it.
    pub fn placeholder(width: u16) -> ChildView {
        let lines = (1..=120).map(|i| match i % 10 {
            1 => Line::from(Span::styled(format!("◆ step {i}: a tool call the child made"), look::accent())),
            5 => Line::from(format!("{i:>3}  a longer line of the child's answer, long enough to wrap on a narrow terminal and show that the body wraps to the width it is drawn at")),
            _ => Line::from(format!("{i:>3}  placeholder transcript line")),
        });
        ChildView::new("placeholder child · no transcript yet", lines, width)
    }

    /// The view's `height` rows at `width` columns: its header, the body's
    /// window, its footer. The body is wrapped again when the width changed.
    /// `waiting`, when an approval waits on the person, takes the footer's
    /// place: it says so and how to get to it.
    pub fn rows(&mut self, width: u16, height: u16, waiting: Option<&str>) -> Vec<Line<'static>> {
        let width = width.max(1);
        if width != self.width {
            self.width = width;
            self.body.rewrap(width);
        }
        let h = usize::from(height);
        let mut rows = vec![Line::from(Span::styled(clip(&self.title, width), look::bold()))];
        if h < 3 {
            rows.resize(h, Line::default());
            return rows;
        }
        let body = height - 2;
        self.body.set_height(body);
        rows.extend(self.body.window().cloned());
        let below = self.body.below();
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
        match k.code {
            KeyCode::Esc => return Key::Close,
            KeyCode::Up => self.body.up(),
            KeyCode::Down => self.body.down(),
            KeyCode::PageUp => self.body.page_up(),
            KeyCode::PageDown => self.body.page_down(),
            KeyCode::Home => self.body.home(),
            KeyCode::End => self.body.end(),
            _ => {}
        }
        Key::Taken
    }

    /// The wheel: `by` rows, up when positive.
    pub fn scroll(&mut self, by: isize) {
        self.body.scroll(by);
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
