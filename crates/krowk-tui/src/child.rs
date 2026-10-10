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
//! the call out. Its header is the main App's to say (`App::child_header`):
//! the child's row, which its frames keep current.

use crate::app::App;
use crate::look;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use krowk_harness::protocol::{LiveEvent, LogBody, LogEvent, StreamLine};
use ratatui::text::{Line, Span};
use std::collections::HashSet;

pub struct ChildView {
    /// The child's session.
    session_id: String,
    /// The child's App: its transcript, kept.
    app: Box<App>,
    /// The ids of the events its log gave: the same events arriving live
    /// (read and sent while the view opened) are not drawn twice.
    read: HashSet<String>,
    /// The items its log has whole: their live frames, still on their way
    /// when it was read, would draw them a second time.
    done: HashSet<String>,
    /// The columns the body is laid out in.
    width: u16,
    /// What the footer says in place of its keys until the next key: why
    /// `x` stopped nothing.
    note: Option<String>,
}

/// What a key did to the view.
#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    /// Taken: scrolled, or nothing to do.
    Taken,
    /// Esc: the view closes.
    Close,
    /// ← or →: the child before or after this one in the Agents overlay.
    Sibling(isize),
    /// `x`: stop this child alone.
    Stop,
    /// Not the view's: Ctrl-C and Ctrl-D go on as anywhere.
    Pass,
}

impl ChildView {
    /// A view on child `session_id`, drawn by `app`: its
    /// history first, then the item it is in the middle of (`under_way`:
    /// its id, its start and what it has streamed so far), unless its log
    /// already has it whole, then live. A log that could not be read is
    /// said, and what arrives from now on is still shown.
    pub fn open(session_id: &str, mut app: App, history: Result<Vec<LogEvent>, String>, under_way: Option<(String, [StreamLine; 2])>) -> ChildView {
        app.keep();
        let (mut read, mut done) = (HashSet::new(), HashSet::new());
        match history {
            Ok(events) => {
                let head = events.last().map(|e| e.id.clone()).unwrap_or_default();
                app.replay(&krowk_harness::log::branch(&events, &head));
                for e in events {
                    if let LogBody::ItemCompleted { item_id, .. } = e.body {
                        done.insert(item_id);
                    }
                    read.insert(e.id);
                }
            }
            Err(e) => app.say(&format!("its history could not be read here ({e}) — what it does from now on is shown"), look::dim()),
        }
        app.session_id = Some(session_id.to_string());
        for line in under_way.filter(|(id, _)| !done.contains(id)).into_iter().flat_map(|(_, lines)| lines) {
            app.on_line(&line);
        }
        // None yet: the first frame lays it out at the width it is drawn at.
        ChildView { session_id: session_id.to_string(), app: Box::new(app), read, done, width: 0, note: None }
    }

    /// A view on `lines` alone, at `width` columns.
    #[cfg(test)]
    pub fn new(lines: impl IntoIterator<Item = Line<'static>>, width: u16) -> ChildView {
        let mut app = App::new(crate::editor::Editor::new(None), width, crate::settings::Settings::default(), None, None);
        for line in lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            app.say(&text, ratatui::style::Style::new());
        }
        ChildView::open("child", app, Ok(Vec::new()), None)
    }

    /// The child's session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Says `note` in the footer until the next key.
    pub fn say(&mut self, note: &str) {
        self.note = Some(note.to_string());
    }

    /// A line of the stream: the child's own are its App's, once. Whether
    /// it was: the view is drawn again for it.
    pub fn on_line(&mut self, line: &StreamLine) -> bool {
        let ours = match line {
            StreamLine::Log(ev) => ev.session_id == self.session_id && !self.read.remove(&ev.id),
            StreamLine::Live(LiveEvent::ItemStarted { item_id, .. } | LiveEvent::ItemDelta { item_id, .. }) if self.done.contains(item_id) => false,
            StreamLine::Live(_) => crate::app::line_session(line) == Some(self.session_id.as_str()),
        };
        if ours {
            self.app.on_line(line);
        }
        ours
    }

    /// The view's `height` rows at `width` columns: `header`, the body's
    /// window, its footer. The body is wrapped again when the width changed.
    /// A note (`say`) takes the footer's place, or else `waiting`, when an
    /// approval waits on the person: it says so and how to get to it.
    pub fn rows(&mut self, width: u16, height: u16, header: &str, waiting: Option<&str>) -> Vec<Line<'static>> {
        let width = width.max(1);
        if width != self.width {
            self.width = width;
            self.app.set_width(width);
        }
        self.app.take_pending();
        let h = usize::from(height);
        let mut rows = vec![Line::from(Span::styled(clip(&crate::card::clean(header), width), look::bold()))];
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
        rows.push(Line::from(match (&self.note, waiting) {
            (Some(note), _) => vec![Span::styled(clip(&format!("{}{note}", look::WARN), width), look::warning())],
            (None, Some(note)) => look::keys(note, look::warning()),
            (None, None) => look::keys("`esc` back · `←` `→` siblings · `x` stop · `↑` `↓` `PgUp` `PgDn` scroll", look::dim()),
        }));
        rows
    }

    /// ↑ ↓ PgUp PgDn Home End scroll the body, Esc closes, ← → go to a
    /// sibling, `x` stops the child, Ctrl-C and Ctrl-D go on; anything else
    /// is taken and does nothing. Any key takes the footer's note away.
    pub fn key(&mut self, k: KeyEvent) -> Key {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char('c' | 'd')) {
            return Key::Pass;
        }
        self.note = None;
        match k.code {
            KeyCode::Esc => return Key::Close,
            KeyCode::Left => return Key::Sibling(-1),
            KeyCode::Right => return Key::Sibling(1),
            KeyCode::Char('x') if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => return Key::Stop,
            _ => {}
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

/// A child's log, as far as it is written: its events, oldest first. The
/// child may be appending as it is read, so a last line not ended yet is
/// left out — it arrives live — where anything else unreadable is an
/// error, as `log::read_events` has it.
pub fn read_log(path: &std::path::Path) -> Result<Vec<LogEvent>, String> {
    // Bytes, not text: a line cut mid-append can end inside a character.
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let whole = bytes.iter().rposition(|&b| b == b'\n').map_or(&[][..], |end| &bytes[..end]);
    let mut out = Vec::new();
    for (n, line) in whole.split(|&b| b == b'\n').enumerate() {
        if line.trim_ascii().is_empty() {
            continue;
        }
        out.push(serde_json::from_slice(line).map_err(|e| format!("{} line {}: {e}", path.display(), n + 1))?);
    }
    Ok(out)
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

    fn said(id: &str, item_id: &str, item: krowk_harness::protocol::Item) -> LogEvent {
        LogEvent { id: id.into(), parent_id: None, session_id: "k1".into(), time_ms: 0, body: LogBody::ItemCompleted { turn_id: "t".into(), item_id: item_id.into(), item } }
    }

    /// R-SUB-11: an item the log has whole, its live frames still queued
    /// when it was read, is drawn once, and nothing of it is left streaming.
    #[test]
    fn r_sub_11_an_item_read_whole_is_not_drawn_again_from_its_queued_frames() {
        use krowk_harness::protocol::{Delta, Item, ItemKind};
        let app = App::new(crate::editor::Editor::new(None), 40, crate::settings::Settings::default(), None, None);
        let done = said("e1", "m1", Item::AssistantText { text: "QUEUED line".into() });
        let mut v = ChildView::open("k1", app, Ok(vec![done.clone()]), None);
        assert!(!v.on_line(&StreamLine::Live(LiveEvent::ItemStarted { session_id: "k1".into(), turn_id: "t".into(), item_id: "m1".into(), item: ItemKind::AssistantText })));
        assert!(!v.on_line(&StreamLine::Live(LiveEvent::ItemDelta { session_id: "k1".into(), turn_id: "t".into(), item_id: "m1".into(), delta: Delta::Text { text: "QUEUED line\nta".into() } })));
        assert!(!v.on_line(&StreamLine::Log(done)));
        let rows = text(&v.rows(40, 10, "k1", None));
        assert_eq!(rows.iter().filter(|r| r.contains("QUEUED line")).count(), 1, "{rows:?}");
        assert!(!rows.iter().any(|r| r.trim() == "ta"), "{rows:?}");
        // The next item streams as ever.
        assert!(v.on_line(&StreamLine::Live(LiveEvent::ItemStarted { session_id: "k1".into(), turn_id: "t".into(), item_id: "m2".into(), item: ItemKind::AssistantText })));
        v.on_line(&StreamLine::Live(LiveEvent::ItemDelta { session_id: "k1".into(), turn_id: "t".into(), item_id: "m2".into(), delta: Delta::Text { text: "next".into() } }));
        assert!(text(&v.rows(40, 10, "k1", None)).iter().any(|r| r == "next"));
    }

    /// R-SUB-11: a running child's log is read as far as it is written: a
    /// last line still being appended is left for the stream to bring.
    #[test]
    fn r_sub_11_a_childs_log_is_read_without_its_half_written_last_line() {
        use krowk_harness::protocol::Item;
        let dir = std::env::temp_dir().join(format!("krowk-sv7-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let line = |e: &LogEvent| serde_json::to_string(e).unwrap();
        let (a, b) = (said("e1", "i1", Item::user("TASK")), said("e2", "i2", Item::AssistantText { text: "x".repeat(5000) }));
        let whole = line(&b);
        std::fs::write(&path, format!("{}\n{}", line(&a), &whole[..whole.len() / 2])).unwrap();
        let read = read_log(&path).unwrap();
        assert_eq!(read.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e1"]);
        // Cut inside a character: the whole lines before it still read.
        let wide = line(&said("e3", "i3", Item::AssistantText { text: "é".repeat(100) }));
        let mid = wide.find('é').unwrap() + 1;
        let mut bytes = format!("{}\n", line(&a)).into_bytes();
        bytes.extend_from_slice(&wide.as_bytes()[..mid]);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_log(&path).unwrap().len(), 1, "cut mid-character");
        std::fs::write(&path, format!("{}\n{whole}\n", line(&a))).unwrap();
        assert_eq!(read_log(&path).unwrap().len(), 2, "whole, every line");
        std::fs::write(&path, format!("not json\n{}\n", line(&a))).unwrap();
        assert!(read_log(&path).unwrap_err().contains("line 1"), "a broken line before the end is an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r_sub_11_the_view_is_its_header_a_window_of_its_body_and_its_footer() {
        let lines = (0..30).map(|i| Line::from(format!("line {i}")));
        let mut v = ChildView::new(lines, 40);
        let rows = text(&v.rows(40, 10, "explore the repo", None));
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0], "explore the repo");
        assert_eq!(rows[1..9], (22..30).map(|i| format!("line {i}")).collect::<Vec<_>>()[..], "the end, followed: {rows:?}");
        assert!(rows[9].contains("esc") && rows[9].contains("back"), "{rows:?}");
        assert_eq!(v.key(KeyEvent::from(KeyCode::PageUp)), Key::Taken);
        let rows = text(&v.rows(40, 10, "explore the repo", None));
        assert_eq!(rows[7], "line 23", "a page up, a line in common: {rows:?}");
        assert!(rows[8].starts_with("↓ 6 more below"), "{rows:?}");
        v.key(KeyEvent::from(KeyCode::End));
        assert_eq!(text(&v.rows(40, 10, "explore the repo", None))[8], "line 29");
        // Taller, shorter, narrower: as many rows as asked, always.
        for (w, h) in [(40, 20), (12, 4), (5, 2), (40, 1)] {
            assert_eq!(v.rows(w, h, "explore the repo", None).len(), usize::from(h), "{w}x{h}");
        }
        let rows = text(&v.rows(40, 10, "explore the repo", Some("⚠ approval waiting · `esc` to answer it")));
        assert_eq!(rows[9], "⚠ approval waiting · esc to answer it", "{rows:?}");
        assert_eq!(v.key(KeyEvent::from(KeyCode::Esc)), Key::Close);
        assert_eq!(v.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Key::Pass);
        assert_eq!(v.key(KeyEvent::from(KeyCode::Char('q'))), Key::Taken, "nothing reaches the prompt");
    }

    /// R-SUB-11: the footer names the view's keys — ← → for the siblings,
    /// `x` to stop — and a note in their place, why `x` stopped nothing,
    /// stays until the next key.
    #[test]
    fn r_sub_11_the_footer_names_siblings_and_stop_and_says_why_nothing_stopped() {
        let mut v = ChildView::new((0..3).map(|i| Line::from(format!("line {i}"))), 60);
        let footer = |v: &mut ChildView| text(&v.rows(80, 8, "h", None))[7].clone();
        assert_eq!(footer(&mut v), "esc back · ← → siblings · x stop · ↑ ↓ PgUp PgDn scroll");
        assert_eq!(v.key(KeyEvent::from(KeyCode::Left)), Key::Sibling(-1));
        assert_eq!(v.key(KeyEvent::from(KeyCode::Right)), Key::Sibling(1));
        assert_eq!(v.key(KeyEvent::from(KeyCode::Char('x'))), Key::Stop);
        assert_eq!(v.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)), Key::Taken);
        v.say("Claude Code cannot stop one agent alone yet");
        assert_eq!(footer(&mut v), "⚠ Claude Code cannot stop one agent alone yet");
        assert_eq!(text(&v.rows(80, 8, "h", Some("⚠ approval waiting")))[7], "⚠ Claude Code cannot stop one agent alone yet", "the answer to the key first");
        v.key(KeyEvent::from(KeyCode::Down));
        assert!(footer(&mut v).starts_with("esc back"), "gone at the next key");
    }
}
