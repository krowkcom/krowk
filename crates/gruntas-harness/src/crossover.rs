//! Crossing harnesses (Rollover for launched sessions D6, R-RL-6): the
//! first message of a session that continues another on the other harness,
//! when the ladder's `other-harness` rung picked its account.
//!
//! The handoff prompt is the old session's status Task as its agent last
//! reported it (summary, phase, plan, next, recent reports), after, where
//! its transcript is readable (Claude Code, through the transcript
//! importer), gruntas's handoff of its recent turns (`handoff::plan`, the
//! same summary a model switch seeds a new thread with). Codex's transcript
//! is not read: its Task alone carries it over.
//!
//! Starting the new session and linking the two Tasks is the launcher's
//! (`gruntas run`'s `continue_on`) and the daemon's (`Activity::Continued`).

use crate::engine::HistoryItem;
use crate::handoff::{self, TurnSpan};
use crate::protocol::{Item, ModelRef};
use gruntas_client::status::{PlanStatus, Task};
use std::path::Path;

/// The most recent reports the prompt lists.
const REPORTS: usize = 5;

/// The first message of the session continuing `task`'s, on the other
/// harness: its handoff, then what its Task says and what to do. A
/// `transcript` that cannot be read is passed over: the Task still says
/// where the work stands.
pub fn prompt(task: &Task, transcript: Option<&Path>) -> String {
    let status = status(task);
    let Some((items, turns)) = transcript.and_then(|p| turns(p, task).ok()) else { return status };
    handoff::plan(&items, &turns, None, &status, None, None).map_or(status, |h| h.fresh.text)
}

/// What `task` says of the work, and what the new session is to do.
fn status(task: &Task) -> String {
    let g = &task.metadata.gruntas;
    let line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let harness = g.harness.as_ref().map_or("the other harness", |h| h.value.as_str());
    let mut out = format!("You are continuing session {} from {harness}, whose account ran out of usage. Its work is in this worktree", task.id);
    if let Some(b) = &g.branch {
        out += &format!(" (branch {})", line(&b.value));
    }
    out += ". Where it stood, as its agent last reported:\n";
    if let Some(m) = &task.status.message {
        out += &format!("- Summary: {}\n", line(&m.summary()));
    }
    if let Some(p) = &g.phase {
        out += &format!("- Phase: {}\n", line(&p.value));
    }
    if let Some(p) = &g.pr {
        out += &format!("- Pull request: {}\n", line(&p.value));
    }
    let reports: Vec<_> = task.history.iter().filter_map(|m| m.metadata.as_ref().map(|md| (m, &md.report))).collect();
    if let Some(next) = reports.iter().rev().find_map(|(_, r)| r.next.as_deref()) {
        out += &format!("- Next: {}\n", line(next));
    }
    if let Some(plan) = g.plan.as_ref().filter(|p| !p.value.is_empty()) {
        out += "- Plan:\n";
        for e in &plan.value {
            let mark = match e.status {
                PlanStatus::Completed => "done",
                PlanStatus::InProgress => "in progress",
                PlanStatus::Pending | PlanStatus::Unknown => "pending",
            };
            out += &format!("  - [{mark}] {}\n", line(&e.content));
        }
    }
    if !reports.is_empty() {
        out += "- Recent reports, oldest first:\n";
        for (m, r) in &reports[reports.len().saturating_sub(REPORTS)..] {
            let mut said = line(&m.summary());
            if let Some(p) = &r.phase {
                said += &format!(" (phase: {})", line(p));
            }
            out += &format!("  - {} {said}\n", r.timestamp);
        }
    }
    out += "Continue the work from where it stopped, and report your status as you go.";
    out
}

/// A Claude Code transcript's recent part as the branch `handoff::plan`
/// reads: its items, and a turn from each prompt the person typed. A turn
/// is on the old session's harness and the model its first answer names.
fn turns(path: &Path, task: &Task) -> Result<(Vec<HistoryItem>, Vec<TurnSpan>), String> {
    let instance = task.metadata.gruntas.harness.as_ref().map_or_else(|| "claude".to_string(), |h| h.value.clone());
    let mut items: Vec<HistoryItem> = Vec::new();
    let mut turns: Vec<TurnSpan> = Vec::new();
    for m in crate::sync::god::read::claude_tail(path)? {
        for p in &m.parts {
            let data: serde_json::Value = serde_json::from_str(&p.data).unwrap_or_default();
            let call_id = || if p.tool_call_id.is_empty() { data["id"].as_str().unwrap_or_default().to_string() } else { p.tool_call_id.clone() };
            let item = match (p.kind.as_str(), m.role) {
                (gruntas_import::PART_TEXT, gruntas_store::Role::User) => {
                    turns.push(TurnSpan { model: ModelRef { instance: instance.clone(), model: String::new() }, items: items.len()..items.len() });
                    Item::UserText { text: data["text"].as_str().unwrap_or_default().to_string(), images: Vec::new(), origin: None }
                }
                (gruntas_import::PART_TEXT, gruntas_store::Role::Assistant) => Item::AssistantText { text: data["text"].as_str().unwrap_or_default().to_string() },
                (gruntas_import::PART_TOOL_CALL, _) => Item::ToolCall { call_id: call_id(), name: data["name"].as_str().unwrap_or_default().to_string(), input: data["input"].clone() },
                (gruntas_import::PART_TOOL_RESULT, _) => Item::ToolResult { call_id: call_id(), output: data["output"].as_str().map_or_else(|| data["output"].to_string(), str::to_string), is_error: data["is_error"].as_bool().unwrap_or_default(), images: Vec::new() },
                _ => continue,
            };
            // Whatever came before the first prompt in the tail is left out.
            let Some(turn) = turns.last_mut() else { continue };
            if turn.model.model.is_empty() && m.role == gruntas_store::Role::Assistant {
                turn.model.model.clone_from(&m.model);
            }
            items.push(HistoryItem { item, response: None });
            turn.items.end = items.len();
        }
    }
    Ok((items, turns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_client::status::{Field, Message, MessageMetadata, PlanEntry, PlanPriority, Report, TaskMetadata, TaskState, TaskStatus};
    use serde_json::json;

    /// The old session's Task, as its agent reported it.
    fn task() -> Task {
        let report = |i: usize, next: Option<&str>| Message {
            metadata: Some(MessageMetadata { report: Report { phase: Some(format!("phase {i}")), next: next.map(String::from), timestamp: format!("2026-11-24T12:0{i}:00Z"), ..Report::default() } }),
            ..Message::text(format!("r{i}"), format!("report {i}"))
        };
        let mut t = Task { id: "old-session".into(), status: TaskStatus { state: TaskState::Working, message: Some(Message::text("m", "wiring the\nhandoff")), timestamp: "2026-11-24T12:06:00Z".into() }, history: (0..7).map(|i| report(i, (i == 5).then_some("write the status test"))).collect(), metadata: TaskMetadata::default() };
        let g = &mut t.metadata.gruntas;
        g.harness = Some(Field::harness("claude".into()));
        g.branch = Some(Field::harness("feature/rl04".into()));
        g.phase = Some(Field::agent("build".into()));
        g.pr = Some(Field::agent("https://github.com/acme/app/pull/7".into()));
        let entry = |content: &str, status| PlanEntry { content: content.into(), priority: PlanPriority::Medium, status };
        g.plan = Some(Field::agent(vec![entry("read the ladder", PlanStatus::Completed), entry("build the prompt", PlanStatus::InProgress), entry("link the Tasks", PlanStatus::Pending)]));
        t
    }

    #[test]
    fn rl04_the_handoff_prompt_carries_the_task() {
        let p = prompt(&task(), None);
        for want in [
            "You are continuing session old-session from claude",
            "(branch feature/rl04)",
            "- Summary: wiring the handoff\n",
            "- Phase: build\n",
            "- Pull request: https://github.com/acme/app/pull/7\n",
            "- Next: write the status test\n",
            "  - [done] read the ladder\n  - [in progress] build the prompt\n  - [pending] link the Tasks\n",
            "  - 2026-11-24T12:02:00Z report 2 (phase: phase 2)\n",
            "  - 2026-11-24T12:06:00Z report 6 (phase: phase 6)\n",
        ] {
            assert!(p.contains(want), "{want:?} in {p}");
        }
        assert!(!p.contains("report 1 "), "only the last {REPORTS} reports: {p}");
        assert!(p.ends_with("Continue the work from where it stopped, and report your status as you go."), "{p}");
    }

    #[test]
    fn rl04_a_claude_transcript_adds_the_handoff_of_its_recent_turns() {
        let dir = std::env::temp_dir().join(format!("gruntas-crossover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut lines = String::new();
        for i in 0..5 {
            let user = json!({ "type": "user", "uuid": format!("u{i}"), "sessionId": "old-session", "message": { "role": "user", "content": format!("prompt {i}") } });
            let assistant = json!({ "type": "assistant", "uuid": format!("a{i}"), "sessionId": "old-session", "message": { "role": "assistant", "model": "claude-sonnet-4-6", "content": [{ "type": "tool_use", "id": format!("c{i}"), "name": "Bash", "input": { "command": format!("make step-{i}") } }, { "type": "text", "text": format!("answer {i}") }] } });
            lines += &format!("{user}\n{assistant}\n");
        }
        std::fs::write(&path, lines).unwrap();
        let p = prompt(&task(), Some(&path));
        assert!(p.starts_with("<handoff from gruntas>\n") && p.contains("on claude/claude-sonnet-4-6"), "{p}");
        assert!(p.contains("2 earlier turns summarized, then the last 3"), "{p}");
        assert!(p.contains("prompt 4") && p.contains("answer 4") && p.contains("make step-4"), "{p}");
        let task_part = p.split("</handoff>\n\n").nth(1).unwrap();
        assert!(task_part.starts_with("You are continuing session old-session") && task_part.contains("- Next: write the status test"), "{task_part}");
        assert_eq!(prompt(&task(), Some(&dir.join("missing.jsonl"))), prompt(&task(), None), "an unreadable transcript leaves the Task");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
