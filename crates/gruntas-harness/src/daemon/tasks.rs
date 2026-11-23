//! Each registered session's status, as the daemon keeps it: one A2A Task
//! per session (`gruntas_client::status`), id the session id (D1).
//!
//! The harness's part (D3, D4), every field it sets tagged `harness`:
//!
//! - A session registered is `SERVING` and back to the agent's last state:
//!   what its agent last reported, else working.
//! - `PermissionRequest` makes it input-required; its next activity of any
//!   other kind (`PostToolUse`, `PermissionDenied`, a tool call, the turn's
//!   end) clears that back to the agent's last state.
//! - `SessionEnd` makes it completed, unless its agent last reported a
//!   terminal state, which stands.
//! - A harness process gone without `SessionEnd` makes it canceled, again
//!   unless its agent reported a terminal state, and `NOT_SERVING`.
//! - Its branch, HEAD, worktree and dirty flag are the daemon's own git run
//!   in its directory (`git`), never the hook's.
//! - Every event it is told of moves `lastActivity`.
//!
//! An agent's own state (`agent_state`) is what its reports set
//! (`Activity::Report`, SL03's `gruntas_report`). A Task
//! stays once its session is over, `NOT_SERVING`, for the board's readers
//! (D8 drops the old ones from the board); at most `MAX_TASKS` are kept,
//! the longest-gone making room.
//!
//! Kept in `status.json` beside `registrations.json`, written as it is
//! (`registry::write_private`); one that cannot be read is started over.

use crate::protocol::{Activity, PluginHarness, Registration};
use gruntas_client::status::{Field, Health, Provenance, Task, TaskMetadata, TaskState, TaskStatus, MAX_TASKS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

pub const FILE: &str = "status.json";

/// The most characters a branch name keeps, and a path: a Task's fields
/// are capped as they come in, so a board fits its budget (D8). A session
/// id longer than a name makes no Task.
const MOST_NAME: usize = 256;
const MOST_PATH: usize = 1024;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tasks {
    /// By session id.
    #[serde(default)]
    tasks: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    task: Task,
    /// The state its agent last reported, if it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_state: Option<TaskState>,
}

/// A session's git context, as `git` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Git {
    /// None when detached.
    pub branch: Option<String>,
    /// None before the first commit.
    pub head: Option<String>,
    pub worktree: String,
    pub dirty: bool,
}

/// Now, RFC 3339 to the second: a Task's times.
pub fn now() -> String {
    jiff::Timestamp::from_second(jiff::Timestamp::now().as_second()).unwrap_or(jiff::Timestamp::UNIX_EPOCH).to_string()
}

impl Tasks {
    /// The Tasks `dir` holds; none when there are none or they cannot be read.
    // blocking: once, as the daemon starts.
    pub fn load(dir: &Path) -> Tasks {
        Tasks::from_file(dir, true)
    }

    /// `load`, saying nothing when the file cannot be read: for a reader
    /// that reads it again and again (the status board's publisher).
    // blocking: a file read.
    pub fn read(dir: &Path) -> Tasks {
        Tasks::from_file(dir, false)
    }

    fn from_file(dir: &Path, say: bool) -> Tasks {
        let path = dir.join(FILE);
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                if say {
                    eprintln!("{} cannot be read ({e}): starting with no status", path.display());
                }
                Tasks::default()
            }),
            Err(_) => Tasks::default(),
        }
    }

    /// What it writes.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("tasks serialize")
    }

    /// Writes it to `dir`, whole, in place of what was there.
    // blocking: the caller runs it off the daemon's thread, or on the way out.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        super::registry::write_private(dir, FILE, &self.to_json())
    }

    pub fn get(&self, session_id: &str) -> Option<&Task> {
        self.tasks.get(session_id).map(|e| &e.task)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values().map(|e| &e.task)
    }

    /// Takes one activity at `now`; whether a Task changed. Only a
    /// registration makes a Task: any other activity of a session the
    /// daemon has none for changes nothing.
    pub fn apply(&mut self, a: &Activity, now: &str) -> bool {
        match a {
            Activity::Register(r) if r.session_id.len() > MOST_NAME => false,
            Activity::Register(r) => {
                self.make_room(&r.session_id);
                let e = self.tasks.entry(r.session_id.clone()).or_insert_with(|| Entry { task: blank(&r.session_id, now), agent_state: None });
                let before = e.clone();
                // A session resumed is at work again, whatever its agent said
                // as it ended.
                e.agent_state = e.agent_state.filter(|s| !s.is_terminal());
                let (state, source) = e.last_state();
                set_state(&mut e.task, state, source, now);
                registered(&mut e.task, r);
                touch(&mut e.task, now);
                *e != before
            }
            Activity::Deregister { session_id } => self.change(session_id, now, |e| {
                let (state, source) = match e.agent_state {
                    Some(s) if s.is_terminal() => (s, Provenance::Agent),
                    _ => (TaskState::Completed, Provenance::Harness),
                };
                set_state(&mut e.task, state, source, now);
                e.task.metadata.gruntas.health = Some(Field::harness(Health::NotServing));
            }),
            Activity::PermissionRequest { session_id } => self.change(session_id, now, |e| {
                if !e.task.status.state.is_terminal() {
                    set_state(&mut e.task, TaskState::InputRequired, Provenance::Harness, now);
                }
            }),
            // A report (SL03's `gruntas_report`) is the agent's own state:
            // the one it names, else the one it last named, else working.
            Activity::Report { session_id, report, .. } => self.change(session_id, now, |e| {
                let state = report.state.filter(|s| *s != TaskState::Unspecified).or(e.agent_state).unwrap_or(TaskState::Working);
                e.agent_state = Some(state);
                set_state(&mut e.task, state, Provenance::Agent, now);
            }),
            Activity::Active { session_id } | Activity::ToolEnd { session_id } | Activity::HeavyStart { session_id, .. } | Activity::HeavyEnd { session_id, .. } | Activity::TurnEnd { session_id } => self.change(session_id, now, |e| {
                if e.task.status.state == TaskState::InputRequired && e.task.metadata.gruntas.state_source == Some(Provenance::Harness) {
                    let (state, source) = e.last_state();
                    set_state(&mut e.task, state, source, now);
                }
            }),
            Activity::Usage { .. } => false,
        }
    }

    /// What an agent's report sets: its own state, with agent provenance,
    /// over whatever the harness last observed. Whether it changed.
    pub fn agent_state(&mut self, session_id: &str, state: TaskState, now: &str) -> bool {
        self.change(session_id, now, |e| {
            e.agent_state = Some(state);
            set_state(&mut e.task, state, Provenance::Agent, now);
        })
    }

    /// Every Task still `SERVING` whose session `live` says is gone (its
    /// harness process exited without `SessionEnd`) made `NOT_SERVING`, and
    /// canceled unless its agent reported a terminal state; whether any was.
    pub fn vanished(&mut self, live: impl Fn(&str) -> bool, now: &str) -> bool {
        let mut changed = false;
        for (id, e) in &mut self.tasks {
            if e.task.metadata.gruntas.health.as_ref().is_some_and(|h| h.value == Health::NotServing) || live(id) {
                continue;
            }
            e.task.metadata.gruntas.health = Some(Field::harness(Health::NotServing));
            if !e.task.status.state.is_terminal() {
                set_state(&mut e.task, TaskState::Canceled, Provenance::Harness, now);
            }
            changed = true;
        }
        changed
    }

    /// A session's git context, as the daemon read it: none outside a
    /// repository, which clears it. Whether it changed.
    pub fn set_git(&mut self, session_id: &str, git: Option<Git>) -> bool {
        let Some(e) = self.tasks.get_mut(session_id) else { return false };
        let g = &mut e.task.metadata.gruntas;
        let before = (g.branch.clone(), g.head.clone(), g.worktree.clone(), g.dirty.clone());
        g.branch = git.as_ref().and_then(|x| x.branch.as_deref()).map(|b| Field::harness(cap(b, MOST_NAME)));
        g.head = git.as_ref().and_then(|x| x.head.as_deref()).map(|h| Field::harness(cap(h, MOST_NAME)));
        g.worktree = git.as_ref().map(|x| Field::harness(cap(&x.worktree, MOST_PATH)));
        g.dirty = git.as_ref().map(|x| Field::harness(x.dirty));
        before != (g.branch.clone(), g.head.clone(), g.worktree.clone(), g.dirty.clone())
    }

    /// Changes `session_id`'s Task with `f`, its last activity moved to
    /// `now`; whether it changed. Nothing for a session with none.
    fn change(&mut self, session_id: &str, now: &str, f: impl FnOnce(&mut Entry)) -> bool {
        let Some(e) = self.tasks.get_mut(session_id) else { return false };
        let before = e.clone();
        f(e);
        touch(&mut e.task, now);
        *e != before
    }

    /// Room for `session_id`'s Task: at `MAX_TASKS`, the one whose state
    /// was set longest ago among those no longer serving goes, else the
    /// first by id.
    fn make_room(&mut self, session_id: &str) {
        if self.tasks.len() < MAX_TASKS || self.tasks.contains_key(session_id) {
            return;
        }
        let gone = |e: &Entry| e.task.metadata.gruntas.health.as_ref().is_some_and(|h| h.value == Health::NotServing);
        let victim = self.tasks.iter().filter(|(_, e)| gone(e)).min_by(|a, b| a.1.task.status.timestamp.cmp(&b.1.task.status.timestamp)).map(|(k, _)| k.clone()).or_else(|| self.tasks.keys().next().cloned());
        if let Some(k) = victim {
            self.tasks.remove(&k);
        }
    }
}

impl Entry {
    /// The agent's last state: what it reported, else working as far as
    /// the harness can tell.
    fn last_state(&self) -> (TaskState, Provenance) {
        match self.agent_state {
            Some(s) => (s, Provenance::Agent),
            None => (TaskState::Working, Provenance::Harness),
        }
    }
}

fn blank(session_id: &str, now: &str) -> Task {
    Task { id: session_id.to_string(), status: TaskStatus { state: TaskState::Working, message: None, timestamp: now.to_string() }, history: Vec::new(), metadata: TaskMetadata::default() }
}

fn set_state(t: &mut Task, state: TaskState, source: Provenance, now: &str) {
    if t.status.state != state || t.metadata.gruntas.state_source != Some(source) {
        t.status.state = state;
        t.status.timestamp = now.to_string();
        t.metadata.gruntas.state_source = Some(source);
    }
}

fn touch(t: &mut Task, now: &str) {
    t.metadata.gruntas.last_activity = Some(Field::harness(now.to_string()));
}

/// What the registration says of the session: it is serving, and its
/// harness, account and project.
fn registered(t: &mut Task, r: &Registration) {
    let g = &mut t.metadata.gruntas;
    g.health = Some(Field::harness(Health::Serving));
    g.harness = Some(Field::harness(
        match r.harness {
            PluginHarness::Claude => "claude",
            PluginHarness::Codex => "codex",
        }
        .to_string(),
    ));
    g.account = r.config_home.as_deref().map(|h| Field::harness(cap(h, MOST_PATH)));
    g.project = r.repository.as_deref().and_then(gruntas_client::presence::normalise_origin).map(|p| Field::harness(cap(&p, MOST_PATH)));
}

fn cap(s: &str, most: usize) -> String {
    s.chars().take(most).collect()
}

/// How long one git read (both its runs) may take before it is killed and
/// nothing read:
/// a huge tree or a stalled mount never holds a read, or the session's
/// next one, past this.
const GIT_DEADLINE: Duration = Duration::from_secs(3);

/// The most of git's output read.
const MOST_GIT_OUT: u64 = 64 << 10;

/// `cwd`'s git context, as git reads it there (hooks and fsmonitor off, no
/// optional locks, submodules not looked into, and none of the daemon's
/// own `GIT_*` variables, which are whichever session's started it): its
/// worktree's top, branch, HEAD and whether anything is changed or
/// untracked. `Some(None)` outside a repository; None when git could not
/// say (it failed, or ran past `GIT_DEADLINE`), so what was read before
/// stands.
// blocking: the daemon runs it on the blocking pool.
pub fn git(cwd: &Path) -> Option<Option<Git>> {
    // A directory gone (a worktree removed under the session) is no
    // repository any more.
    if !cwd.is_dir() {
        return Some(None);
    }
    let deadline = std::time::Instant::now() + GIT_DEADLINE;
    let query = || {
        gruntas_api::git::query(cwd).ok().map(|mut c| {
            for (k, _) in std::env::vars_os() {
                if k.to_string_lossy().starts_with("GIT_") {
                    c.env_remove(k);
                }
            }
            c.stderr(Stdio::null());
            c
        })
    };
    let mut top = query()?;
    top.args(["rev-parse", "--show-toplevel"]);
    let (out, code) = bounded(top, deadline)?;
    let worktree = String::from_utf8_lossy(&out).trim_end_matches(['\n', '\r']).to_string();
    match code {
        // git's own "not a git repository" (and a repository it refuses).
        Some(128) => return Some(None),
        Some(0) if !worktree.is_empty() => {}
        _ => return None,
    }
    let mut status = query()?;
    status.args(["status", "--porcelain=v2", "--branch", "--ignore-submodules=all"]);
    let (out, code) = bounded(status, deadline)?;
    // Cut off at `MOST_GIT_OUT`, it fails for the pipe closed: what it said
    // is still so, and dirty.
    if code != Some(0) && (out.len() as u64) < MOST_GIT_OUT {
        return None;
    }
    let (mut branch, mut head, mut dirty) = (None, None, false);
    for line in String::from_utf8_lossy(&out).lines() {
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            head = Some(oid.to_string()).filter(|o| o != "(initial)");
        } else if let Some(name) = line.strip_prefix("# branch.head ") {
            branch = Some(name.to_string()).filter(|n| n != "(detached)");
        } else if !line.starts_with('#') && !line.is_empty() {
            dirty = true;
            break;
        }
    }
    Some(Some(Git { branch, head, worktree, dirty }))
}

/// What `cmd` prints on stdout, at most `MOST_GIT_OUT` of it, before
/// `deadline`, and its exit code: none when a signal ended it, this one's
/// kill past the deadline included. None when it could not start or its
/// output did not end in time.
fn bounded(mut cmd: std::process::Command, deadline: std::time::Instant) -> Option<(Vec<u8>, Option<i32>)> {
    use std::io::Read;
    let mut child = cmd.stdout(Stdio::piped()).spawn().ok()?;
    let out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.take(MOST_GIT_OUT).read_to_end(&mut b);
        let _ = tx.send(b);
    });
    let got = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).ok();
    // Once its output is read it is given until the deadline to exit, and
    // its own status counts; past that, or with nothing read, it is killed.
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if got.is_some() && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    got.map(|out| (out, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PluginHarness;

    const T0: &str = "2026-11-23T12:00:00Z";
    const T1: &str = "2026-11-23T12:00:01Z";
    const T2: &str = "2026-11-23T12:00:02Z";

    fn reg(id: &str) -> Activity {
        Activity::Register(Registration {
            session_id: id.into(),
            harness: PluginHarness::Claude,
            harness_pid: Some(1),
            config_home: Some("/h/.claude".into()),
            cwd: Some("/r".into()),
            repository: Some("git@github.com:Acme/app.git".into()),
            transcript_path: None,
            attendance: None,
        })
    }

    fn state(t: &Tasks, id: &str) -> (TaskState, Option<Provenance>) {
        let t = t.get(id).unwrap();
        (t.status.state, t.metadata.gruntas.state_source)
    }

    fn health(t: &Tasks, id: &str) -> Health {
        t.get(id).unwrap().metadata.gruntas.health.as_ref().unwrap().value
    }

    #[test]
    fn sl04_a_registration_is_serving_and_working_with_its_fields() {
        let mut t = Tasks::default();
        assert!(t.apply(&reg("s"), T0));
        assert_eq!(state(&t, "s"), (TaskState::Working, Some(Provenance::Harness)));
        assert_eq!(health(&t, "s"), Health::Serving);
        let g = &t.get("s").unwrap().metadata.gruntas;
        assert_eq!(g.project, Some(Field::harness("github.com/Acme/app".into())));
        assert_eq!(g.harness, Some(Field::harness("claude".into())));
        assert_eq!(g.account, Some(Field::harness("/h/.claude".into())));
        assert_eq!(g.last_activity, Some(Field::harness(T0.into())));
        assert!(!t.apply(&reg("s"), T0), "the same again changes nothing");
        assert!(!t.apply(&Activity::Active { session_id: "unregistered".into() }, T1), "no Task without a registration");
        assert!(t.get("unregistered").is_none());
    }

    #[test]
    fn sl04_a_permission_request_is_input_required_until_the_next_activity() {
        for clearing in [Activity::Active { session_id: "s".into() }, Activity::HeavyEnd { session_id: "s".into(), tool_call_id: "t".into() }, Activity::TurnEnd { session_id: "s".into() }] {
            let mut t = Tasks::default();
            t.apply(&reg("s"), T0);
            assert!(t.agent_state("s", TaskState::Working, T0));
            assert!(t.apply(&Activity::PermissionRequest { session_id: "s".into() }, T1));
            assert_eq!(state(&t, "s"), (TaskState::InputRequired, Some(Provenance::Harness)));
            assert_eq!(t.get("s").unwrap().status.timestamp, T1);
            assert!(t.apply(&clearing, T2));
            assert_eq!(state(&t, "s"), (TaskState::Working, Some(Provenance::Agent)), "back to the agent's last state after {clearing:?}");
        }
        // With no report, back to working as the harness sees it.
        let mut t = Tasks::default();
        t.apply(&reg("s"), T0);
        t.apply(&Activity::PermissionRequest { session_id: "s".into() }, T1);
        t.apply(&Activity::Active { session_id: "s".into() }, T2);
        assert_eq!(state(&t, "s"), (TaskState::Working, Some(Provenance::Harness)));
    }

    #[test]
    fn sl04_activity_moves_last_activity_and_leaves_an_agents_input_required() {
        let mut t = Tasks::default();
        t.apply(&reg("s"), T0);
        t.agent_state("s", TaskState::InputRequired, T0);
        assert!(t.apply(&Activity::Active { session_id: "s".into() }, T1));
        assert_eq!(state(&t, "s"), (TaskState::InputRequired, Some(Provenance::Agent)), "the agent's own input-required stands");
        assert_eq!(t.get("s").unwrap().metadata.gruntas.last_activity, Some(Field::harness(T1.into())));
    }

    #[test]
    fn sl04_session_end_completes_unless_the_agent_reported_a_terminal_state() {
        let mut t = Tasks::default();
        t.apply(&reg("a"), T0);
        t.apply(&reg("b"), T0);
        t.agent_state("a", TaskState::Working, T0);
        t.agent_state("b", TaskState::Failed, T0);
        t.apply(&Activity::Deregister { session_id: "a".into() }, T1);
        t.apply(&Activity::Deregister { session_id: "b".into() }, T1);
        assert_eq!(state(&t, "a"), (TaskState::Completed, Some(Provenance::Harness)));
        assert_eq!(state(&t, "b"), (TaskState::Failed, Some(Provenance::Agent)));
        assert_eq!(health(&t, "a"), Health::NotServing);
        // A permission prompt changes nothing once it is over; a resume
        // starts it again.
        assert!(!t.apply(&Activity::PermissionRequest { session_id: "a".into() }, T1));
        t.apply(&reg("b"), T2);
        assert_eq!(state(&t, "b"), (TaskState::Working, Some(Provenance::Harness)));
        assert_eq!(health(&t, "b"), Health::Serving);
    }

    #[test]
    fn sl04_a_vanished_harness_is_canceled_and_not_serving() {
        let mut t = Tasks::default();
        for id in ["live", "gone", "done"] {
            t.apply(&reg(id), T0);
        }
        t.apply(&Activity::PermissionRequest { session_id: "gone".into() }, T0);
        t.agent_state("done", TaskState::Completed, T0);
        assert!(t.vanished(|id| id == "live", T1));
        assert!(!t.vanished(|id| id == "live", T2), "once");
        assert_eq!((state(&t, "live"), health(&t, "live")), ((TaskState::Working, Some(Provenance::Harness)), Health::Serving));
        assert_eq!((state(&t, "gone"), health(&t, "gone")), ((TaskState::Canceled, Some(Provenance::Harness)), Health::NotServing));
        assert_eq!(t.get("gone").unwrap().status.timestamp, T1);
        assert_eq!((state(&t, "done"), health(&t, "done")), ((TaskState::Completed, Some(Provenance::Agent)), Health::NotServing), "the agent's terminal report stands");
    }

    #[test]
    fn sl04_it_keeps_at_most_max_tasks_the_longest_gone_first() {
        let mut t = Tasks::default();
        for n in 0..MAX_TASKS {
            t.apply(&reg(&format!("s{n:05}")), T1);
        }
        t.apply(&Activity::Deregister { session_id: "s00500".into() }, T2);
        t.apply(&Activity::Deregister { session_id: "s00700".into() }, T1);
        t.apply(&reg("new"), T2);
        assert_eq!(t.tasks().count(), MAX_TASKS);
        assert!(t.get("s00700").is_none() && t.get("s00500").is_some() && t.get("new").is_some());
    }

    #[test]
    fn sl04_git_reads_branch_head_worktree_and_dirty_and_none_outside_a_repository() {
        let root = std::env::temp_dir().join(format!("gruntas-tasks-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let root = root.canonicalize().unwrap();
        let git_ = |args: &[&str]| assert!(std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgSign=false"]).args(args).current_dir(&root).stdout(Stdio::null()).status().unwrap().success());
        git_(&["init", "-q", "-b", "trunk"]);
        let g = git(&root.join("sub")).unwrap().unwrap();
        assert_eq!((g.branch.as_deref(), g.head.as_deref(), g.worktree.as_str(), g.dirty), (Some("trunk"), None, root.to_str().unwrap(), false));
        std::fs::write(root.join("f"), "x").unwrap();
        assert!(git(&root).unwrap().unwrap().dirty, "untracked counts");
        git_(&["add", "f"]);
        git_(&["commit", "-q", "-m", "c"]);
        let g = git(&root).unwrap().unwrap();
        assert_eq!(g.head.as_ref().map(String::len), Some(40));
        assert!(!g.dirty);
        git_(&["checkout", "-q", "--detach"]);
        assert_eq!(git(&root).unwrap().unwrap().branch, None);
        let mut t = Tasks::default();
        t.apply(&reg("s"), T0);
        assert!(t.set_git("s", Some(g.clone())));
        assert!(!t.set_git("s", Some(g)));
        assert_eq!(t.get("s").unwrap().metadata.gruntas.worktree, Some(Field::harness(root.display().to_string())));
        assert!(t.set_git("s", None));
        assert_eq!(t.get("s").unwrap().metadata.gruntas.branch, None);
        assert_eq!(git(Path::new("/")), Some(None));
        assert_eq!(git(&root.join("gone")), Some(None), "a directory gone");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sl04_it_is_read_back_as_written_and_garbage_starts_empty() {
        let dir = std::env::temp_dir().join(format!("gruntas-tasks-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut t = Tasks::default();
        t.apply(&reg("s"), T0);
        t.agent_state("s", TaskState::Working, T1);
        t.save(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(Tasks::load(&dir), t);
        std::fs::write(dir.join(FILE), b"{nope").unwrap();
        assert_eq!(Tasks::load(&dir), Tasks::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
