//! Automatic sync's policy (spec R-SYNC-3; engineering/harness.md → Sync →
//! Automatic sync): whether this device syncs its sessions without a
//! command, which directories opt out, how long a bridge may sit idle, and
//! which directory a session belongs to. Pure reads of the environment, the
//! person's own `config.json` and the files on disk — no network, nothing
//! spawned. The supervisor (Harness P4 tickets 94, 96) and `krowk sync
//! status` (ticket 95) call it.

use crate::protocol::LogBody;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a bridge may sit idle before it is let go, when neither
/// `KROWK_SYNC_IDLE` nor `sync.idleMinutes` says.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(10 * 60);

/// The keys `sync` takes. Any other is refused, so a misspelt opt-out
/// (`"Auto": false`, `"excludes"`) is never read as nothing said.
const KEYS: [&str; 3] = ["auto", "exclude", "idleMinutes"];

/// A window this long or longer is never: no supervisor runs for a
/// century, and a `Duration` near `u64::MAX` seconds would overflow an
/// `Instant` it is added to.
const NEVER_AFTER: u64 = 100 * 365 * 24 * 60 * 60;

const AUTO_KEY: &str = "sync.auto in config.json";
const AUTO_ENV: &str = "KROWK_SYNC_AUTO";

/// What the device has said about automatic sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Sessions are synced without a command.
    pub on: bool,
    /// What turned it off, for a person to read: `KROWK_SYNC_AUTO` or
    /// `sync.auto in config.json`. `None` when on.
    pub off_by: Option<&'static str>,
    /// `sync.exclude`: absolute directories whose sessions are never
    /// synced, nor any session under them.
    pub exclude: Vec<PathBuf>,
    /// How long a bridge may sit idle before it is let go; `None` is never.
    pub idle: Option<Duration>,
}

/// Why a directory's sessions are not synced, naming the source a person
/// can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptOut {
    /// Under this `sync.exclude` entry (as written in config.json).
    Excluded(PathBuf),
    /// This `.krowk/no-sync`, in the directory or one of its ancestors.
    Marker(PathBuf),
}

/// The policy from `KROWK_SYNC_AUTO` and `KROWK_SYNC_IDLE`, else the
/// `sync` object of `config` — which must be the person's own
/// `~/.krowk/config.json`, never a repository's: a repository can only
/// turn sync off (with `.krowk/no-sync`), never on. No config reads as `{}`:
/// on, nothing excluded, the default idle window.
///
/// A bad value is an `Err` naming the key and how to fix it. **An `Err`
/// means off**: the supervisor syncs nothing while the policy cannot be
/// read, so a typo never sends a session off the machine. Never read an
/// error as "on". Every value is checked, so a broken `sync.exclude` is
/// an `Err` even with `KROWK_SYNC_AUTO=0`: still off, named by the error.
pub fn policy(env: &dyn Fn(&str) -> String, config: Option<&Value>) -> Result<Policy, String> {
    let sync = match config {
        None => None,
        Some(Value::Object(c)) => match c.get("sync") {
            None => None,
            Some(Value::Object(o)) => Some(o),
            // `"sync": false` must not read as "absent, so on".
            Some(v) => return Err(format!("sync in config.json is {}, not an object — write it as {{\"auto\": false}} to turn automatic sync off, or remove it", kind(v))),
        },
        Some(v) => return Err(format!("config.json is {}, not an object — fix it, or remove it", kind(v))),
    };
    if let Some(k) = sync.and_then(|o| o.keys().find(|k| !KEYS.contains(&k.as_str()))) {
        return Err(format!("sync.{k} in config.json is not a setting — sync takes {}; fix the name, or remove it", KEYS.join(", ")));
    }
    let key = |k: &str| sync.and_then(|o| o.get(k));

    let (on, off_by) = match env(AUTO_ENV).trim() {
        "" => match key("auto") {
            None => (true, None),
            Some(Value::Bool(true)) => (true, None),
            Some(Value::Bool(false)) => (false, Some(AUTO_KEY)),
            Some(v) => return Err(format!("{AUTO_KEY} is {v}, not true or false — set it to false to turn automatic sync off, or remove it")),
        },
        s => match s.to_ascii_lowercase().as_str() {
            "0" | "false" | "off" => (false, Some(AUTO_ENV)),
            "1" | "true" | "on" => (true, None),
            _ => return Err(format!("{AUTO_ENV} is {s:?}, not a switch — set it to 0, false or off to turn automatic sync off, to 1, true or on to turn it on, or unset it")),
        },
    };

    let exclude = match key("exclude") {
        None => Vec::new(),
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|e| match e.as_str() {
                Some(s) if Path::new(s).is_absolute() => Ok(PathBuf::from(s)),
                Some(s) => Err(format!("sync.exclude in config.json has {s:?}, which is not an absolute path — write it in full, from / (not ~ or relative)")),
                None => Err(format!("sync.exclude in config.json has {e}, which is not a path — every entry is an absolute directory, as a string")),
            })
            .collect::<Result<_, _>>()?,
        Some(v) => return Err(format!("sync.exclude in config.json is {v}, not a list — write it as [\"/absolute/dir\", …], or remove it")),
    };

    let secs = match env("KROWK_SYNC_IDLE").trim() {
        "" => match key("idleMinutes") {
            None => DEFAULT_IDLE.as_secs(),
            // Under a second would round to 0, which is never: refused
            // rather than turned into the opposite of what it says (the
            // rule `daemon::idle_window` applies to host.idleMinutes).
            Some(v) => v.as_f64().filter(|m| *m == 0.0 || *m * 60.0 >= 1.0).map(|m| (m * 60.0).round() as u64).ok_or_else(|| format!("sync.idleMinutes in config.json is {v}, not a number of minutes of at least one second (0 never lets go) — fix it, or remove it for {} minutes", DEFAULT_IDLE.as_secs() / 60))?,
        },
        s => s.parse::<u64>().map_err(|_| format!("KROWK_SYNC_IDLE is {s:?}, not a number of seconds — set it to one (0 never lets go), or unset it"))?,
    };

    Ok(Policy { on, off_by, exclude, idle: (secs > 0 && secs < NEVER_AFTER).then(|| Duration::from_secs(secs)) })
}

impl Policy {
    /// Whether sessions in `cwd` are kept off sync, and by what: under a
    /// `sync.exclude` entry, matched by whole components (`/work/a` does
    /// not take `/work/ab`), or below a `.krowk/no-sync` — a file or a
    /// directory — in `cwd` or any ancestor. The `.krowk` that is krowk's
    /// own home (`krowk_home`) is not a repository's: a `no-sync` there
    /// opts out nothing (the device's switch is `sync.auto: false`).
    ///
    /// Paths are compared both as given and canonicalized (each falling
    /// back to itself when it cannot be), so a symlink on either side
    /// never lets a directory slip out of an opt-out. A marker that cannot
    /// be looked up for any reason but its absence (`EACCES` on a
    /// `.krowk`, say) counts as there. `cwd` is absolute, as `session_dir`
    /// gives it; a relative one would be read against this process's own
    /// directory. Blocking: a few `stat`s per ancestor.
    pub fn opted_out(&self, cwd: &Path, krowk_home: Option<&Path>) -> Option<OptOut> {
        debug_assert!(cwd.is_absolute(), "a session's directory is absolute: {}", cwd.display());
        let cwds = forms(cwd);
        for entry in &self.exclude {
            if forms(entry).iter().any(|e| cwds.iter().any(|c| c.starts_with(e))) {
                return Some(OptOut::Excluded(entry.clone()));
            }
        }
        let home = krowk_home.map(canonical);
        for c in &cwds {
            for dir in c.ancestors() {
                let dot = dir.join(".krowk");
                if home.as_ref().is_some_and(|h| canonical(&dot) == *h) {
                    continue;
                }
                let marker = dot.join("no-sync");
                match std::fs::symlink_metadata(&marker) {
                    // `.krowk` a file is no marker either.
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {}
                    _ => return Some(OptOut::Marker(marker)),
                }
            }
        }
        None
    }
}

/// What a JSON value is, for an error that should not repeat all of it.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The path as given and canonicalized, once when they are the same.
fn forms(p: &Path) -> Vec<PathBuf> {
    let c = canonical(p);
    if c == p { vec![c] } else { vec![c, p.to_path_buf()] }
}

/// A session's own directory, from its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDir {
    pub cwd: PathBuf,
    /// A subagent's session (its start names a parent).
    pub child: bool,
}

/// The directory session `id` runs in: the `cwd` of the `session.started`
/// its log in `sessions` begins with — never the directory of a client
/// that prompted it, since a resumed session keeps its own. `None` for an
/// id that is no session id, no log, a first line that does not parse, an
/// event of another kind or session, or a `cwd` that is not absolute.
/// **`None` is a directory unknown, so not synced**: a caller never reads
/// it as "not opted out".
///
/// Reads the first line only, so a `session.moved` later in the log (a
/// handoff, which syncs the session by a command of its own) is not
/// read here. Blocking: run it off any async runtime.
pub fn session_dir(sessions: &Path, id: &str) -> Option<SessionDir> {
    use std::io::BufRead;
    if !crate::log::valid_id(id) {
        return None;
    }
    let f = std::fs::File::open(sessions.join(id).join(crate::log::EVENTS_FILE)).ok()?;
    let first = std::io::BufReader::new(f).lines().next()?.ok()?;
    let e = serde_json::from_str::<crate::protocol::LogEvent>(&first).ok()?;
    match e.body {
        LogBody::SessionStarted { cwd, parent_session_id, .. } if e.session_id == id && Path::new(&cwd).is_absolute() => Some(SessionDir { cwd: PathBuf::from(cwd), child: parent_session_id.is_some() }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env<'a>(pairs: &'a [(&'a str, String)]) -> impl Fn(&str) -> String + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_default()
    }

    const NONE: [(&str, String); 0] = [];

    /// R-SYNC-3: on unless the device turns it off. R-SYNC-1: the policy is
    /// the environment and the person's own config, read with no account
    /// and nothing on the network; whether a paid account is there is the
    /// supervisor's question (ticket 94), not this one's.
    #[test]
    fn r_sync_3_the_switch_comes_from_the_env_then_config_and_is_on_by_default() {
        let p = policy(&env(&NONE), None).unwrap();
        assert_eq!((p.on, p.off_by), (true, None), "R-SYNC-3: synced without a command");
        assert!(policy(&env(&NONE), Some(&json!({}))).unwrap().on);
        assert!(policy(&env(&NONE), Some(&json!({"sync": {}}))).unwrap().on);
        assert!(policy(&env(&NONE), Some(&json!({"sync": {"auto": true}}))).unwrap().on);
        let off = json!({"sync": {"auto": false}});
        let p = policy(&env(&NONE), Some(&off)).unwrap();
        assert_eq!((p.on, p.off_by), (false, Some("sync.auto in config.json")));

        for s in ["0", "false", "off", "OFF", " False "] {
            let e = [("KROWK_SYNC_AUTO", s.to_string())];
            let p = policy(&env(&e), Some(&json!({"sync": {"auto": true}}))).unwrap();
            assert_eq!((p.on, p.off_by), (false, Some("KROWK_SYNC_AUTO")), "{s:?} turns it off, over config");
        }
        for s in ["1", "true", "on", "On"] {
            let e = [("KROWK_SYNC_AUTO", s.to_string())];
            let p = policy(&env(&e), Some(&off)).unwrap();
            assert_eq!((p.on, p.off_by), (true, None), "{s:?} turns it on, over config");
        }

        let e = [("KROWK_SYNC_AUTO", "maybe".to_string())];
        let err = policy(&env(&e), None).unwrap_err();
        assert!(err.contains("KROWK_SYNC_AUTO") && err.contains("\"maybe\"") && err.contains("off"), "{err}");
        for bad in [json!({"sync": {"auto": "no"}}), json!({"sync": {"auto": 0}}), json!({"sync": {"auto": null}})] {
            let err = policy(&env(&NONE), Some(&bad)).unwrap_err();
            assert!(err.contains("sync.auto in config.json"), "{err}");
        }
        // `"sync": false` is no object, and not read as absent (which is on).
        for bad in [json!({"sync": false}), json!({"sync": "off"}), json!({"sync": null})] {
            assert!(policy(&env(&NONE), Some(&bad)).unwrap_err().contains("sync in config.json"), "{bad}");
        }
        for bad in [json!(null), json!([]), json!(false)] {
            assert!(policy(&env(&NONE), Some(&bad)).unwrap_err().contains("config.json is"), "{bad}");
        }
        // A misspelt opt-out is refused, never read as nothing said.
        for (bad, named) in [(json!({"sync": {"Auto": false}}), "sync.Auto"), (json!({"sync": {"excludes": ["/w"]}}), "sync.excludes"), (json!({"sync": {"auto": false, "enabled": false}}), "sync.enabled")] {
            let err = policy(&env(&NONE), Some(&bad)).unwrap_err();
            assert!(err.contains(named) && err.contains("not a setting"), "{err}");
        }
        // Every value is checked, the switch's override too: still an Err, so off.
        let e = [("KROWK_SYNC_AUTO", "0".to_string())];
        assert!(policy(&env(&e), Some(&json!({"sync": {"exclude": "/w"}}))).is_err());
    }

    #[test]
    fn r_sync_3_the_exclude_list_takes_absolute_paths_only() {
        assert!(policy(&env(&NONE), None).unwrap().exclude.is_empty());
        let cfg = json!({"sync": {"exclude": ["/work/secret", "/home/me/client/"]}});
        assert_eq!(policy(&env(&NONE), Some(&cfg)).unwrap().exclude, vec![PathBuf::from("/work/secret"), PathBuf::from("/home/me/client/")]);
        for (entry, named) in [(json!("work/secret"), "\"work/secret\""), (json!("~/secret"), "\"~/secret\""), (json!(""), "\"\""), (json!(7), "7"), (json!({"path": "/x"}), "{\"path\":\"/x\"}")] {
            let cfg = json!({"sync": {"exclude": ["/ok", entry]}});
            let err = policy(&env(&NONE), Some(&cfg)).unwrap_err();
            assert!(err.contains("sync.exclude in config.json") && err.contains(named), "{err}");
        }
        for not_a_list in [json!("/work/secret"), json!({"/work": true}), json!(null)] {
            let err = policy(&env(&NONE), Some(&json!({"sync": {"exclude": not_a_list}}))).unwrap_err();
            assert!(err.contains("sync.exclude in config.json") && err.contains("not a list"), "{err}");
        }
    }

    #[test]
    fn r_sync_3_the_idle_window_comes_from_the_env_then_config() {
        assert_eq!(policy(&env(&NONE), None).unwrap().idle, Some(DEFAULT_IDLE));
        assert_eq!(DEFAULT_IDLE, Duration::from_secs(600));
        let cfg = json!({"sync": {"idleMinutes": 2}});
        assert_eq!(policy(&env(&NONE), Some(&cfg)).unwrap().idle, Some(Duration::from_secs(120)));
        let e = [("KROWK_SYNC_IDLE", "3".to_string())];
        assert_eq!(policy(&env(&e), Some(&cfg)).unwrap().idle, Some(Duration::from_secs(3)), "the env wins");
        let e = [("KROWK_SYNC_IDLE", "0".to_string())];
        assert_eq!(policy(&env(&e), Some(&cfg)).unwrap().idle, None, "0 never lets go");
        assert_eq!(policy(&env(&NONE), Some(&json!({"sync": {"idleMinutes": 0}}))).unwrap().idle, None);
        for tiny in [json!(0.005), json!(-1), json!("10")] {
            let err = policy(&env(&NONE), Some(&json!({"sync": {"idleMinutes": tiny}}))).unwrap_err();
            assert!(err.contains("sync.idleMinutes in config.json") && err.contains("at least one second"), "not rounded down to never: {err}");
        }
        let e = [("KROWK_SYNC_IDLE", u64::MAX.to_string())];
        assert_eq!(policy(&env(&e), None).unwrap().idle, None, "past a century is never, and no Instant overflows");
        assert_eq!(policy(&env(&NONE), Some(&json!({"sync": {"idleMinutes": 1e300}}))).unwrap().idle, None);
        for bad in ["soon", "-1", "1.5"] {
            let e = [("KROWK_SYNC_IDLE", bad.to_string())];
            assert!(policy(&env(&e), None).unwrap_err().contains("KROWK_SYNC_IDLE"), "{bad}");
        }
    }

    #[test]
    fn r_sync_3_a_directory_opts_out_by_the_list_or_a_marker_at_any_ancestor() {
        let root = std::env::temp_dir().join(format!("krowk-sync3-optout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root = {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::canonicalize(&root).unwrap()
        };
        let mk = |p: &str| {
            let d = root.join(p);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let (secret, deep, sibling) = (mk("work/a"), mk("work/a/src/x"), mk("work/ab"));
        let (repo_sub, high_sub) = (mk("repo/crates/core"), mk("mono/team/svc/src"));
        mk("repo/.krowk");
        std::fs::write(root.join("repo/.krowk/no-sync"), "").unwrap();
        mk("mono/.krowk/no-sync"); // a directory marker, two levels above the repository at svc
        mk("mono/team/svc/.krowk");
        // krowk's own home is `home/.krowk`; its no-sync covers nothing.
        let (krowk_home, in_home) = (mk("home/.krowk"), mk("home/code/proj"));
        mk("home/.krowk/no-sync");

        let p = Policy { on: true, off_by: None, exclude: vec![secret.clone()], idle: None };
        let home = Some(krowk_home.as_path());
        assert_eq!(p.opted_out(&secret, home), Some(OptOut::Excluded(secret.clone())));
        assert_eq!(p.opted_out(&deep, home), Some(OptOut::Excluded(secret.clone())), "its subdirectories too");
        assert_eq!(p.opted_out(&sibling, home), None, "whole components: /work/a does not take /work/ab");
        assert_eq!(p.opted_out(&root.join("repo"), home), Some(OptOut::Marker(root.join("repo/.krowk/no-sync"))));
        assert_eq!(p.opted_out(&repo_sub, home), Some(OptOut::Marker(root.join("repo/.krowk/no-sync"))), "from the repository's root");
        assert_eq!(p.opted_out(&high_sub, home), Some(OptOut::Marker(root.join("mono/.krowk/no-sync"))), "from two levels above it, as a directory");
        assert_eq!(p.opted_out(&in_home, home), None, "krowk's home is no repository");
        assert_eq!(p.opted_out(&root.join("home"), home), None);
        assert_eq!(p.opted_out(&in_home, None), Some(OptOut::Marker(root.join("home/.krowk/no-sync"))), "without the home given, it is just a .krowk");

        // A `.krowk` that is a file holds no marker; one that cannot be
        // looked into counts as one.
        let (plain, shut) = (mk("plain/src"), mk("shut/src"));
        std::fs::write(root.join("plain/.krowk"), "").unwrap();
        assert_eq!(p.opted_out(&plain, home), None);
        mk("shut/.krowk");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("shut/.krowk"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::symlink_metadata(root.join("shut/.krowk/no-sync")).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        let got = p.opted_out(&shut, home);
        std::fs::set_permissions(root.join("shut/.krowk"), std::fs::Permissions::from_mode(0o700)).unwrap();
        if unreadable {
            assert_eq!(got, Some(OptOut::Marker(root.join("shut/.krowk/no-sync"))), "fails closed");
        } // else root, which reads through any mode

        // A symlink on either side does not slip out.
        let link = root.join("link-to-a");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert_eq!(p.opted_out(&link.join("src"), home), Some(OptOut::Excluded(secret.clone())), "a cwd reached through a link");
        let by_link = Policy { exclude: vec![link.clone()], ..p.clone() };
        assert_eq!(by_link.opted_out(&deep, home), Some(OptOut::Excluded(link.clone())), "an entry that is a link");
        // The entry is named as written, even where it does not exist.
        let gone = Policy { exclude: vec![root.join("nowhere")], ..p.clone() };
        assert_eq!(gone.opted_out(&root.join("nowhere/x"), home), Some(OptOut::Excluded(root.join("nowhere"))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn r_sync_3_a_sessions_directory_is_its_logs_own() {
        let sessions = std::env::temp_dir().join(format!("krowk-sync3-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sessions);
        let (root, kid, garbage, other, foreign, relative, absent) = (
            "01a0ec7b-0000-7000-8000-0000000000b1",
            "01a0ec7b-0000-7000-8000-0000000000b2",
            "01a0ec7b-0000-7000-8000-0000000000b3",
            "01a0ec7b-0000-7000-8000-0000000000b4",
            "01a0ec7b-0000-7000-8000-0000000000b5",
            "01a0ec7b-0000-7000-8000-0000000000b6",
            "01a0ec7b-0000-7000-8000-0000000000b7",
        );
        let write = |id: &str, first: String| {
            std::fs::create_dir_all(sessions.join(id)).unwrap();
            std::fs::write(sessions.join(id).join(crate::log::EVENTS_FILE), format!("{first}\n{{\"later\": true}}\n")).unwrap();
        };
        let started = |id: &str, of: &str, cwd: &str, parent: Option<&str>| {
            let e = crate::protocol::LogEvent { id: id.into(), parent_id: None, session_id: of.into(), time_ms: 1, body: LogBody::SessionStarted { cwd: cwd.into(), krowk_version: "t".into(), protocol_version: 1, parent_session_id: parent.map(String::from), agent: None } };
            serde_json::to_string(&e).unwrap()
        };
        write(root, started(root, root, "/work/repo", None));
        write(kid, started(kid, kid, "/work/repo/sub", Some(root)));
        write(garbage, "not json".into());
        let ended = crate::protocol::LogEvent { id: other.into(), parent_id: None, session_id: other.into(), time_ms: 1, body: LogBody::TodosUpdated { turn_id: "t".into(), todos: Vec::new() } };
        write(other, serde_json::to_string(&ended).unwrap());
        write(foreign, started(foreign, root, "/elsewhere", None));
        write(relative, started(relative, relative, "repo", None));

        assert_eq!(session_dir(&sessions, root), Some(SessionDir { cwd: "/work/repo".into(), child: false }));
        assert_eq!(session_dir(&sessions, kid), Some(SessionDir { cwd: "/work/repo/sub".into(), child: true }));
        for (id, why) in [(absent, "no log"), (garbage, "a garbage first line"), (other, "another kind of event"), (foreign, "another session's start"), (relative, "a relative cwd"), ("../x", "an invalid id"), ("", "an empty id")] {
            assert_eq!(session_dir(&sessions, id), None, "{why}");
        }
        let _ = std::fs::remove_dir_all(&sessions);
    }
}
