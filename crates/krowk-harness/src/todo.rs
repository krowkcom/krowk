//! `todo_write` (R-TODO-1, R-TOOL-1): the model's plan for the task, as one
//! list it replaces whole on every call — never items added or ticked one
//! at a time, so there is one list and no merge to get wrong.
//!
//! The list lives in the session log (R-TODO-2): each write is logged as
//! `todos.updated`, which is what a client shows, and the call itself is in
//! the conversation, which is what the model reads back — so the list
//! survives a resume, a handoff and a model switch with the rest of the
//! session.
//!
//! A list left alone goes stale (R-TODO-3): when items are still open and
//! `STALE_CALLS` model calls have passed without a write, the native loop
//! adds a system reminder before the next call, holding the list, so the
//! model either updates it or carries on knowing it is behind. The reminder
//! is a `userText` item like steering, logged where it was sent, so a
//! replayed conversation is the one the model saw.

use crate::engine::HistoryItem;
use crate::protocol::{Item, Todo, TodoStatus};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub const TODO_WRITE: &str = "todo_write";

/// What the model is told. Short: it rides on every call.
pub const DESCRIPTION: &str = "Replace the whole todo list. For work of three or more steps: keep one item in_progress, and mark each completed as soon as it is done.";

/// Model calls without a write, with items open, before a reminder.
pub const STALE_CALLS: usize = 10;

/// What a reminder starts with: clients show it as krowk's, not the
/// person's words.
pub const REMINDER: &str = "<system-reminder>";

/// Replace the todo list.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoWriteInput {
    /// The whole list, in order.
    pub todos: Vec<Todo>,
}

/// The most items a list takes: a plan, not a backlog.
pub const MAX_ITEMS: usize = 50;
/// Characters of an item kept: a step, not a document. The rest is cut.
pub const MAX_CONTENT: usize = 500;

/// A call's input as the new list, or why it is refused.
pub fn parse(input: &Value) -> Result<Vec<Todo>, String> {
    let i = TodoWriteInput::deserialize(input).map_err(|e| format!("invalid input for todo_write: {e}"))?;
    if i.todos.len() > MAX_ITEMS {
        return Err(format!("todo_write takes at most {MAX_ITEMS} items; this list has {} — keep it to the steps of the task", i.todos.len()));
    }
    if let Some(n) = i.todos.iter().position(|t| t.content.trim().is_empty()) {
        return Err(format!("todo {} has no content", n + 1));
    }
    Ok(i.todos
        .into_iter()
        .map(|t| {
            let content = t.content.trim();
            let content = if content.chars().count() > MAX_CONTENT { content.chars().take(MAX_CONTENT).collect::<String>() + "…" } else { content.to_string() };
            Todo { content, ..t }
        })
        .collect())
}

/// What the model reads back: the counts, so it knows the write landed.
pub fn summary(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "The todo list is empty.".into();
    }
    let n = |s: TodoStatus| todos.iter().filter(|t| t.status == s).count();
    format!("Todos updated: {} in progress, {} pending, {} completed.", n(TodoStatus::InProgress), n(TodoStatus::Pending), n(TodoStatus::Completed))
}

/// The list as the branch last set it: the input of its last `todo_write`
/// call that was not refused.
pub fn current(history: &[HistoryItem]) -> Vec<Todo> {
    let refused: Vec<&str> = history
        .iter()
        .filter_map(|h| match &h.item {
            Item::ToolResult { call_id, is_error: true, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    history
        .iter()
        .rev()
        .find_map(|h| match &h.item {
            Item::ToolCall { call_id, name, input } if name == TODO_WRITE && !refused.contains(&call_id.as_str()) => parse(input).ok(),
            _ => None,
        })
        .unwrap_or_default()
}

/// The reminder to send before the next call, when the list has items
/// open and `STALE_CALLS` model calls have passed since it was last written
/// or last reminded of.
pub fn reminder(history: &[HistoryItem]) -> Option<String> {
    let last = history.iter().rposition(|h| match &h.item {
        Item::ToolCall { name, .. } => name == TODO_WRITE,
        Item::UserText { text, .. } => text.starts_with(REMINDER),
        _ => false,
    })?;
    let mut calls: Vec<usize> = history[last + 1..].iter().filter_map(|h| h.response).collect();
    calls.dedup();
    if calls.len() < STALE_CALLS {
        return None;
    }
    let todos = current(history);
    let open: Vec<&Todo> = todos.iter().filter(|t| t.status != TodoStatus::Completed).collect();
    if open.is_empty() {
        return None;
    }
    let list: String = todos
        .iter()
        .map(|t| {
            let mark = match t.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Completed => "[x]",
            };
            format!("\n{mark} {}", t.content)
        })
        .collect();
    Some(format!(
        "{REMINDER}The todo list has not been updated in {STALE_CALLS} model calls and {} of its items are not completed:{list}\nIf it no longer matches the work, update it with todo_write. Do not mention this reminder.</system-reminder>",
        open.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, input: Value, response: usize) -> HistoryItem {
        HistoryItem { item: Item::ToolCall { call_id: id.into(), name: TODO_WRITE.into(), input }, response: Some(response) }
    }

    fn result(id: &str, is_error: bool) -> HistoryItem {
        HistoryItem { item: Item::ToolResult { call_id: id.into(), output: String::new(), is_error }, response: None }
    }

    #[test]
    fn r_todo_1_every_write_replaces_the_whole_list() {
        let first = json!({"todos": [{"content": "read the code", "status": "completed"}, {"content": "fix the bug", "status": "in_progress"}, {"content": "test it", "status": "pending"}]});
        let todos = parse(&first).unwrap();
        assert_eq!(todos.len(), 3);
        assert_eq!(summary(&todos), "Todos updated: 1 in progress, 1 pending, 1 completed.");
        let second = json!({"todos": [{"content": "ship it", "status": "pending"}]});
        let history = vec![call("a", first, 0), result("a", false), call("b", second, 1), result("b", false)];
        assert_eq!(current(&history), [Todo { content: "ship it".into(), status: TodoStatus::Pending }], "the last write is the list, whole");
        // A refused write changes nothing.
        let mut h = history.clone();
        h.push(call("c", json!({"todos": [{"content": "", "status": "pending"}]}), 2));
        h.push(result("c", true));
        assert_eq!(current(&h).len(), 1);
        assert!(parse(&json!({"todos": [{"content": "x", "status": "done"}]})).unwrap_err().starts_with("invalid input for todo_write"));
        assert!(parse(&json!({"todos": [{"content": " ", "status": "pending"}]})).unwrap_err().contains("no content"));
        assert_eq!(summary(&[]), "The todo list is empty.");
    }

    #[test]
    fn r_todo_1_claude_codes_todowrite_shape_is_taken_and_a_list_is_bounded() {
        // Claude Code's TodoWrite, then its older shape with ids and priorities.
        let claude = json!({"todos": [{"content": "Run the tests", "status": "in_progress", "activeForm": "Running the tests"}, {"id": "2", "content": "Fix it", "status": "pending", "priority": "high"}]});
        assert_eq!(parse(&claude).unwrap(), [Todo { content: "Run the tests".into(), status: TodoStatus::InProgress }, Todo { content: "Fix it".into(), status: TodoStatus::Pending }]);
        let long = parse(&json!({"todos": [{"content": "x".repeat(2000), "status": "pending"}]})).unwrap();
        assert_eq!(long[0].content.chars().count(), MAX_CONTENT + 1, "cut, and marked so");
        let many: Vec<Value> = (0..=MAX_ITEMS).map(|i| json!({"content": format!("step {i}"), "status": "pending"})).collect();
        assert!(parse(&json!({ "todos": many })).unwrap_err().contains(&format!("at most {MAX_ITEMS}")));
    }

    #[test]
    fn r_todo_3_a_list_left_open_for_ten_calls_gets_one_reminder() {
        let list = json!({"todos": [{"content": "fix the bug", "status": "in_progress"}, {"content": "test it", "status": "pending"}]});
        let mut h = vec![call("a", list, 0), result("a", false)];
        let answer = |r: usize| HistoryItem { item: Item::AssistantText { text: "…".into() }, response: Some(r) };
        for r in 1..STALE_CALLS {
            h.push(answer(r));
        }
        assert_eq!(reminder(&h), None, "nine calls is not yet stale");
        h.push(answer(STALE_CALLS));
        let text = reminder(&h).expect("ten calls without a write");
        assert!(text.starts_with(REMINDER) && text.contains("[~] fix the bug") && text.contains("[ ] test it") && text.contains("2 of its items"), "{text}");
        // Once reminded, the count starts again.
        h.push(HistoryItem { item: Item::user(text), response: None });
        h.push(answer(STALE_CALLS + 1));
        assert_eq!(reminder(&h), None);
        // A list with nothing open, or no list at all, is never stale.
        let done = json!({"todos": [{"content": "fix the bug", "status": "completed"}]});
        let mut h = vec![call("b", done, 0), result("b", false)];
        h.extend((1..=STALE_CALLS * 2).map(answer));
        assert_eq!(reminder(&h), None);
        assert_eq!(reminder(&(1..=30).map(answer).collect::<Vec<_>>()), None);
    }
}
