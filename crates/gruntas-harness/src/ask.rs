//! The agent asks the person which way to go — a few questions, each with
//! options to pick from, or the person's own answer: what Claude Code's
//! `AskUserQuestion` and Codex's `item/tool/requestUserInput` become, one
//! approval request that carries the questions (`Gate::ask`), answered by
//! whichever client is attached. The input is Claude Code's.

use crate::protocol::{Question, QuestionAnswer, QuestionOption};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// The most questions one call asks, and options one question offers.
pub const MAX_QUESTIONS: usize = 4;
pub const MAX_OPTIONS: usize = 6;

// No doc comments on these: a field's description rides on every call, and
// the names say it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskUserInput {
    pub questions: Vec<AskQuestion>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestion {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    pub options: Vec<AskOption>,
    #[serde(default)]
    pub multi_select: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A call's input as the questions to ask, each named by its text as
/// written — as Claude Code's answers name them — or why it is refused. What else
/// Claude Code's input carries is no matter.
pub fn parse(input: &Value) -> Result<Vec<Question>, String> {
    let questions = input.get("questions").cloned().unwrap_or(Value::Null);
    let i = AskUserInput::deserialize(serde_json::json!({ "questions": questions })).map_err(|e| format!("invalid input for ask_user: {e}"))?;
    if i.questions.is_empty() || i.questions.len() > MAX_QUESTIONS {
        return Err(format!("ask_user takes 1 to {MAX_QUESTIONS} questions; this call has {}", i.questions.len()));
    }
    let mut out: Vec<Question> = Vec::new();
    for q in i.questions {
        let text = q.question.trim().to_string();
        if text.is_empty() {
            return Err("a question has no text".into());
        }
        if out.iter().any(|o| o.question == text) {
            return Err(format!("the question {text:?} is asked twice"));
        }
        if q.options.len() > MAX_OPTIONS {
            return Err(format!("the question {text:?} has {} options; offer at most {MAX_OPTIONS}", q.options.len()));
        }
        let options: Vec<QuestionOption> = q.options.into_iter().map(|o| QuestionOption { label: o.label.trim().to_string(), description: o.description.unwrap_or_default().trim().to_string() }).collect();
        if options.iter().any(|o| o.label.is_empty()) {
            return Err(format!("an option of {text:?} has no label"));
        }
        // Named by the text as the model wrote it, untrimmed: Claude Code
        // matches its answers to that exact string.
        out.push(Question { id: q.question.clone(), header: q.header.unwrap_or_default().trim().to_string(), question: text, options, multi_select: q.multi_select.unwrap_or(false), secret: false });
    }
    Ok(out)
}

/// An answer in a line: the options picked and what the person wrote,
/// as Claude Code joins several picks. `None` when left unanswered.
pub fn said(a: &QuestionAnswer) -> Option<String> {
    let parts: Vec<&str> = a.picked.iter().map(String::as_str).chain(a.text.as_deref()).filter(|s| !s.trim().is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_codes_input_is_the_questions_and_its_answers_come_back_in_its_words() {
        // As Claude Code writes it, `metadata` and `preview` and all.
        let input = json!({"questions": [
            {"question": "Which database?", "header": "DB", "multiSelect": false, "options": [{"label": "Postgres (Recommended)", "description": "what prod runs", "preview": "x"}, {"label": "SQLite"}]},
            {"question": "Which tests?", "options": [{"label": "unit"}, {"label": "e2e"}], "multiSelect": true},
        ], "metadata": {"source": "x"}});
        let qs = parse(&input).unwrap();
        assert_eq!((qs[0].id.as_str(), qs[0].header.as_str(), qs[0].options[0].description.as_str(), qs[1].multi_select), ("Which database?", "DB", "what prod runs", true));
        let padded = parse(&json!({"questions": [{"question": " Which? \n", "options": []}]})).unwrap();
        assert_eq!((padded[0].id.as_str(), padded[0].question.as_str()), (" Which? \n", "Which?"), "named as written, shown trimmed");
        let answers = [
            QuestionAnswer { id: "Which database?".into(), picked: vec![], text: Some("DuckDB".into()) },
            QuestionAnswer { id: "Which tests?".into(), picked: vec!["unit".into(), "e2e".into()], text: Some("fuzz".into()) },
        ];
        assert_eq!((said(&answers[0]).as_deref(), said(&answers[1]).as_deref()), (Some("DuckDB"), Some("unit, e2e, fuzz")));
        assert_eq!(said(&QuestionAnswer { id: "Which database?".into(), ..Default::default() }), None, "none answered");
    }

    #[test]
    fn a_call_that_asks_nothing_asks_twice_or_offers_too_much_is_refused() {
        assert!(parse(&json!({"questions": []})).unwrap_err().contains("1 to 4"));
        assert!(parse(&json!({})).unwrap_err().contains("invalid input"));
        let q = |t: &str| json!({"question": t, "options": [{"label": "a"}, {"label": "b"}]});
        assert!(parse(&json!({"questions": [q("a?"), q("b?"), q("c?"), q("d?"), q("e?")]})).unwrap_err().contains("1 to 4"));
        assert!(parse(&json!({"questions": [q("a?"), q(" a? ")]})).unwrap_err().contains("asked twice"));
        assert!(parse(&json!({"questions": [{"question": "x?", "options": [{"label": " "}]}]})).unwrap_err().contains("no label"));
        let many: Vec<Value> = (0..7).map(|i| json!({"label": i.to_string()})).collect();
        assert!(parse(&json!({"questions": [{"question": "x?", "options": many}]})).unwrap_err().contains("at most 6"));
    }
}
