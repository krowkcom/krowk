//! The sessions of harnesses gruntas does not run itself (Claude Code,
//! Codex), as their plugin's hooks registered them (`activity` frames).
//! Kept in `registrations.json` beside the daemon's socket, so a daemon that
//! exits and starts again still knows them; read when it starts, written
//! after every change. An entry goes when its session ends, or when the
//! harness process it named is gone (`prune`).
//!
//! The file is the daemon's alone: written whole to a temporary file and
//! renamed over, `0600`, in a directory only this user can open (`super::dir`).
//! One that cannot be read is started over, never refused: the next
//! `SessionStart` registers its session again.
//!
//! Beside each registration it keeps the session's status (SL03, D2): the
//! A2A Task its agent's reports (`gruntas_report`) build, and whether the
//! session did work (tool calls) since its last report, which its Stop
//! hook reminds it of.

use crate::protocol::{Activity, PluginHarness, Registration};
use gruntas_client::presence::normalise_origin;
use gruntas_client::status::{self, Field, Health, Message, MessageMetadata, Provenance, Report, Task, TaskMetadata, TaskState, TaskStatus};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const FILE: &str = "registrations.json";

/// More than any machine runs at once: a hook that registers without end
/// cannot grow the file past this, the first by id making room.
const MOST: usize = 1024;

/// The most characters a report's summary keeps, and its other words
/// (phase, blocked on, next): capped as they come in, so ten reports a
/// session stay well inside a board's budget (D8).
pub const SUMMARY_MOST: usize = 200;
pub const WORD_MOST: usize = 120;
/// A pull request's URL.
pub const PR_MOST: usize = 300;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    /// By session id.
    #[serde(default)]
    sessions: BTreeMap<String, Registration>,
    /// When each session was first registered, RFC 3339 by this machine's
    /// clock: its status's time until anything newer is known.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    registered_at: BTreeMap<String, String>,
    /// Each reported session's status, by session id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tasks: BTreeMap<String, Task>,
    /// The sessions that did work since their last report or reminder.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    unreported: BTreeSet<String>,
}

impl Registry {
    /// The registry `dir` holds, its dead entries pruned; empty when there
    /// is none or it cannot be read, which is said.
    // blocking: once, as the daemon starts.
    pub fn load(dir: &Path) -> Registry {
        Registry::from_file(dir, true)
    }

    /// `load`, saying nothing when the file cannot be read: for a reader
    /// that reads it again and again (the status board's publisher).
    // blocking: a file read.
    pub fn read(dir: &Path) -> Registry {
        Registry::from_file(dir, false)
    }

    fn from_file(dir: &Path, say: bool) -> Registry {
        let path = dir.join(FILE);
        let mut r = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                if say {
                    eprintln!("{} cannot be read ({e}): starting with no registrations", path.display());
                }
                Registry::default()
            }),
            Err(_) => Registry::default(),
        };
        r.prune(alive);
        r
    }

    /// Times every session kept from before registrations were timed, from
    /// now; whether any was. The daemon calls it as it loads the file, and
    /// saves it when it was, so the time holds from then on.
    pub fn stamp(&mut self) -> bool {
        let now = jiff::Timestamp::now().to_string();
        let mut stamped = false;
        for id in self.sessions.keys() {
            if !self.registered_at.contains_key(id) {
                self.registered_at.insert(id.clone(), now.clone());
                stamped = true;
            }
        }
        stamped
    }

    /// Writes it to `dir`, whole, in place of what was there.
    // blocking: the caller runs it off the daemon's thread.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        save_json(dir, &serde_json::to_vec(self).expect("a registry serializes"))
    }

    /// What it would write: for a caller that saves elsewhere than here.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a registry serializes")
    }

    /// Takes one activity; whether the registrations changed. Heavy
    /// commands and usage windows are not registrations: they change
    /// nothing here.
    pub fn apply(&mut self, a: &Activity) -> bool {
        match a {
            Activity::Register(r) => {
                if self.sessions.get(&r.session_id) == Some(r) {
                    return false;
                }
                if self.sessions.len() >= MOST && !self.sessions.contains_key(&r.session_id) {
                    let first = self.sessions.keys().next().cloned();
                    if let Some(k) = first {
                        self.forget(&k);
                    }
                }
                self.registered_at.entry(r.session_id.clone()).or_insert_with(|| jiff::Timestamp::now().to_string());
                self.sessions.insert(r.session_id.clone(), r.clone());
                true
            }
            Activity::Deregister { session_id } => self.forget(session_id),
            Activity::ToolEnd { session_id } | Activity::HeavyEnd { session_id, .. } => self.sessions.contains_key(session_id) && self.unreported.insert(session_id.clone()),
            Activity::TurnEnd { session_id } => self.unreported.remove(session_id),
            Activity::Report { session_id, summary, report } => self.report(session_id, summary, report, jiff::Timestamp::now()),
            Activity::HeavyStart { .. } | Activity::PermissionRequest { .. } | Activity::Active { .. } | Activity::Launched { .. } | Activity::Usage { .. } => false,
        }
    }

    /// Drops a session and its status; whether it was there.
    fn forget(&mut self, session_id: &str) -> bool {
        self.registered_at.remove(session_id);
        self.tasks.remove(session_id);
        self.unreported.remove(session_id);
        self.sessions.remove(session_id).is_some()
    }

    /// Takes a registered session's report, made at `now` (this machine's
    /// clock, whatever the report says): its Task's state (kept when the
    /// report names none, working for a first), summary, phase and PR set
    /// with agent provenance, the report appended to its history with the
    /// oldest past `status::MAX_HISTORY` dropped, and its work reported.
    /// Every word is capped as it comes in. Whether it changed anything:
    /// nothing for a session not registered.
    pub fn report(&mut self, session_id: &str, summary: &str, report: &Report, now: jiff::Timestamp) -> bool {
        if !self.sessions.contains_key(session_id) {
            return false;
        }
        let at = now.to_string();
        let word = |s: &Option<String>, most: usize| s.as_deref().map(|s| cap(s, most)).filter(|s| !s.is_empty());
        // A state this build does not know is no state; a PR is a web link
        // or nothing, as a reader may open it.
        let state = report.state.filter(|s| *s != TaskState::Unspecified);
        // Never cut: a shortened link opens another page.
        let pr = report.pr.as_deref().map(str::trim).filter(|p| p.chars().count() <= PR_MOST && is_web_link(p)).map(String::from);
        let report = Report { state, phase: word(&report.phase, WORD_MOST), blocked_on: word(&report.blocked_on, WORD_MOST), pr, next: word(&report.next, WORD_MOST), timestamp: at.clone() };
        let id = format!("report-{}", now.as_nanosecond());
        let summary = cap(summary, SUMMARY_MOST);
        let task = self.tasks.entry(session_id.to_string()).or_insert_with(|| Task {
            id: session_id.to_string(),
            status: TaskStatus { state: TaskState::Working, message: None, timestamp: at.clone() },
            history: Vec::new(),
            metadata: TaskMetadata::default(),
        });
        task.status = TaskStatus { state: report.state.unwrap_or(task.status.state), message: Some(Message::text(id.clone(), summary.clone())), timestamp: at.clone() };
        let g = &mut task.metadata.gruntas;
        g.state_source = Some(Provenance::Agent);
        if let Some(p) = &report.phase {
            g.phase = Some(Field::agent(p.clone()));
        }
        if let Some(p) = &report.pr {
            g.pr = Some(Field::agent(p.clone()));
        }
        g.last_activity = Some(Field::harness(at));
        let mut entry = Message::text(id, summary);
        entry.metadata = Some(MessageMetadata { report });
        task.history.push(entry);
        let over = task.history.len().saturating_sub(status::MAX_HISTORY);
        task.history.drain(..over);
        self.unreported.remove(session_id);
        true
    }

    /// A session's status, once its agent has reported.
    pub fn task(&self, session_id: &str) -> Option<&Task> {
        self.tasks.get(session_id)
    }

    /// Whether a session did work (tool calls) since its last report or
    /// reminder.
    pub fn report_due(&self, session_id: &str) -> bool {
        self.unreported.contains(session_id)
    }

    /// The status board's Tasks (D1, D8): one per session whose harness
    /// process is still there, by session id, with what registration
    /// observed (the harness, the project, health `SERVING`, and the
    /// registration as its last activity), all with harness provenance.
    /// A session's state is `working` until a report or a hook says
    /// otherwise; a session whose agent reported starts from its reported
    /// Task (SL03). The machine is the publisher's to fill.
    pub fn board_tasks(&self) -> Vec<Task> {
        self.sessions()
            .map(|r| {
                let at = self.registered_at.get(&r.session_id).cloned().unwrap_or_else(|| jiff::Timestamp::UNIX_EPOCH.to_string());
                let reported = self.tasks.get(&r.session_id).cloned();
                let mut t = reported.clone().unwrap_or_else(|| Task { id: r.session_id.clone(), status: TaskStatus { state: TaskState::Working, message: None, timestamp: at.clone() }, history: Vec::new(), metadata: TaskMetadata::default() });
                let g = &mut t.metadata.gruntas;
                if reported.is_none() {
                    g.state_source = Some(Provenance::Harness);
                    g.last_activity = Some(Field::harness(at));
                }
                g.health = Some(Field::harness(Health::Serving));
                g.harness = Some(Field::harness(match r.harness {
                    PluginHarness::Claude => "claude".into(),
                    PluginHarness::Codex => "codex".into(),
                }));
                g.project = r.repository.as_deref().and_then(normalise_origin).map(Field::harness);
                t
            })
            .collect()
    }

    /// Drops every entry whose harness process `alive` says is gone;
    /// whether any went. One that named no process stays until its session
    /// ends.
    pub fn prune(&mut self, alive: impl Fn(u32) -> bool) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|_, r| r.harness_pid.is_none_or(&alive));
        let sessions = &self.sessions;
        self.registered_at.retain(|id, _| sessions.contains_key(id));
        self.tasks.retain(|id, _| sessions.contains_key(id));
        self.unreported.retain(|id| sessions.contains_key(id));
        self.sessions.len() != before
    }

    /// A session, unless its harness process is gone: what is read is
    /// never staler than the read, however long since the last prune.
    pub fn get(&self, session_id: &str) -> Option<&Registration> {
        self.sessions.get(session_id).filter(|r| r.harness_pid.is_none_or(alive))
    }

    /// Every session whose harness process is still there.
    pub fn sessions(&self) -> impl Iterator<Item = &Registration> {
        self.sessions.values().filter(|r| r.harness_pid.is_none_or(alive))
    }
}

/// Whether `s` is an `http` or `https` link, with nothing in it a parser
/// would quietly drop (white space, control characters).
pub fn is_web_link(s: &str) -> bool {
    !s.chars().any(|c| c.is_whitespace() || c.is_control()) && url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
}

/// `s` cut to at most `most` characters, an ellipsis last when cut, and
/// trimmed.
fn cap(s: &str, most: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= most {
        return s.to_string();
    }
    s.chars().take(most - 1).collect::<String>() + "…"
}

/// Writes `bytes` as `dir`'s registry: to a temporary file, then renamed
/// over, so a reader never sees half of one.
pub fn save_json(dir: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_private(dir, FILE, bytes)
}

/// Writes `bytes` as `dir`'s `file`, `0600`, as `save_json` writes the
/// registry. One writer at a time: the temporary file is per process.
pub fn write_private(dir: &Path, file: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = dir.join(format!("{file}.{}.tmp", std::process::id()));
    let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::rename(&tmp, dir.join(file))
}

/// Whether process `pid` is still there: one that exists but is another
/// user's still counts.
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::UsageWindow;

    fn reg(id: &str, pid: Option<u32>) -> Registration {
        Registration { session_id: id.into(), harness: PluginHarness::Claude, harness_pid: pid, config_home: None, cwd: Some("/r".into()), repository: None, transcript_path: None, attendance: None }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gruntas-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn kt01_register_and_deregister_change_it_and_the_rest_do_not() {
        let mut r = Registry::default();
        assert!(r.apply(&Activity::Register(reg("a", None))));
        assert!(!r.apply(&Activity::Register(reg("a", None))), "the same registration again changes nothing");
        assert!(!r.apply(&Activity::HeavyStart { session_id: "a".into(), tool_call_id: "t".into() }));
        assert!(!r.apply(&Activity::Usage { config_home: "/h".into(), windows: vec![UsageWindow { kind: "fiveHour".into(), used_percent: 1.0, resets_at_ms: None }] }));
        assert_eq!(r.get("a"), Some(&reg("a", None)));
        assert!(r.apply(&Activity::Deregister { session_id: "a".into() }));
        assert!(!r.apply(&Activity::Deregister { session_id: "a".into() }));
        assert_eq!(r.sessions().count(), 0);
    }

    #[test]
    fn kt01_it_is_read_back_as_written_with_its_dead_entries_pruned() {
        let dir = scratch("persist");
        let mut r = Registry::default();
        r.apply(&Activity::Register(reg("live", Some(std::process::id()))));
        r.apply(&Activity::Register(reg("unnamed", None)));
        // A process that has exited, its pid not reused this soon.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        r.apply(&Activity::Register(reg("dead", Some(dead))));
        r.save(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        let back = Registry::load(&dir);
        let ids: Vec<&str> = back.sessions().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, ["live", "unnamed"]);
        assert_eq!(back.get("live"), r.get("live"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kt01_prune_drops_only_the_entries_whose_process_is_gone() {
        let mut r = Registry::default();
        for (id, pid) in [("a", Some(1)), ("b", Some(2)), ("c", None)] {
            r.apply(&Activity::Register(reg(id, pid)));
        }
        assert!(r.prune(|pid| pid == 1));
        assert!(!r.prune(|pid| pid == 1));
        let ids: Vec<&str> = r.sessions().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);
    }

    #[test]
    fn kt01_a_registry_that_cannot_be_read_starts_empty() {
        let dir = scratch("garbage");
        std::fs::write(dir.join(FILE), b"{not json").unwrap();
        assert_eq!(Registry::load(&dir), Registry::default());
        assert_eq!(Registry::load(&dir.join("absent")), Registry::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kt01_it_never_holds_more_than_its_bound() {
        let mut r = Registry::default();
        for n in 0..MOST + 5 {
            r.apply(&Activity::Register(reg(&format!("s{n:05}"), None)));
        }
        assert_eq!(r.sessions().count(), MOST);
        assert!(r.get(&format!("s{:05}", MOST + 4)).is_some());
    }

    /// SL05: the board lists one Task per live session, with what
    /// registration observed and a time that holds until the session
    /// changes, so an unchanged registry is an unchanged board.
    #[test]
    fn sl05_the_board_has_a_task_per_live_session_with_harness_fields() {
        let mut r = Registry::default();
        let mut a = reg("a", None);
        a.repository = Some("git@github.com:acme/app.git".into());
        r.apply(&Activity::Register(a.clone()));
        r.apply(&Activity::Register(reg("b", Some(u32::MAX))));
        let tasks = r.board_tasks();
        assert_eq!(tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["a"], "a session whose harness is gone is not on it");
        let (t, g) = (&tasks[0], &tasks[0].metadata.gruntas);
        assert_eq!((t.status.state, g.state_source), (TaskState::Working, Some(Provenance::Harness)));
        assert_eq!((g.harness.as_ref().map(|f| f.value.as_str()), g.project.as_ref().map(|f| f.value.as_str())), (Some("claude"), Some("github.com/acme/app")));
        assert_eq!(g.health, Some(Field::harness(Health::Serving)));
        assert_eq!(g.last_activity.as_ref().map(|f| &f.value), Some(&t.status.timestamp));
        r.apply(&Activity::Register(a));
        assert_eq!(r.board_tasks(), tasks, "the same registration again changes nothing");
        assert!(!r.stamp(), "every registration is timed already");
        r.registered_at.clear();
        assert!(r.stamp() && !r.stamp(), "one kept untimed is timed once");
        r.apply(&Activity::Deregister { session_id: "a".into() });
        assert!(r.board_tasks().is_empty() && !r.registered_at.contains_key("a"));
    }

    fn report(state: Option<TaskState>) -> Report {
        Report { state, timestamp: "1999-01-01T00:00:00Z".into(), ..Report::default() }
    }

    #[test]
    fn sl03_a_report_sets_the_task_with_agent_provenance_and_keeps_ten() {
        let mut r = Registry::default();
        let at = |n: i64| jiff::Timestamp::from_second(1_800_000_000 + n).unwrap();
        assert!(!r.report("a", "x", &report(None), at(0)), "not registered");
        assert_eq!(r.task("a"), None);
        r.apply(&Activity::Register(reg("a", None)));
        assert!(r.report("a", "first", &report(None), at(0)));
        assert_eq!(r.task("a").unwrap().status.state, TaskState::Working, "working when the first names no state");
        let full = Report { state: Some(TaskState::InputRequired), phase: Some(" review ".into()), blocked_on: Some("a decision".into()), pr: Some("https://github.com/o/r/pull/1".into()), next: Some("merge".into()), timestamp: "ignored".into() };
        r.report("a", "waiting on you", &full, at(1));
        r.report("a", "still", &report(None), at(2));
        let t = r.task("a").unwrap();
        assert_eq!((t.status.state, t.status.message.as_ref().unwrap().summary().as_str()), (TaskState::InputRequired, "still"), "the state stays when a report names none");
        assert_eq!(t.status.timestamp, at(2).to_string(), "this machine's clock");
        let g = &t.metadata.gruntas;
        assert_eq!(g.state_source, Some(Provenance::Agent));
        assert_eq!(g.phase, Some(Field::agent("review".to_string())));
        assert_eq!(g.pr, Some(Field::agent("https://github.com/o/r/pull/1".to_string())));
        let second = t.history[1].metadata.as_ref().unwrap();
        assert_eq!((second.report.blocked_on.as_deref(), second.report.next.as_deref(), second.report.timestamp.as_str()), (Some("a decision"), Some("merge"), at(1).to_string().as_str()));
        for n in 3..12 {
            r.report("a", &format!("r{n}"), &report(None), at(n));
        }
        let t = r.task("a").unwrap();
        assert_eq!(t.history.len(), status::MAX_HISTORY);
        assert_eq!(t.history[0].summary(), "still", "twelve reports: the oldest two went");
        assert_eq!(t.history[9].summary(), "r11");
    }

    #[test]
    fn sl03_report_words_are_capped_as_they_come_in() {
        let mut r = Registry::default();
        r.apply(&Activity::Register(reg("a", None)));
        let long = "x".repeat(10_000);
        r.report("a", &long, &Report { phase: Some(long.clone()), timestamp: String::new(), ..Report::default() }, jiff::Timestamp::now());
        let t = r.task("a").unwrap();
        assert_eq!(t.status.message.as_ref().unwrap().summary().chars().count(), SUMMARY_MOST);
        assert_eq!(t.metadata.gruntas.phase.as_ref().unwrap().value.chars().count(), WORD_MOST);
    }

    #[test]
    fn sl03_work_is_due_until_reported_or_reminded_and_goes_with_its_session() {
        let mut r = Registry::default();
        assert!(!r.apply(&Activity::ToolEnd { session_id: "a".into() }), "not registered");
        r.apply(&Activity::Register(reg("a", None)));
        assert!(!r.report_due("a"));
        assert!(r.apply(&Activity::ToolEnd { session_id: "a".into() }));
        assert!(!r.apply(&Activity::ToolEnd { session_id: "a".into() }), "already due");
        assert!(r.report_due("a"));
        r.report("a", "done", &report(None), jiff::Timestamp::now());
        assert!(!r.report_due("a"), "reported");
        r.apply(&Activity::ToolEnd { session_id: "a".into() });
        assert!(r.apply(&Activity::TurnEnd { session_id: "a".into() }));
        assert!(!r.report_due("a"), "reminded once");
        r.apply(&Activity::ToolEnd { session_id: "a".into() });
        r.apply(&Activity::Deregister { session_id: "a".into() });
        assert!(!r.report_due("a") && r.task("a").is_none(), "gone with its session");
        // And with a harness that went.
        r.apply(&Activity::Register(reg("b", Some(1))));
        r.report("b", "x", &report(None), jiff::Timestamp::now());
        r.prune(|_| false);
        assert_eq!(r.task("b"), None);
    }

    #[test]
    fn sl03_a_heavy_commands_end_is_work_and_a_bad_state_or_link_is_dropped() {
        let mut r = Registry::default();
        r.apply(&Activity::Register(reg("a", None)));
        assert!(r.apply(&Activity::HeavyEnd { session_id: "a".into(), tool_call_id: "t".into() }));
        assert!(r.report_due("a"));
        let odd = Report { state: Some(TaskState::Unspecified), pr: Some("javascript:alert(1)".into()), timestamp: String::new(), ..Report::default() };
        r.report("a", "x", &odd, jiff::Timestamp::now());
        let t = r.task("a").unwrap();
        assert_eq!((t.status.state, &t.metadata.gruntas.pr), (TaskState::Working, &None));
        assert!(is_web_link("https://github.com/o/r/pull/1") && !is_web_link("file:///etc/passwd") && !is_web_link("github.com/o/r") && !is_web_link("https://git\nhub.com/o/r"));
        let long = Report { pr: Some(format!("https://x.example/{}", "y".repeat(PR_MOST))), timestamp: String::new(), ..Report::default() };
        r.report("a", "x", &long, jiff::Timestamp::now());
        assert_eq!(r.task("a").unwrap().metadata.gruntas.pr, None, "too long is dropped, never cut");
    }

    #[test]
    fn sl03_a_reported_session_is_on_the_board_as_its_agent_reported_it() {
        let mut r = Registry::default();
        r.apply(&Activity::Register(reg("a", None)));
        r.report("a", "halfway", &report(Some(TaskState::InputRequired)), jiff::Timestamp::now());
        let t = &r.board_tasks()[0];
        assert_eq!((t.status.state, t.metadata.gruntas.state_source, t.history.len()), (TaskState::InputRequired, Some(Provenance::Agent), 1));
        assert_eq!(t.metadata.gruntas.health, Some(Field::harness(Health::Serving)), "with what the harness observed");
        assert_eq!(&t.metadata.gruntas.last_activity, &r.task("a").unwrap().metadata.gruntas.last_activity);
    }

    #[test]
    fn kt01_pid_zero_and_out_of_range_are_never_alive() {
        assert!(!alive(0));
        assert!(!alive(u32::MAX));
        assert!(alive(std::process::id()));
    }
}
