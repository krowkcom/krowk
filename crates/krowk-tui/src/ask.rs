//! The agent's questions, as they are answered over the prompt: an
//! approval request that carries `questions` (Claude Code's
//! `AskUserQuestion`, Codex's `requestUserInput`, the native `ask_user`).
//! Each question's options are rows to pick from, and the last row is the
//! person's own answer, typed in place. Several questions are answered one
//! after the other, and sent together once each has an answer.

use crate::app::{clip_spans, shown, wrap};
use crate::look::{self, bold, dim, warning as yellow};
use crossterm::event::{KeyCode, KeyEvent};
use krowk_harness::protocol::{ApprovalRequest, Question, QuestionAnswer};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::collections::BTreeSet;

/// How much of a question, or of an option, is shown.
const MAX_QUESTION: usize = 400;
const MAX_OPTION: usize = 200;
/// The most a person's own answer holds.
const MAX_TEXT: usize = 4000;

/// The questions of the request shown now, and the person's answers so far.
#[derive(Debug, Clone)]
pub struct Asking {
    pub request_id: String,
    questions: Vec<Question>,
    /// The question shown.
    at: usize,
    /// Per question: the row the cursor is on — an option, or the last,
    /// `options.len()`, the person's own answer.
    cursor: Vec<usize>,
    picked: Vec<BTreeSet<usize>>,
    text: Vec<String>,
    /// Per question: its answer as Enter last took it — kept as it was
    /// when the person goes back to look.
    saved: Vec<Option<QuestionAnswer>>,
}

/// How the person finished with the questions.
#[derive(Debug, Clone, PartialEq)]
pub enum Done {
    Answered(Vec<QuestionAnswer>),
    Declined,
}

impl Asking {
    /// The questions `req` asks, if it asks any.
    pub fn new(req: &ApprovalRequest) -> Option<Asking> {
        let n = req.questions.len();
        (n > 0).then(|| Asking {
            request_id: req.request_id.clone(),
            questions: req.questions.clone(),
            at: 0,
            cursor: vec![0; n],
            picked: vec![BTreeSet::new(); n],
            text: vec![String::new(); n],
            saved: vec![None; n],
        })
    }

    fn q(&self) -> &Question {
        &self.questions[self.at]
    }

    /// Whether the cursor is on the person's own answer, where keys type.
    pub fn typing(&self) -> bool {
        self.cursor[self.at] == self.q().options.len()
    }

    /// A key, while the questions are shown. Ctrl's keys are not for it.
    pub fn key(&mut self, k: KeyEvent) -> Option<Done> {
        let own = self.q().options.len();
        let multi = self.q().multi_select;
        let typing = self.typing();
        let cur = self.cursor[self.at];
        match k.code {
            KeyCode::Esc => return Some(Done::Declined),
            KeyCode::Up => self.cursor[self.at] = cur.saturating_sub(1),
            KeyCode::Down => self.cursor[self.at] = (cur + 1).min(own),
            KeyCode::Tab => self.turn(1),
            KeyCode::BackTab => self.turn(-1),
            KeyCode::Right if !typing || self.text[self.at].is_empty() => self.turn(1),
            KeyCode::Left if !typing || self.text[self.at].is_empty() => self.turn(-1),
            KeyCode::Backspace if typing => {
                self.text[self.at].pop();
            }
            KeyCode::Enter => return self.enter(),
            KeyCode::Char(' ') if multi && !typing => self.toggle(cur),
            KeyCode::Char(' ') if !typing => return self.enter(),
            // A number picks — until the person has started their own
            // answer, which it is then part of.
            KeyCode::Char(c) if !typing && self.text[self.at].is_empty() && c.is_ascii_digit() && (1..=own).contains(&(c as usize - '0' as usize)) => {
                let i = c as usize - '1' as usize;
                self.cursor[self.at] = i;
                if multi {
                    self.toggle(i);
                } else {
                    return self.enter();
                }
            }
            // Anything else typed is the person's own answer.
            KeyCode::Char(c) => {
                self.cursor[self.at] = own;
                self.type_str(&c.to_string());
            }
            _ => {}
        }
        None
    }

    /// Text pasted: the person's own answer, wherever the cursor was.
    pub fn paste(&mut self, s: &str) {
        self.cursor[self.at] = self.q().options.len();
        self.type_str(s);
    }

    fn type_str(&mut self, s: &str) {
        let t = &mut self.text[self.at];
        for c in s.chars().map(|c| if c.is_control() { ' ' } else { c }) {
            if t.chars().count() >= MAX_TEXT {
                break;
            }
            t.push(c);
        }
    }

    fn toggle(&mut self, i: usize) {
        let p = &mut self.picked[self.at];
        if !p.remove(&i) {
            p.insert(i);
        }
    }

    /// The question `by` away, round the ends.
    fn turn(&mut self, by: isize) {
        let n = self.questions.len() as isize;
        self.at = (self.at as isize + by).rem_euclid(n) as usize;
    }

    /// Enter: the row under the cursor is the answer — for several picks,
    /// those picked (the row under the cursor, when none is), with what
    /// the person wrote — and the next question unanswered comes up, or,
    /// with none left, they are sent.
    fn enter(&mut self) -> Option<Done> {
        let q = &self.questions[self.at];
        let own = q.options.len();
        let cur = self.cursor[self.at];
        let text = self.text[self.at].trim().to_string();
        let label = |i: &usize| q.options[*i].label.clone();
        let answer = if q.multi_select {
            if self.picked[self.at].is_empty() && cur < own {
                self.picked[self.at].insert(cur);
            }
            if self.picked[self.at].is_empty() && text.is_empty() {
                return None;
            }
            QuestionAnswer { id: q.id.clone(), picked: self.picked[self.at].iter().map(label).collect(), text: (!text.is_empty()).then_some(text) }
        } else if cur == own {
            if text.is_empty() {
                return None;
            }
            QuestionAnswer { id: q.id.clone(), picked: Vec::new(), text: Some(text) }
        } else {
            self.picked[self.at] = BTreeSet::from([cur]);
            QuestionAnswer { id: q.id.clone(), picked: vec![label(&cur)], text: None }
        };
        self.saved[self.at] = Some(answer);
        let n = self.questions.len();
        match (1..n).map(|d| (self.at + d) % n).find(|&i| self.saved[i].is_none()) {
            Some(next) => {
                self.at = next;
                None
            }
            None => Some(Done::Answered(self.answers())),
        }
    }

    /// The answers, a question each, in the order asked.
    pub fn answers(&self) -> Vec<QuestionAnswer> {
        self.questions.iter().zip(&self.saved).map(|(q, a)| a.clone().unwrap_or_else(|| QuestionAnswer { id: q.id.clone(), ..QuestionAnswer::default() })).collect()
    }

    /// The questions as they are shown over the prompt: the one shown, its
    /// options and the person's own answer, and the keys. `from` says whose
    /// they are when a subagent asks; `waiting` counts the requests in line.
    pub fn rows(&self, width: usize, from: Option<&str>, waiting: usize) -> Vec<Line<'static>> {
        let q = self.q();
        let n = self.questions.len();
        let more = if waiting > 1 { format!(" (1 of {waiting})") } else { String::new() };
        let who = from.map(|f| format!("{}: ", shown(f, 80))).unwrap_or_default();
        let header = if q.header.is_empty() || n > 1 { String::new() } else { format!("{} — ", shown(&q.header, 40)) };
        let title = format!("{}{who}{header}{}{more}", look::TOOL, shown(&q.question, MAX_QUESTION));
        let mut rows: Vec<Line<'static>> = wrap(&title, width).into_iter().map(|l| Line::from(Span::styled(l, yellow().add_modifier(Modifier::BOLD)))).collect();
        if n > 1 {
            // The questions by their headers: the one shown in bold, those
            // answered ticked.
            let mut tabs = vec![Span::raw("  ")];
            for (i, q) in self.questions.iter().enumerate() {
                if i > 0 {
                    tabs.push(Span::styled(" · ", dim()));
                }
                let name = if q.header.is_empty() { format!("Question {}", i + 1) } else { shown(&q.header, 40) };
                let tick = if self.saved[i].is_some() { "✓ " } else { "" };
                tabs.push(Span::styled(format!("{tick}{name}"), if i == self.at { bold() } else { dim() }));
            }
            rows.push(Line::from(clip_spans(tabs, width)));
        }
        let cur = self.cursor[self.at];
        for (i, o) in q.options.iter().enumerate() {
            let check = match (q.multi_select, self.picked[self.at].contains(&i)) {
                (false, _) => "",
                (true, true) => "[x] ",
                (true, false) => "[ ] ",
            };
            let lead = format!("{}{check}{}. ", if i == cur { "❯ " } else { "  " }, i + 1);
            let label = shown(&o.label, MAX_OPTION);
            let says = if o.description.is_empty() { String::new() } else { format!("  {}", shown(&o.description, MAX_OPTION)) };
            rows.extend(hung(&lead, &label, &says, i == cur, width));
        }
        let own = q.options.len();
        let lead = format!("{}{}", if cur == own { "❯ " } else { "  " }, if q.options.is_empty() { String::new() } else { format!("{}. ", own + 1) });
        let typed = &self.text[self.at];
        let line = if typed.is_empty() {
            let hint = if q.options.is_empty() { "type your answer" } else { "something else — type it" };
            vec![Span::styled(lead, bold()), Span::styled(hint, dim())]
        } else {
            let shown_text = if q.secret { "•".repeat(typed.chars().count()) } else { shown(typed, MAX_TEXT) };
            vec![Span::styled(lead, bold()), Span::styled(shown_text, if cur == own { bold() } else { Default::default() })]
        };
        rows.push(Line::from(clip_spans(line, width)));
        let pick = if q.multi_select { "`space` pick · `enter` done" } else { "`enter` pick" };
        let turn = if n > 1 { " · `tab` next question" } else { "" };
        rows.push(Line::from(clip_spans(look::keys(&format!("  `↑↓` choose · {pick}{turn} · `esc` decline"), look::accent()), width)));
        rows
    }
}

/// An option's row: `lead` and its label, bold when chosen, what it means
/// dimmed after it, wrapped under the label.
fn hung(lead: &str, label: &str, says: &str, chosen: bool, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let hang = " ".repeat(lead.width());
    let text = format!("{label}{says}");
    let mut out = Vec::new();
    let mut left = label.chars().count();
    for (i, row) in wrap(&text, width.saturating_sub(lead.width()).max(1)).into_iter().enumerate() {
        let mut spans = vec![Span::styled(if i == 0 { lead.to_string() } else { hang.clone() }, bold())];
        // The label's part of the row, then the description's.
        let n = row.chars().count().min(left);
        let (l, d): (String, String) = (row.chars().take(n).collect(), row.chars().skip(n).collect());
        left = left.saturating_sub(n);
        spans.push(Span::styled(l, if chosen { bold() } else { Default::default() }));
        spans.push(Span::styled(d, dim()));
        out.push(Line::from(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use krowk_harness::protocol::QuestionOption;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn question(id: &str, multi: bool, options: &[&str]) -> Question {
        Question {
            id: id.into(),
            header: id.to_uppercase(),
            question: format!("Which {id}?"),
            options: options.iter().map(|l| QuestionOption { label: l.to_string(), description: format!("about {l}") }).collect(),
            multi_select: multi,
            secret: false,
        }
    }

    fn asking(questions: Vec<Question>) -> Asking {
        let req = ApprovalRequest {
            session_id: "s".into(),
            turn_id: "t".into(),
            request_id: "r".into(),
            tool: "ask_user".into(),
            input: serde_json::json!({}),
            summary: String::new(),
            reason: String::new(),
            remember: vec![],
            questions,
        };
        Asking::new(&req).unwrap()
    }

    fn text(rows: &[Line]) -> String {
        rows.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn one_question_is_answered_by_its_option_number_or_the_arrows_and_enter() {
        let mut a = asking(vec![question("db", false, &["Postgres", "SQLite"])]);
        let shown = text(&a.rows(80, None, 1));
        assert!(shown.contains("DB — Which db?") && shown.contains("❯ 1. Postgres  about Postgres") && shown.contains("  2. SQLite") && shown.contains("3. something else"), "{shown}");
        assert_eq!(a.key(key(KeyCode::Char('2'))), Some(Done::Answered(vec![QuestionAnswer { id: "db".into(), picked: vec!["SQLite".into()], text: None }])));

        let mut a = asking(vec![question("db", false, &["Postgres", "SQLite"])]);
        a.key(key(KeyCode::Down));
        assert!(text(&a.rows(80, None, 1)).contains("❯ 2. SQLite"));
        assert!(matches!(a.key(key(KeyCode::Enter)), Some(Done::Answered(ans)) if ans[0].picked == ["SQLite"]));
        assert_eq!(asking(vec![question("db", false, &["a", "b"])]).key(key(KeyCode::Esc)), Some(Done::Declined));
    }

    #[test]
    fn typing_is_the_persons_own_answer_and_replaces_a_pick() {
        let mut a = asking(vec![question("db", false, &["Postgres", "SQLite"])]);
        for c in "duckdb 9".chars() {
            assert_eq!(a.key(key(KeyCode::Char(c))), None, "typed, not picked: {c}");
        }
        a.key(key(KeyCode::Backspace));
        assert!(a.typing() && text(&a.rows(80, None, 1)).contains("❯ 3. duckdb "));
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Done::Answered(vec![QuestionAnswer { id: "db".into(), picked: vec![], text: Some("duckdb".into()) }])));
        // Once the person writes, a number is part of what they write.
        let mut a = asking(vec![question("n", false, &["one", "two"])]);
        a.key(key(KeyCode::Char('x')));
        a.key(key(KeyCode::Up));
        assert_eq!(a.key(key(KeyCode::Char('2'))), None, "typed, though the cursor was on an option");
        assert!(matches!(a.key(key(KeyCode::Enter)), Some(Done::Answered(ans)) if ans[0].text.as_deref() == Some("x2")));
        // Space picks where one may be picked.
        assert!(matches!(asking(vec![question("db", false, &["a", "b"])]).key(key(KeyCode::Char(' '))), Some(Done::Answered(ans)) if ans[0].picked == ["a"]));
        // An empty own answer is no answer.
        let mut a = asking(vec![question("db", false, &["Postgres"])]);
        a.key(key(KeyCode::Down));
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        // A question with no options is only typed; a secret is not shown.
        let mut a = asking(vec![Question { secret: true, ..question("key", false, &[]) }]);
        a.paste("hunter2\n");
        let shown = text(&a.rows(80, None, 1));
        assert!(shown.contains("❯ ••••••••") && !shown.contains("hunter2"), "{shown}");
        assert!(matches!(a.key(key(KeyCode::Enter)), Some(Done::Answered(ans)) if ans[0].text.as_deref() == Some("hunter2")));
    }

    #[test]
    fn several_picks_toggle_and_several_questions_are_sent_once_each_is_answered() {
        let mut a = asking(vec![question("db", false, &["Postgres", "SQLite"]), question("tests", true, &["unit", "e2e", "bench"])]);
        let shown = text(&a.rows(80, None, 1));
        assert!(shown.contains("Which db?") && shown.contains("  DB · TESTS") && shown.contains("tab next question"), "{shown}");
        assert_eq!(a.key(key(KeyCode::Char('1'))), None, "on to the next question");
        let shown = text(&a.rows(80, None, 1));
        assert!(shown.contains("✓ DB · TESTS") && shown.contains("❯ [ ] 1. unit"), "{shown}");
        a.key(key(KeyCode::Char('1')));
        a.key(key(KeyCode::Char('3')));
        a.key(key(KeyCode::Char('3')));
        a.key(key(KeyCode::Char('2')));
        a.key(key(KeyCode::Down));
        for c in "fuzz".chars() {
            a.key(key(KeyCode::Char(c)));
        }
        assert!(text(&a.rows(80, None, 1)).contains("[x] 1. unit"));
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Done::Answered(vec![
                QuestionAnswer { id: "db".into(), picked: vec!["Postgres".into()], text: None },
                QuestionAnswer { id: "tests".into(), picked: vec!["unit".into(), "e2e".into()], text: Some("fuzz".into()) },
            ]))
        );
        // An answer stays as Enter took it while the person looks back.
        let mut a = asking(vec![question("a", false, &["x", "y"]), question("b", false, &["x", "y"])]);
        a.key(key(KeyCode::Char('1')));
        a.key(key(KeyCode::Tab));
        a.key(key(KeyCode::Down));
        a.key(key(KeyCode::Down));
        a.key(key(KeyCode::Tab));
        assert!(matches!(a.key(key(KeyCode::Char('2'))), Some(Done::Answered(ans)) if ans[0].picked == ["x"] && ans[1].picked == ["y"]));
        // Enter on the last goes back to one left unanswered.
        let mut a = asking(vec![question("a", false, &["x", "y"]), question("b", false, &["x", "y"])]);
        a.key(key(KeyCode::Tab));
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert!(matches!(a.key(key(KeyCode::Enter)), Some(Done::Answered(ans)) if ans[0].picked == ["x"] && ans[1].picked == ["x"]));
    }

    #[test]
    fn what_the_model_wrote_is_shown_flat_and_cut() {
        let mut q = question("db", false, &["a\x1b[2Jb"]);
        q.question = format!("line one\nline two {}", "x".repeat(1000));
        let shown = text(&asking(vec![q]).rows(60, Some("subagent “s”"), 2));
        assert!(!shown.contains('\x1b') && shown.contains("⏎") && shown.contains(" … ") && shown.contains("(1 of 2)") && shown.contains("subagent “s”: "), "{shown}");
    }
}
