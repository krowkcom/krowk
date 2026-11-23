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

use crate::protocol::{Activity, Registration};
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
                        self.sessions.remove(&k);
                    }
                }
                self.sessions.insert(r.session_id.clone(), r.clone());
                true
            }
            Activity::Deregister { session_id } => self.sessions.remove(session_id).is_some(),
            Activity::HeavyStart { .. } | Activity::HeavyEnd { .. } | Activity::Usage { .. } => false,
        }
    }

    /// Drops every entry whose harness process `alive` says is gone;
    /// whether any went. One that named no process stays until its session
    /// ends.
    pub fn prune(&mut self, alive: impl Fn(u32) -> bool) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|_, r| r.harness_pid.is_none_or(&alive));
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

    #[test]
    fn kt01_pid_zero_and_out_of_range_are_never_alive() {
        assert!(!alive(0));
        assert!(!alive(u32::MAX));
        assert!(alive(std::process::id()));
    }
}
