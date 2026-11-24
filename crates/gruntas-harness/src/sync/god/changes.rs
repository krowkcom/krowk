//! `changes` (GA03, D4): what this machine's sessions did since a time, from
//! the daemon's 24-hour log of their state transitions and reports.
//!
//! The log is kept by the board's publisher (`boards::tick`), which reads
//! the daemon's Tasks every `boards::POLL`: a session whose state moved is a
//! transition, one whose summary changed is a report (with the state it
//! set). Kept in `FILE` beside `status.json`, with what each session was
//! last seen as, so a restarted daemon logs no session twice. Each change's
//! time is later than every one before it, so an answer's `at`, the last
//! change it holds, is a `since` that skips nothing: neither a change
//! logged as the answer was read nor one an answer cut for size left out.

use super::super::auto::Config;
use crate::daemon::registry::write_private;
use gruntas_client::host_command::{Change, ChangesSince, HostAnswer, MAX_RECORD};
use gruntas_client::status::{Task, TaskState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const FILE: &str = "changes.json";
/// How long a change is kept.
const KEPT: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);
/// The most changes kept, however recent.
const MAX_CHANGES: usize = 5000;
/// The most JSON an answer's changes take: within an answer record, with
/// room for its sealing. One change is far less (a summary is at most
/// `registry::SUMMARY_MOST` characters).
const MOST: usize = MAX_RECORD * 3 / 4;

/// What a session was last seen as: its state and its summary.
type Seen = (TaskState, String);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Log {
    #[serde(default)]
    seen: BTreeMap<String, Seen>,
    /// Oldest first, each later than the one before.
    #[serde(default)]
    changes: Vec<Change>,
}

impl Log {
    fn load(dir: &Path) -> Log {
        std::fs::read(dir.join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
}

/// The publisher's log of the changes, loaded at its first look and kept.
#[derive(Debug, Default)]
pub struct Watch {
    log: Option<Log>,
}

impl Watch {
    /// `tasks`, as the daemon's board has them `now`: each session's change
    /// since the last look added to `dir`'s log, the log cut to 24 hours
    /// and `MAX_CHANGES`. Written only when something changed.
    pub fn observe(&mut self, dir: &Path, tasks: &[Task], now: jiff::Timestamp) {
        let log = self.log.get_or_insert_with(|| Log::load(dir));
        let mut at = log.changes.last().and_then(|c| c.at.parse::<jiff::Timestamp>().ok()).map_or(now, |last| now.max(last));
        let mut changed = false;
        for t in tasks {
            let state = t.status.state;
            let summary = t.status.message.as_ref().map(|m| m.summary()).unwrap_or_default();
            let was = log.seen.get(&t.id);
            let moved = was.is_none_or(|w| w.0 != state);
            let reported = !summary.is_empty() && was.is_none_or(|w| w.1 != summary);
            if moved || reported {
                at += jiff::SignedDuration::from_nanos(1);
                log.changes.push(Change { session: t.id.clone(), at: at.to_string(), state: Some(state), summary: reported.then(|| summary.clone()) });
                changed = true;
            }
            if was != Some(&(state, summary.clone())) {
                log.seen.insert(t.id.clone(), (state, summary));
                changed = true;
            }
        }
        // A session no longer on the board is forgotten.
        let before = log.seen.len();
        log.seen.retain(|id, _| tasks.iter().any(|t| t.id == *id));
        if !changed && log.seen.len() == before {
            return;
        }
        let oldest = now - KEPT;
        log.changes.retain(|c| c.at.parse::<jiff::Timestamp>().is_ok_and(|t| t >= oldest));
        let over = log.changes.len().saturating_sub(MAX_CHANGES);
        log.changes.drain(..over);
        if let Ok(bytes) = serde_json::to_vec(&*log) {
            let _ = write_private(dir, FILE, &bytes);
        }
    }
}

/// The answer to `changes { since }`: `HostAnswer::Changes`, oldest first.
pub(crate) fn answer(c: &Config, since: &str) -> HostAnswer {
    let dir = c.socket.parent().unwrap_or(Path::new("/"));
    HostAnswer::Changes(since_in(dir, since))
}

/// `dir`'s logged changes after `since`, oldest first, as many as an answer
/// record holds; `at`, the next `since`, the last of them (`since` itself
/// when there are none).
fn since_in(dir: &Path, since: &str) -> ChangesSince {
    let from = since.parse::<jiff::Timestamp>().ok();
    let mut changes: Vec<Change> = Vec::new();
    let mut size = 0;
    for change in Log::load(dir).changes {
        if from.is_some_and(|f| change.at.parse::<jiff::Timestamp>().is_ok_and(|t| t <= f)) {
            continue;
        }
        size += serde_json::to_vec(&change).map_or(0, |b| b.len() + 1);
        if size > MOST {
            break;
        }
        changes.push(change);
    }
    let at = changes.last().map_or_else(|| since.to_string(), |c| c.at.clone());
    ChangesSince { since: since.to_string(), at, changes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_client::status::{Message, TaskMetadata, TaskStatus};

    fn task(id: &str, state: TaskState, summary: &str) -> Task {
        let message = (!summary.is_empty()).then(|| Message::text("m", summary));
        Task { id: id.into(), status: TaskStatus { state, message, timestamp: String::new() }, history: Vec::new(), metadata: TaskMetadata::default() }
    }

    fn at(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    /// D4: transitions and reports are logged, a day's worth; asked since a
    /// time, the ones after it come back, oldest first; a restarted watch
    /// logs nothing it saw before.
    #[test]
    fn ga03_changes_since_a_time_are_its_transitions_and_reports() {
        let dir = std::env::temp_dir().join(format!("gruntas-ga03-changes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.as_path();
        let mut w = Watch::default();
        w.observe(d, &[task("a", TaskState::Working, "")], at("2026-11-23T09:00:00Z"));
        w.observe(d, &[task("a", TaskState::Working, "")], at("2026-11-24T09:00:00Z"));
        w.observe(d, &[task("a", TaskState::InputRequired, ""), task("b", TaskState::Working, "")], at("2026-11-24T10:00:00Z"));
        w.observe(d, &[task("a", TaskState::Completed, "built it"), task("b", TaskState::Working, "")], at("2026-11-24T10:05:00Z"));
        Watch::default().observe(d, &[task("a", TaskState::Completed, "built it"), task("b", TaskState::Working, "")], at("2026-11-24T10:06:00Z"));
        let all = since_in(d, "2026-11-24T00:00:00Z");
        let said: Vec<(&str, Option<TaskState>, Option<&str>)> = all.changes.iter().map(|c| (c.session.as_str(), c.state, c.summary.as_deref())).collect();
        assert_eq!(said, [("a", Some(TaskState::InputRequired), None), ("b", Some(TaskState::Working), None), ("a", Some(TaskState::Completed), Some("built it"))]);
        // Logged at one look, each later than the one before; the next since
        // is the last.
        assert_eq!(all.changes[..2].iter().map(|c| c.at.as_str()).collect::<Vec<_>>(), ["2026-11-24T10:00:00.000000001Z", "2026-11-24T10:00:00.000000002Z"]);
        assert_eq!(all.at, all.changes[2].at);
        // The first sight, more than a day before the last change, was cut.
        assert_eq!(Log::load(d).changes.len(), 3);
        assert_eq!(since_in(d, "2026-11-24T10:00:00.000000002Z").changes.len(), 1);
        assert_eq!(since_in(d, &all.at).at, all.at);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
