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

use crate::protocol::{Activity, PluginHarness, Registration};
use gruntas_client::presence::normalise_origin;
use gruntas_client::status::{Field, Health, Provenance, Task, TaskMetadata, TaskState, TaskStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const FILE: &str = "registrations.json";

/// More than any machine runs at once: a hook that registers without end
/// cannot grow the file past this, the first by id making room.
const MOST: usize = 1024;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    /// By session id.
    #[serde(default)]
    sessions: BTreeMap<String, Registration>,
    /// When each session was first registered, RFC 3339 by this machine's
    /// clock: its status's time until anything newer is known.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    registered_at: BTreeMap<String, String>,
}

impl Registry {
    /// The registry `dir` holds, its dead entries pruned; empty when there
    /// is none or it cannot be read.
    // blocking: once, as the daemon starts.
    pub fn load(dir: &Path) -> Registry {
        let path = dir.join(FILE);
        let mut r = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                eprintln!("{} cannot be read ({e}): starting with no registrations", path.display());
                Registry::default()
            }),
            Err(_) => Registry::default(),
        };
        r.prune(alive);
        // One kept before registrations were timed: from now.
        let now = jiff::Timestamp::now().to_string();
        for id in r.sessions.keys() {
            r.registered_at.entry(id.clone()).or_insert_with(|| now.clone());
        }
        r
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
            Activity::HeavyStart { .. } | Activity::HeavyEnd { .. } | Activity::TurnEnd { .. } | Activity::Usage { .. } => false,
        }
    }

    /// Drops a session; whether it was there.
    fn forget(&mut self, session_id: &str) -> bool {
        self.registered_at.remove(session_id);
        self.sessions.remove(session_id).is_some()
    }

    /// The status board's Tasks (D1, D8): one per session whose harness
    /// process is still there, by session id, with what registration
    /// observed (the harness, the project, health `SERVING`, and the
    /// registration as its last activity), all with harness provenance.
    /// A session's state is `working` until a report or a hook says
    /// otherwise. The machine is the publisher's to fill.
    pub fn board_tasks(&self) -> Vec<Task> {
        self.sessions()
            .map(|r| {
                let at = self.registered_at.get(&r.session_id).cloned().unwrap_or_else(|| jiff::Timestamp::UNIX_EPOCH.to_string());
                let mut t = Task { id: r.session_id.clone(), status: TaskStatus { state: TaskState::Working, message: None, timestamp: at.clone() }, history: Vec::new(), metadata: TaskMetadata::default() };
                let g = &mut t.metadata.gruntas;
                g.state_source = Some(Provenance::Harness);
                g.health = Some(Field::harness(Health::Serving));
                g.harness = Some(Field::harness(match r.harness {
                    PluginHarness::Claude => "claude".into(),
                    PluginHarness::Codex => "codex".into(),
                }));
                g.project = r.repository.as_deref().and_then(normalise_origin).map(Field::harness);
                g.last_activity = Some(Field::harness(at));
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

/// Writes `bytes` as `dir`'s registry: to a temporary file, then renamed
/// over, so a reader never sees half of one.
pub fn save_json(dir: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::rename(&tmp, dir.join(FILE))
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
    use crate::protocol::{PluginHarness, UsageWindow};

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
        r.apply(&Activity::Deregister { session_id: "a".into() });
        assert!(r.board_tasks().is_empty() && !r.registered_at.contains_key("a"));
    }

    #[test]
    fn kt01_pid_zero_and_out_of_range_are_never_alive() {
        assert!(!alive(0));
        assert!(!alive(u32::MAX));
        assert!(alive(std::process::id()));
    }
}
